//! Context CLI assembly, cache, output models, and selection identity.

use crate::{
    CliContext, StoreAccess, context_session_id, hook_active, hook_options, load_config,
    project_binding_store, rebuild_store_index, resolve_agent_id, resolve_project_id,
    resolve_store,
};
use anyhow::Result;
use clap::Args;
use hive_memory::config::Config;
use hive_memory::{
    config, context as memory_context, hook as memory_hook, index, inject, path as memory_path,
    write,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use time::OffsetDateTime;

const MAX_CONTEXT_CACHE_BYTES: u64 = 16 * 1024 * 1024;

/// Arguments for `hm context`.
#[derive(Debug, Args)]
pub(crate) struct ContextArgs {
    /// Maximum approximate tokens to emit.
    #[arg(long)]
    max_tokens: Option<usize>,
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
    /// Active path hint to display in context headers.
    #[arg(long)]
    path: Option<String>,
    /// Suppress output when this session already saw the same context selection.
    #[arg(long)]
    if_changed: bool,
    /// Include candidate-level selection decisions in JSON output.
    #[arg(long)]
    explain: bool,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

impl ContextArgs {
    /// Return whether this invocation requires structured error output.
    pub(crate) fn wants_json(&self) -> bool {
        self.json
    }
}

pub(crate) fn run(args: ContextArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let path_hint = args.project.or(args.path);
    let project_id = resolve_project_id(args.project_id, path_hint.as_deref())?;
    let include_search_only = args.include_inbox
        || args
            .source
            .iter()
            .any(|source| source == "inbox" || source == "all");
    let assembly = assemble_cli_context(
        &config,
        &context,
        ContextSelection {
            max_tokens: args.max_tokens,
            include_inbox: args.include_inbox,
            include_search_only,
            explain: args.explain,
            scopes: args.scope,
            sources: args.source,
            project_id,
            path_hint,
        },
    )?;

    // Without a session id there is no durable cursor to compare against.
    // Treat that as "changed" and emit fresh context instead of making one-off
    // CLI/debug calls fail because they are outside a managed agent session.
    if args.if_changed
        && let Some(session_id) = context_session_id()
    {
        let context_key = context_selection_key_from_assembly(&config, &assembly);
        let state = memory_hook::load_state(&config.state_dir, &session_id)?;
        if state.context_key.as_deref() == Some(context_key.as_str()) {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&context_json_suppressed(assembly, false, None))?
                );
            }
            return Ok(());
        }
        memory_hook::mark_context_key(
            &config.state_dir,
            &session_id,
            context_key,
            &hook_options(&config),
        )?;
    }

    if args.json {
        let stale = assembly.stale;
        let cache_created_at = assembly.cache_created_at.clone();
        println!(
            "{}",
            serde_json::to_string_pretty(&context_json(assembly, true, stale, cache_created_at))?
        );
    } else {
        print!("{}", assembly.output.markdown);
    }
    Ok(())
}

pub(crate) struct ContextSelection {
    /// Explicit token budget. Missing means command-mode or hook-mode defaults.
    pub(crate) max_tokens: Option<usize>,
    /// Explicitly opt into lower-confidence raw inbox notes.
    pub(crate) include_inbox: bool,
    /// Explicitly render records the relevance strategy classifies as search-only.
    pub(crate) include_search_only: bool,
    /// Capture candidate-level selection decisions for JSON debugging.
    pub(crate) explain: bool,
    /// Scope filter from CLI/hook policy. Empty defers to config defaults.
    pub(crate) scopes: Vec<String>,
    /// Source filter from CLI/hook policy. Empty defers to config defaults.
    pub(crate) sources: Vec<String>,
    /// Project identity override. Missing can still resolve from env.
    pub(crate) project_id: Option<String>,
    /// Human path/project hint to render in the context header.
    pub(crate) path_hint: Option<String>,
}

pub(crate) struct CliContextAssembly {
    pub(crate) output: memory_context::ContextOutput,
    agent_id: Option<String>,
    project_id: Option<String>,
    project_hint: Option<String>,
    pub(crate) stores: Vec<String>,
    store_source: String,
    scopes: Vec<String>,
    sources: Vec<String>,
    /// Whether raw inbox records were eligible for this assembly.
    include_inbox: bool,
    /// Whether search-only records were intentionally rendered.
    include_search_only: bool,
    /// Resolved selection strategy label, part of the cache key.
    strategy: String,
    pub(crate) stale: bool,
    cache_created_at: Option<String>,
}

/// Assemble context for CLI commands and hook entry points.
///
/// This is intentionally the single in-binary adapter over the library context
/// API. Command parsing, env fallback, store affinity, and cache rebuilding are
/// CLI concerns; once those are resolved, hooks and `hm context` should feed the
/// same `ContextInput` shape so privacy/source/scope behavior cannot drift.
pub(crate) fn assemble_cli_context(
    config: &Config,
    context: &CliContext,
    selection: ContextSelection,
) -> Result<CliContextAssembly> {
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let scopes = if selection.scopes.is_empty() {
        config.defaults.search_scopes.clone()
    } else {
        selection.scopes
    };
    let sources = if selection.sources.is_empty() {
        config.defaults.context_sources.clone()
    } else {
        selection.sources
    };
    let include_inbox = selection.include_inbox
        || sources
            .iter()
            .any(|source| source == "inbox" || source == "all");
    let include_search_only = selection.include_search_only && include_inbox;
    // Resolve the selection strategy once; it feeds both the assembly and the
    // cache key so a strategy change invalidates any cached context.
    let strategy_label = config.defaults.context_strategy.clone();
    let inject_strategy = inject::Strategy::from_config(&strategy_label);
    let path_hint = selection.path_hint.or_else(|| {
        selection
            .project_id
            .is_none()
            .then(|| std::env::var("HIVE_MEMORY_PROJECT").ok())
            .flatten()
    });
    // Hooks often know an active buffer or tool path but not a precomputed
    // project id. Resolve here so hook adapters can stay policy-light while
    // still benefiting from project-scoped memory and local store bindings.
    let project_id = resolve_project_id(selection.project_id, path_hint.as_deref())?;
    let project_binding = project_binding_store(config, project_id.as_deref())?;
    let resolved_store = resolve_store(
        config,
        context.store.as_deref(),
        project_binding.as_deref(),
        agent_id.as_deref(),
        StoreAccess::Read,
    )?;
    let store_name = resolved_store.name.clone();
    let store_source = resolved_store.source.to_string();
    let store_config = &config.stores[resolved_store.name.as_str()];
    let stores = vec![store_name.clone()];
    let store_keys = vec![index::store_cache_key(&store_name, &store_config.root)];
    let context_key = context_selection_key(
        agent_id.as_deref().unwrap_or("unknown"),
        &store_keys,
        project_id.as_deref(),
        path_hint.as_deref(),
        &scopes,
        &sources,
        ContextKeyPolicy {
            include_inbox,
            include_search_only,
            strategy: &strategy_label,
        },
    );
    let cached_index_path =
        index::scoped_index_path(&config.cache_dir, &store_name, &store_config.root);
    let hook_active = hook_active(context);
    if hook_active
        && let Some(assembly) = load_context_cache(
            config,
            &context_key,
            store_source.clone(),
            path_hint.as_deref(),
            &cached_index_path,
        )?
    {
        // Lifecycle hooks have a hard latency budget. A canonical store can be
        // a reachable but cold cloud mount, so even a freshness stat may block
        // past that budget. Serve the complete policy-scoped local snapshot;
        // canonical refresh belongs outside the synchronous hook path.
        return Ok(assembly);
    }
    if hook_active
        && let Some(report) = index::load_cached_index(&index::LoadIndexInput {
            store_name: &store_name,
            store_root: &store_config.root,
            cache_dir: &config.cache_dir,
            options: write::AtomicWriteOptions {
                fsync: config.storage.fsync.into(),
                ..write::AtomicWriteOptions::default()
            },
            // Cache-only loads never serialize paths, so path_case is unused.
            // Avoid resolve_case: auto mode probes the canonical mount.
            path_case: memory_path::PathCase::Sensitive,
        })?
    {
        let max_tokens = selection.max_tokens.unwrap_or_else(|| {
            usize::try_from(config.defaults.hook_context_max_tokens)
                .expect("hook context token budget fits usize")
        });
        let mut output = memory_context::assemble_local_index_context(
            memory_context::ContextInput {
                store_name: store_name.as_str(),
                store_root: &store_config.root,
                entries: &report.projection.entries,
                scopes: &scopes,
                sources: &sources,
                include_inbox,
                include_search_only,
                agent_id: agent_id.as_deref(),
                project_id: project_id.as_deref(),
                path_hint: path_hint.as_deref(),
                max_tokens,
                inject_strategy,
                explain: selection.explain,
            },
            &report.projection.project_aliases,
        )?;
        let cache_created_at = std::fs::metadata(&report.path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(OffsetDateTime::from)
            .and_then(|created_at| {
                created_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .ok()
            });
        let age = cache_created_at.as_deref().unwrap_or("unknown time");
        let snapshot_label = if report.projection.complete {
            "complete local index snapshot"
        } else {
            "best-available incomplete local index snapshot"
        };
        output.markdown = format!(
            "> Hive Memory context is a {snapshot_label} from {age}; canonical refresh is asynchronous.\n\n{}",
            output.markdown
        );
        let assembly = CliContextAssembly {
            output,
            agent_id,
            project_id,
            project_hint: path_hint,
            stores,
            store_source,
            scopes,
            sources,
            include_inbox,
            include_search_only,
            strategy: strategy_label,
            stale: true,
            cache_created_at,
        };
        if let Err(err) = write_context_cache(config, &assembly) {
            eprintln!("warning: failed to write local context cache: {err}");
        }
        return Ok(assembly);
    }
    if hook_active {
        // Lifecycle hooks must never bootstrap from canonical storage. A cold
        // or corrupt local cache returns a bounded empty view immediately;
        // the caller schedules detached projection hydration after responding.
        let aliases = std::collections::BTreeMap::new();
        let mut output = memory_context::assemble_local_index_context(
            memory_context::ContextInput {
                store_name: store_name.as_str(),
                store_root: &store_config.root,
                entries: &[],
                scopes: &scopes,
                sources: &sources,
                include_inbox,
                include_search_only,
                agent_id: agent_id.as_deref(),
                project_id: project_id.as_deref(),
                path_hint: path_hint.as_deref(),
                max_tokens: selection.max_tokens.unwrap_or_else(|| {
                    usize::try_from(config.defaults.hook_context_max_tokens)
                        .expect("hook context token budget fits usize")
                }),
                inject_strategy,
                explain: selection.explain,
            },
            &aliases,
        )?;
        output.markdown = format!(
            "> Hive Memory has no local projection yet; canonical hydration is asynchronous.\n\n{}",
            output.markdown
        );
        return Ok(CliContextAssembly {
            output,
            agent_id,
            project_id,
            project_hint: path_hint,
            stores,
            store_source,
            scopes,
            sources,
            include_inbox,
            include_search_only,
            strategy: strategy_label,
            stale: true,
            cache_created_at: None,
        });
    }
    // Hook mode returned above. Interactive `hm context` keeps the larger v1
    // default for inspection and manual debugging.
    let max_tokens = selection.max_tokens.unwrap_or(4000);

    // Interactive canonical reads require a reachable, valid store. This also
    // prevents an unavailable root from being mistaken for a legitimate empty
    // store and replacing the last local projection.
    crate::read_store_manifest(config, &resolved_store.name, store_config)?;

    let output = rebuild_store_index(config, &resolved_store.name).and_then(|report| {
        memory_context::assemble_context(memory_context::ContextInput {
            store_name: store_name.as_str(),
            store_root: &store_config.root,
            entries: &report.entries,
            scopes: &scopes,
            sources: &sources,
            include_inbox,
            include_search_only,
            agent_id: agent_id.as_deref(),
            project_id: project_id.as_deref(),
            path_hint: path_hint.as_deref(),
            max_tokens,
            inject_strategy,
            explain: selection.explain,
        })
        .map_err(anyhow::Error::from)
    })?;
    // Per-record degradations are non-fatal by design; surface them on stderr
    // so sync damage is visible without stripping memory from the session.
    for warning in &output.warnings {
        eprintln!(
            "warning: context skipped {}: {}",
            warning.source_path, warning.message
        );
    }

    let assembly = CliContextAssembly {
        output,
        agent_id,
        project_id,
        project_hint: path_hint,
        stores,
        store_source,
        scopes,
        sources,
        include_inbox,
        include_search_only,
        strategy: strategy_label,
        stale: false,
        cache_created_at: None,
    };
    if let Err(err) = write_context_cache(config, &assembly) {
        // Fresh context is still correct even if the operational fallback cache
        // cannot be updated. Warn rather than failing agent startup.
        eprintln!("warning: failed to write context cache: {err}");
    }
    Ok(assembly)
}

#[derive(Debug, Serialize)]
struct ContextJsonOutput {
    /// Active agent id, when one was supplied through CLI/env.
    agent_id: Option<String>,
    /// Resolved project id, when project context was supplied.
    project_id: Option<String>,
    /// Original project/path hint used for resolution and header display.
    project_hint: Option<String>,
    /// Selected store aliases.
    stores: Vec<String>,
    /// Source of store selection:
    /// cli, env, project-binding, agent-default, or global-default.
    store_source: String,
    /// Scope filter actually used for this assembly.
    scopes: Vec<String>,
    /// Source filter actually used for this assembly.
    sources: Vec<String>,
    /// Whether lower-confidence raw inbox notes were eligible.
    include_inbox: bool,
    /// Whether search-only records were intentionally rendered.
    include_search_only: bool,
    /// Approximate token count for the emitted Markdown.
    estimated_tokens: usize,
    /// False only when `--if-changed` suppresses unchanged context.
    emitted: bool,
    /// True only for last-success cache fallback output.
    stale: bool,
    /// Creation timestamp for stale cache fallback output.
    cache_created_at: Option<String>,
    /// Included memory sections after filtering and budgeting.
    sections: Vec<ContextSectionJson>,
    /// Candidate-level include/skip reasons, present when `--explain` was used.
    decisions: Vec<ContextDecisionJson>,
}

#[derive(Debug, Serialize)]
struct ContextSectionJson {
    /// Memory id.
    id: String,
    /// Store alias that supplied this section.
    store: String,
    /// Memory scope used for filtering.
    scope: String,
    /// Owning project identity for project-scoped memory.
    project_id: Option<String>,
    /// Trust label: curated, remembered, or raw.
    trust: &'static str,
    /// Explicit agent audience for agent-private memory.
    audience: Vec<String>,
    /// Store-relative source path.
    source_path: String,
    /// Approximate tokens consumed by this section.
    estimated_tokens: usize,
    /// Safe-to-inject body rendered for this context section.
    body: String,
}

#[derive(Debug, Serialize)]
struct ContextDecisionJson {
    id: String,
    source_path: String,
    action: &'static str,
    reason: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContextCacheEntry {
    /// Cache schema for rejecting future incompatible entries.
    schema_version: u32,
    /// RFC3339 write time used for max-age policy.
    created_at: String,
    /// Local index generation rendered into this snapshot.
    #[serde(default)]
    index_generation: Option<u128>,
    /// Full context selection key that produced this entry.
    key: String,
    /// Exact Markdown injected during the successful fresh assembly.
    markdown: String,
    /// Agent identity used for audience filtering.
    agent_id: Option<String>,
    /// Project identity used for project-scoped filtering.
    project_id: Option<String>,
    /// Original path/project hint rendered into the context header.
    project_hint: Option<String>,
    /// Selected store aliases.
    stores: Vec<String>,
    /// Store selection source rendered in JSON output.
    store_source: String,
    /// Scope filter used for this assembly.
    scopes: Vec<String>,
    /// Source filter used for this assembly.
    sources: Vec<String>,
    /// Whether lower-confidence raw inbox notes were eligible.
    #[serde(default)]
    include_inbox: bool,
    /// Whether search-only records were intentionally rendered.
    #[serde(default)]
    include_search_only: bool,
    /// Token estimate from the fresh assembly.
    estimated_tokens: usize,
    /// Section metadata kept so stale JSON output preserves data boundaries.
    sections: Vec<ContextCacheSection>,
    /// Candidate decisions captured with the fresh assembly.
    #[serde(default)]
    decisions: Vec<ContextCacheDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContextCacheSection {
    id: String,
    store: String,
    scope: String,
    #[serde(default)]
    project_id: Option<String>,
    trust: String,
    audience: Vec<String>,
    source_path: String,
    estimated_tokens: usize,
    body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContextCacheDecision {
    id: String,
    source_path: String,
    action: String,
    reason: String,
}

fn write_context_cache(config: &Config, assembly: &CliContextAssembly) -> Result<PathBuf> {
    // The cache is an operational fallback for unavailable stores, not a second
    // memory source. Keep the full rendered Markdown plus section metadata so a
    // later stale response can preserve the same data-boundary labeling without
    // touching the store root.
    let key = context_selection_key_from_assembly(config, assembly);
    let path = context_cache_path(&config.state_dir, &key);
    let index_generation = assembly
        .stores
        .first()
        .and_then(|store_name| config.stores.get(store_name))
        .and_then(|store| {
            index_generation(&index::scoped_index_path(
                &config.cache_dir,
                assembly.stores.first().expect("selected store exists"),
                &store.root,
            ))
        });
    let entry = ContextCacheEntry {
        schema_version: 1,
        created_at: OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("RFC3339 formatting should not fail"),
        index_generation,
        key,
        markdown: assembly.output.markdown.clone(),
        agent_id: assembly.agent_id.clone(),
        project_id: assembly.project_id.clone(),
        project_hint: assembly.project_hint.clone(),
        stores: assembly.stores.clone(),
        store_source: assembly.store_source.clone(),
        scopes: assembly.scopes.clone(),
        sources: assembly.sources.clone(),
        include_inbox: assembly.include_inbox,
        include_search_only: assembly.include_search_only,
        estimated_tokens: assembly.output.estimated_tokens,
        sections: assembly
            .output
            .sections
            .iter()
            .map(|section| ContextCacheSection {
                id: section.id.clone(),
                store: section.store.clone(),
                scope: section.scope.clone(),
                project_id: section.project_id.clone(),
                trust: section.trust.as_str().to_owned(),
                audience: section.audience.clone(),
                source_path: section.source_path.clone(),
                estimated_tokens: section.estimated_tokens,
                body: section.body.clone(),
            })
            .collect(),
        decisions: assembly
            .output
            .decisions
            .iter()
            .map(|decision| ContextCacheDecision {
                id: decision.id.clone(),
                source_path: decision.source_path.clone(),
                action: decision.action.to_owned(),
                reason: decision.reason.to_owned(),
            })
            .collect(),
    };
    let json = serde_json::to_vec_pretty(&entry)?;
    write::write_atomic(&path, &json, &hook_options(config))?;
    Ok(path)
}

fn context_cache_path(state_dir: &std::path::Path, key: &str) -> PathBuf {
    state_dir.join("context-cache").join(format!(
        "{}.json",
        hive_memory::hash::sha256_hex(key.as_bytes())
    ))
}

/// Remove context snapshots that policy would no longer allow hooks to replay.
///
/// This runs only during explicit/background refresh, never on a hook response
/// path. Malformed entries are also disposable because they cannot be loaded.
pub(crate) fn prune_expired_context_cache(config: &Config) -> Result<usize> {
    let dir = config.state_dir.join("context-cache");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    let mut removed = 0;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let reusable = std::fs::read_to_string(&path)
            .ok()
            .and_then(|contents| serde_json::from_str::<ContextCacheEntry>(&contents).ok())
            .is_some_and(|entry| {
                context_cache_is_fresh(&entry.created_at, &config.defaults.context_cache_max_age)
            });
        if !reusable {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err.into()),
            }
            removed += 1;
        }
    }
    Ok(removed)
}

/// Load a last-success context assembly for an exact selection key.
///
/// This is intentionally stricter than a generic "last context" cache. Hook
/// fallback should only replay context after the same agent/store/project/scope
/// policy has been selected again; otherwise an offline store could leak stale
/// memory into the wrong long-lived agent session.
fn load_context_cache(
    config: &Config,
    key: &str,
    store_source: String,
    path_hint: Option<&str>,
    cached_index_path: &std::path::Path,
) -> Result<Option<CliContextAssembly>> {
    let path = context_cache_path(&config.state_dir, key);
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_CONTEXT_CACHE_BYTES) {
        return Ok(None);
    }
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    let Ok(entry) = serde_json::from_str::<ContextCacheEntry>(&contents) else {
        return Ok(None);
    };
    if entry.schema_version != 1 || entry.key != key {
        return Ok(None);
    }
    if let Some(current_generation) = index_generation(cached_index_path) {
        let cache_covers_generation = entry.index_generation.map_or_else(
            || {
                index_generation(&path)
                    .is_some_and(|cache_generation| cache_generation >= current_generation)
            },
            |cached_generation| cached_generation == current_generation,
        );
        if !cache_covers_generation {
            return Ok(None);
        }
    }
    // Cache fallback happens only after store resolution has enforced the
    // current agent policy. Matching the full context key keeps stale data tied
    // to the same selected store/project/scope/source set instead of treating
    // the cache as a general read source.
    if !context_cache_is_fresh(&entry.created_at, &config.defaults.context_cache_max_age) {
        return Ok(None);
    }

    let cached_markdown = cached_markdown_with_path(&entry.markdown, path_hint);
    let markdown = format!(
        "> Hive Memory context is served from local cache from {}; stores: {}.\n\n{}",
        entry.created_at,
        entry.stores.join(","),
        cached_markdown
    );
    let sections = entry
        .sections
        .into_iter()
        .map(|section| memory_context::ContextSection {
            id: section.id,
            store: section.store,
            scope: section.scope,
            project_id: section.project_id,
            trust: cached_trust(&section.trust),
            audience: section.audience,
            source_path: section.source_path,
            estimated_tokens: section.estimated_tokens,
            body: section.body,
        })
        .collect();
    let decisions = entry
        .decisions
        .into_iter()
        .map(|decision| memory_context::ContextDecision {
            id: decision.id,
            source_path: decision.source_path,
            action: cached_decision_label(&decision.action),
            reason: cached_decision_label(&decision.reason),
        })
        .collect();

    Ok(Some(CliContextAssembly {
        output: memory_context::ContextOutput {
            markdown,
            sections,
            decisions,
            estimated_tokens: entry.estimated_tokens,
            // Cached fallback output never replays assembly-time degradations;
            // staleness itself is already labeled on the assembly.
            warnings: Vec::new(),
        },
        agent_id: entry.agent_id,
        project_id: entry.project_id,
        project_hint: path_hint.map(str::to_owned).or(entry.project_hint),
        stores: entry.stores,
        store_source,
        scopes: entry.scopes,
        sources: entry.sources,
        include_inbox: entry.include_inbox,
        include_search_only: entry.include_search_only,
        // A cache hit means the key matched, and the key includes the strategy,
        // so the active strategy is the one this entry was written under.
        strategy: config.defaults.context_strategy.clone(),
        stale: true,
        cache_created_at: Some(entry.created_at),
    }))
}

fn index_generation(path: &std::path::Path) -> Option<u128> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

/// Refresh presentation-only path metadata when a stable project cache is reused.
fn cached_markdown_with_path(markdown: &str, path_hint: Option<&str>) -> String {
    let path_hint = path_hint.unwrap_or("none");
    let path_hint = path_hint
        .chars()
        .map(|ch| match ch {
            '\r' | '\n' | '\t' => ' ',
            _ => ch,
        })
        .collect::<String>();
    let mut replaced = false;
    let mut rewritten = markdown
        .lines()
        .map(|line| {
            if !replaced && line.starts_with("path: ") {
                replaced = true;
                format!("path: {}", path_hint.trim())
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if markdown.ends_with('\n') {
        rewritten.push('\n');
    }
    rewritten
}

/// Return whether a context cache entry is still acceptable for hook fallback.
///
/// Future timestamps are rejected instead of treated as fresh. That keeps clock
/// skew or manually edited cache files from extending stale memory indefinitely.
fn context_cache_is_fresh(created_at: &str, max_age: &str) -> bool {
    let Ok(created_at) =
        OffsetDateTime::parse(created_at, &time::format_description::well_known::Rfc3339)
    else {
        return false;
    };
    let Some(max_age) = parse_context_cache_max_age(max_age) else {
        return false;
    };
    let age = OffsetDateTime::now_utc() - created_at;
    !age.is_negative() && age <= max_age
}

/// Parse compact max-age durations used by config, such as `10m` or `2h`.
fn parse_context_cache_max_age(input: &str) -> Option<time::Duration> {
    config::parse_duration_time(input)
}

fn cached_trust(value: &str) -> memory_context::TrustLevel {
    match value {
        "curated" => memory_context::TrustLevel::Curated,
        "raw" => memory_context::TrustLevel::Raw,
        _ => memory_context::TrustLevel::Remembered,
    }
}

fn cached_decision_label(value: &str) -> &'static str {
    match value {
        "included" => "included",
        "skipped" => "skipped",
        "source" => "source",
        "scope" => "scope",
        "project" => "project",
        "audience" => "audience",
        "search-only" => "search-only",
        "budget" => "budget",
        _ => "unknown",
    }
}

fn context_json(
    assembly: CliContextAssembly,
    emitted: bool,
    stale: bool,
    cache_created_at: Option<String>,
) -> ContextJsonOutput {
    ContextJsonOutput {
        agent_id: assembly.agent_id,
        project_id: assembly.project_id,
        project_hint: assembly.project_hint,
        stores: assembly.stores,
        store_source: assembly.store_source,
        scopes: assembly.scopes,
        sources: assembly.sources,
        include_inbox: assembly.include_inbox,
        include_search_only: assembly.include_search_only,
        estimated_tokens: assembly.output.estimated_tokens,
        emitted,
        stale,
        cache_created_at,
        sections: assembly
            .output
            .sections
            .into_iter()
            .map(|section| ContextSectionJson {
                id: section.id,
                store: section.store,
                scope: section.scope,
                project_id: section.project_id,
                trust: section.trust.as_str(),
                audience: section.audience,
                source_path: section.source_path,
                estimated_tokens: section.estimated_tokens,
                body: section.body,
            })
            .collect(),
        decisions: assembly
            .output
            .decisions
            .into_iter()
            .map(|decision| ContextDecisionJson {
                id: decision.id,
                source_path: decision.source_path,
                action: decision.action,
                reason: decision.reason,
            })
            .collect(),
    }
}

fn context_json_suppressed(
    assembly: CliContextAssembly,
    stale: bool,
    cache_created_at: Option<String>,
) -> ContextJsonOutput {
    ContextJsonOutput {
        agent_id: assembly.agent_id,
        project_id: assembly.project_id,
        project_hint: assembly.project_hint,
        stores: assembly.stores,
        store_source: assembly.store_source,
        scopes: assembly.scopes,
        sources: assembly.sources,
        include_inbox: assembly.include_inbox,
        include_search_only: assembly.include_search_only,
        estimated_tokens: 0,
        emitted: false,
        stale,
        cache_created_at,
        sections: Vec::new(),
        decisions: Vec::new(),
    }
}

fn context_selection_key_from_assembly(config: &Config, assembly: &CliContextAssembly) -> String {
    let agent_id = assembly.agent_id.as_deref().unwrap_or("unknown");
    let policy = ContextKeyPolicy {
        include_inbox: assembly.include_inbox,
        include_search_only: assembly.include_search_only,
        strategy: &assembly.strategy,
    };
    let store_keys = assembly
        .stores
        .iter()
        .filter_map(|store_name| {
            config
                .stores
                .get(store_name)
                .map(|store| index::store_cache_key(store_name, &store.root))
        })
        .collect::<Vec<_>>();
    context_selection_key(
        agent_id,
        &store_keys,
        assembly.project_id.as_deref(),
        assembly.project_hint.as_deref(),
        &assembly.scopes,
        &assembly.sources,
        policy,
    )
}

/// Return the stable cursor used by `hm context --if-changed` and hook refreshes.
///
/// This key intentionally tracks selection identity, not memory file mtimes.
/// New memory writes are handled by write receipts and refresh; this cursor is
/// only for long-lived agents moving between projects, stores, or source policy.
pub(crate) fn context_selection_key(
    agent_id: &str,
    stores: &[String],
    project_id: Option<&str>,
    path_hint: Option<&str>,
    scopes: &[String],
    sources: &[String],
    policy: ContextKeyPolicy<'_>,
) -> String {
    let ContextKeyPolicy {
        include_inbox,
        include_search_only,
        strategy,
    } = policy;
    // Once a stable project id exists, the literal file/cwd hint changes only
    // presentation. Keeping it in the identity creates one cache file per
    // editor buffer and prevents same-project reuse during a store outage.
    let path_hint = project_id.is_none().then_some(path_hint).flatten();
    format_context_selection_key(
        agent_id,
        stores,
        project_id,
        path_hint,
        scopes,
        sources,
        include_inbox,
        include_search_only,
        strategy,
    )
}

#[allow(clippy::too_many_arguments)]
fn format_context_selection_key(
    agent_id: &str,
    stores: &[String],
    project_id: Option<&str>,
    path_hint: Option<&str>,
    scopes: &[String],
    sources: &[String],
    include_inbox: bool,
    include_search_only: bool,
    strategy: &str,
) -> String {
    format!(
        "agent={agent_id}\nstores={}\nproject_id={}\npath={}\nscopes={}\nsources={}\ninclude_inbox={include_inbox}\ninclude_search_only={include_search_only}\nstrategy={strategy}",
        stores.join(","),
        project_id.unwrap_or_default(),
        path_hint.unwrap_or_default(),
        scopes.join(","),
        sources.join(",")
    )
}

pub(crate) struct ContextKeyPolicy<'a> {
    pub(crate) include_inbox: bool,
    pub(crate) include_search_only: bool,
    pub(crate) strategy: &'a str,
}

#[cfg(test)]
mod tests {
    use super::cached_markdown_with_path;

    #[test]
    fn cached_path_rewrite_crosses_local_snapshot_banner() {
        let markdown = "> Hive Memory context is a local snapshot.\n\nHive Memory Context\npath: /repo/src/old.rs\n\n<memory>body</memory>\n";
        let rewritten = cached_markdown_with_path(markdown, Some("/repo/src/new.rs"));

        assert!(rewritten.contains("path: /repo/src/new.rs"));
        assert!(!rewritten.contains("path: /repo/src/old.rs"));
    }
}
