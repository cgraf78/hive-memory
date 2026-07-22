//! Search CLI arguments, output models, and backend adapters.

use crate::{
    CliContext, StoreAccess, load_config, read_store_manifest, rebuild_store_index,
    resolve_agent_id, resolve_project_id, resolve_store,
};
use anyhow::Result;
use clap::Args;
use hive_memory::config::Config;
use hive_memory::{config, index, note, retrieval, search};
use serde::Serialize;
use std::path::Path;
use time::OffsetDateTime;

/// Arguments for `hm search`.
#[derive(Debug, Args)]
pub(crate) struct SearchArgs {
    /// Case-insensitive substring query.
    query: String,
    /// Maximum hits to show.
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Only search indexed records created within this age (for example 30m, 2h, 1d) or today.
    #[arg(long)]
    since: Option<String>,
    /// Include lower-confidence raw `hm note` entries.
    #[arg(long)]
    include_inbox: bool,
    /// Optional comma-separated scope filter.
    #[arg(long, value_delimiter = ',')]
    scope: Vec<String>,
    /// Optional comma-separated source filter.
    #[arg(long, value_delimiter = ',')]
    source: Vec<String>,
    /// Active project id for project-scoped memory.
    #[arg(long)]
    project_id: Option<String>,
    /// Active project path or file hint.
    #[arg(long)]
    project: Option<String>,
    /// Restrict results to memory owned by the active project.
    #[arg(long)]
    project_only: bool,
    /// Include structured scoring diagnostics.
    #[arg(long)]
    explain: bool,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

impl SearchArgs {
    /// Return whether this invocation requires structured error output.
    pub(crate) fn wants_json(&self) -> bool {
        self.json
    }
}

pub(crate) fn run(args: SearchArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent);
    let project_id = resolve_project_id(args.project_id, args.project.as_deref())?;
    if args.project_only && project_id.is_none() {
        anyhow::bail!("--project-only requires --project or --project-id");
    }
    let resolved_store = resolve_store(
        &config,
        context.store.as_deref(),
        None,
        agent_id.as_deref(),
        StoreAccess::Read,
    )?;
    let store_config = &config.stores[resolved_store.name.as_str()];
    let manifest = read_store_manifest(&config, &resolved_store.name, store_config)?;
    let report = rebuild_store_index(&config, &resolved_store.name)?;
    let since = args.since.as_deref().map(search_since_cutoff).transpose()?;

    let scopes = if args.scope.is_empty() {
        config.defaults.search_scopes.clone()
    } else {
        args.scope
    };
    let sources = if args.source.is_empty() {
        config.defaults.search_sources.clone()
    } else {
        args.source
    };
    let include_inbox = search_include_inbox(args.include_inbox, &sources);

    let filtered_entries;
    let entries = if let Some(cutoff) = since {
        filtered_entries = report
            .entries
            .iter()
            .filter(|entry| entry_created_at_is_since(entry, cutoff))
            .cloned()
            .collect::<Vec<_>>();
        filtered_entries.as_slice()
    } else {
        report.entries.as_slice()
    };

    let search_input = search::SearchInput {
        store_root: &store_config.root,
        entries,
        query: &args.query,
        scopes: &scopes,
        sources: &sources,
        include_inbox,
        agent_id: agent_id.as_deref(),
        project_id: project_id.as_deref(),
        limit: args.limit,
    };
    let hits = run_search_backend(
        &config,
        &resolved_store.name,
        &store_config.root,
        search_input,
        args.project_only,
    )?;

    if args.json {
        let output = hits
            .iter()
            .map(|hit| search_json_hit(&resolved_store.name, &manifest.store.id, hit, args.explain))
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!("store: {}", resolved_store.name);
    println!("scopes: {}", display_filter_values(&scopes));
    println!("sources: {}", display_filter_values(&sources));
    println!(
        "inbox: {}",
        if include_inbox {
            "included"
        } else {
            "excluded (use --include-inbox or --source inbox)"
        }
    );
    if let Some(since) = args.since.as_deref() {
        println!("since: {since}");
    }
    println!("hits: {}", hits.len());
    for hit in hits {
        println!("id: {}", hit.entry.id);
        println!("score: {}", hit.score);
        if args.explain {
            print_score_trace(&hit.trace);
        }
        println!("note: {}", hit.entry.note_path);
        println!("snippet: {}", hit.snippet);
    }
    Ok(())
}

/// Run `hm search` through the configured backend. The Tantivy backend raises
/// recall on paraphrased queries; on any retrieval failure it falls back to the
/// lexical scan with a warning so a degraded index never strips results.
fn run_search_backend(
    config: &Config,
    store_name: &str,
    store_root: &Path,
    input: search::SearchInput<'_>,
    project_only: bool,
) -> Result<Vec<search::SearchHit>> {
    if config
        .defaults
        .search_backend
        .trim()
        .eq_ignore_ascii_case("tantivy")
    {
        match tantivy_search(config, store_name, store_root, input.clone(), project_only) {
            Ok(hits) => return Ok(hits),
            Err(err) => {
                eprintln!(
                    "warning: full-text search backend unavailable ({err}); using lexical search"
                );
            }
        }
    }
    if project_only {
        Ok(search::search_project_only(input)?)
    } else {
        Ok(search::search(input)?)
    }
}

/// Open (or create) the store's persistent Tantivy index, refresh it from the
/// current entries when their fingerprint changed, and run a policy-filtered
/// BM25 search. The index lives under the disposable cache dir, keyed by store.
///
/// The rebuild branch takes the same cache-key `RebuildLock` that
/// `cli::sync::perform` holds, so an interactive `hm search` and a concurrent
/// `hm refresh` cannot fight over the shared cache artifact. On contention we
/// degrade to read-only/lexical rather than block this latency-sensitive read: a
/// stale-but-valid index searches fine, and the rebuild the other holder is
/// running will land for the next query.
/// (Tantivy's own writer lock already prevents corruption; this is about honoring
/// the documented single-writer contract and avoiding redundant rebuild scans.)
fn tantivy_search(
    config: &Config,
    store_name: &str,
    store_root: &Path,
    input: search::SearchInput<'_>,
    project_only: bool,
) -> std::result::Result<Vec<search::SearchHit>, search::SearchError> {
    let dir = config.cache_dir.join("search").join(store_name);
    let index = retrieval::SearchIndex::open_or_create_in_dir(&dir)
        .map_err(|err| search::SearchError::Retrieval(err.to_string()))?;
    let fingerprint = search::entries_fingerprint(input.entries);
    if !index.is_fresh(&fingerprint) {
        // Serialize the rebuild against refresh/other interactive rebuilds via the
        // cache-key lock. `Ok(Some(lock))` lets us rebuild while holding it;
        // `Ok(None)` means another rebuild already holds it, and a lock I/O error
        // means we cannot coordinate — in both of those cases skip our rebuild and
        // search the current index read-only instead of blocking this
        // latency-sensitive read. The lock guard must outlive the rebuild, so it
        // is bound for the whole branch.
        match index::try_rebuild_lock(&config.cache_dir, store_name, store_root) {
            Ok(Some(_rebuild_lock)) => {
                let documents = search::search_documents(store_root, input.entries)?;
                index
                    .rebuild_tagged(&documents, Some(&fingerprint))
                    .map_err(|err| search::SearchError::Retrieval(err.to_string()))?;
            }
            // Rebuild skipped (another holder has the lock, or we cannot coordinate).
            // `open_or_create_in_dir` will have just created an EMPTY index when none
            // existed, so serving it now would silently return zero hits and strip
            // results on the primary recall path. A populated-but-stale index is fine
            // to serve (the other holder's rebuild lands for the next query), but a
            // never-built/empty index must NOT short-circuit the lexical fallback.
            // The manifest is written only after a successful rebuild commit, so its
            // absence distinguishes "never built / empty" from "stale but populated".
            _ if index.manifest().is_none() => {
                return Err(search::SearchError::Retrieval(
                    "full-text index not yet populated and a concurrent rebuild holds the cache lock"
                        .to_owned(),
                ));
            }
            _ => {}
        }
    }
    if project_only {
        search::search_indexed_project_only(input, &index)
    } else {
        search::search_indexed(input, &index)
    }
}

/// Hook-safe BM25 search: query the persistent index ONLY when it is already
/// fresh for `input.entries`. Never rebuilds — the prompt-submit hook must not
/// pay for a full index rebuild on its latency budget. Returns `None` (so the
/// caller falls back to lexical) when the backend is off, the index is
/// stale/absent, or the engine errors. The index is kept fresh out of band by
/// `hm refresh` (tool-complete) and interactive `hm search`.
pub(crate) fn tantivy_search_if_fresh(
    config: &Config,
    store_name: &str,
    input: search::SearchInput<'_>,
) -> Option<Vec<search::SearchHit>> {
    if !config
        .defaults
        .search_backend
        .trim()
        .eq_ignore_ascii_case("tantivy")
    {
        return None;
    }
    let dir = config.cache_dir.join("search").join(store_name);
    // Read-only open: never create or rebuild on the hook's hot path. A missing
    // or stale index yields None so the caller uses lexical; refresh rebuilds it.
    let index = retrieval::SearchIndex::open_existing_in_dir(&dir)
        .ok()
        .flatten()?;
    if !index.is_fresh(&search::entries_fingerprint(input.entries)) {
        return None;
    }
    search::search_indexed(input, &index).ok()
}

/// Rebuild the store's persistent Tantivy index from `entries` when the backend
/// is enabled and the index is stale. Called off the hot path (refresh /
/// tool-complete) so the prompt hook can query a fresh index cheaply.
pub(crate) fn refresh_tantivy_index(
    config: &Config,
    store_name: &str,
    store_root: &Path,
    entries: &[index::IndexEntry],
) {
    if !config
        .defaults
        .search_backend
        .trim()
        .eq_ignore_ascii_case("tantivy")
    {
        return;
    }
    let dir = config.cache_dir.join("search").join(store_name);
    let result = retrieval::SearchIndex::open_or_create_in_dir(&dir).and_then(|index| {
        let fingerprint = search::entries_fingerprint(entries);
        if index.is_fresh(&fingerprint) {
            return Ok(());
        }
        let documents = search::search_documents(store_root, entries)
            .map_err(|err| retrieval::RetrievalError::Engine(err.to_string()))?;
        index
            .rebuild_tagged(&documents, Some(&fingerprint))
            .map(|_| ())
    });
    if let Err(err) = result {
        // Best-effort: a failed cache refresh must never break the write/refresh
        // flow. The next interactive search rebuilds, and recall falls back to
        // lexical meanwhile.
        eprintln!("warning: full-text index refresh skipped: {err}");
    }
}

#[derive(Debug, Serialize)]
struct SearchJsonHit {
    id: String,
    store: String,
    store_id: String,
    scope: String,
    project_id: Option<String>,
    trust: &'static str,
    audience: Vec<String>,
    path: String,
    title: String,
    snippet: String,
    score: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    score_trace: Option<SearchJsonScoreTrace>,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct SearchJsonScoreTrace {
    retrieval: usize,
    body_phrase: usize,
    body_terms: usize,
    metadata_phrase: usize,
    metadata_terms: usize,
    combined_phrase: usize,
    combined_terms: usize,
    entity: usize,
    project: usize,
    total: usize,
}

fn search_json_hit(
    store_name: &str,
    manifest_store_id: &str,
    hit: &search::SearchHit,
    explain: bool,
) -> SearchJsonHit {
    let entry = &hit.entry;
    SearchJsonHit {
        id: entry.id.clone(),
        store: store_name.to_owned(),
        store_id: if entry.store_id.is_empty() {
            manifest_store_id.to_owned()
        } else {
            entry.store_id.clone()
        },
        scope: entry.scope.clone(),
        project_id: entry.project_id.clone(),
        trust: search_trust(entry),
        audience: entry.audience.clone(),
        path: entry.note_path.clone(),
        title: entry
            .subject
            .clone()
            .unwrap_or_else(|| entry.note_path.clone()),
        snippet: hit.snippet.clone(),
        score: hit.score,
        score_trace: explain.then(|| search_json_score_trace(&hit.trace)),
        created_at: entry.created_at.clone(),
    }
}

fn search_json_score_trace(trace: &search::SearchScoreTrace) -> SearchJsonScoreTrace {
    SearchJsonScoreTrace {
        retrieval: trace.retrieval,
        body_phrase: trace.body_phrase,
        body_terms: trace.body_terms,
        metadata_phrase: trace.metadata_phrase,
        metadata_terms: trace.metadata_terms,
        combined_phrase: trace.combined_phrase,
        combined_terms: trace.combined_terms,
        entity: trace.entity,
        project: trace.project,
        total: trace.total(),
    }
}

fn print_score_trace(trace: &search::SearchScoreTrace) {
    println!(
        "score_trace: retrieval={} body_phrase={} body_terms={} metadata_phrase={} metadata_terms={} combined_phrase={} combined_terms={} entity={} project={} total={}",
        trace.retrieval,
        trace.body_phrase,
        trace.body_terms,
        trace.metadata_phrase,
        trace.metadata_terms,
        trace.combined_phrase,
        trace.combined_terms,
        trace.entity,
        trace.project,
        trace.total()
    );
}

fn display_filter_values(values: &[String]) -> String {
    if values.is_empty() {
        "(none)".to_owned()
    } else {
        values.join(",")
    }
}

pub(crate) fn search_include_inbox(include_inbox: bool, sources: &[String]) -> bool {
    include_inbox || source_filter_includes_inbox(sources)
}

/// Source filters are machine policy, not display text: `inbox` and `all`
/// both grant access to raw notes everywhere search policy is applied.
fn source_filter_includes_inbox(sources: &[String]) -> bool {
    sources
        .iter()
        .any(|source| source == "inbox" || source == "all")
}

pub(crate) fn source_filter_includes_curated(sources: &[String]) -> bool {
    sources
        .iter()
        .any(|source| source == "curated" || source == "all")
}

fn search_since_cutoff(value: &str) -> Result<OffsetDateTime> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("today") {
        return Ok(OffsetDateTime::now_utc().date().midnight().assume_utc());
    }
    if let Some(duration) = config::parse_duration_time(trimmed) {
        return Ok(OffsetDateTime::now_utc() - duration);
    }
    OffsetDateTime::parse(trimmed, &time::format_description::well_known::Rfc3339).map_err(|err| {
        anyhow::anyhow!("--since must be today, a duration like 30m/2h/1d, or RFC3339: {err}")
    })
}

fn entry_created_at_is_since(entry: &index::IndexEntry, cutoff: OffsetDateTime) -> bool {
    OffsetDateTime::parse(
        &entry.created_at,
        &time::format_description::well_known::Rfc3339,
    )
    .is_ok_and(|created_at| created_at >= cutoff)
}

fn search_trust(entry: &index::IndexEntry) -> &'static str {
    if entry.id.starts_with("curated:") {
        return "curated";
    }
    match entry.entry_kind {
        note::EntryKind::Remember => "remembered",
        note::EntryKind::Note => "raw",
    }
}
