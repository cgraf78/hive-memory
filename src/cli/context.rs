//! Context CLI assembly, cache, output models, and selection identity.

use crate::{
    BackendUnavailable, CliContext, StoreAccess, context_session_id, hook_active, hook_options,
    load_config, project_binding_store, rebuild_store_index, resolve_agent_id, resolve_project_id,
    resolve_store,
};
use anyhow::Result;
use clap::Args;
use hive_memory::config::Config;
use hive_memory::{config, context as memory_context, hook as memory_hook, inject, store, write};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use time::OffsetDateTime;

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
        let context_key = context_selection_key_from_assembly(&assembly);
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
    stores: Vec<String>,
    store_source: String,
    scopes: Vec<String>,
    sources: Vec<String>,
    /// Whether raw inbox records were eligible for this assembly.
    include_inbox: bool,
    /// Whether search-only records were intentionally rendered.
    include_search_only: bool,
    /// Resolved selection strategy label, part of the cache key.
    strategy: String,
    stale: bool,
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
    let context_key = context_selection_key(
        agent_id.as_deref().unwrap_or("unknown"),
        &stores,
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
    let hook_active = hook_active(context);
    if hook_active
        && let Err(store::StoreError::Io { .. }) = store::read_manifest(&store_config.root)
    {
        // Hook context runs at agent startup/prompt boundaries, where failing
        // hard on an offline cloud/mount path is worse than using the last
        // known-good context. Outside hook mode, interactive commands should
        // rebuild normally and surface the underlying store read failure.
        if let Some(assembly) = load_context_cache(config, &context_key, store_source.clone())? {
            return Ok(assembly);
        }
        return Err(BackendUnavailable {
            message: format!(
                "store {} is unavailable and no valid context cache exists",
                resolved_store.name
            ),
        }
        .into());
    }
    let max_tokens = selection.max_tokens.unwrap_or_else(|| {
        // Hooks run on latency-sensitive agent boundaries, so they use the
        // configured hook budget unless the caller has explicitly provided a
        // tighter or broader limit. Interactive `hm context` keeps the larger
        // v1 default for inspection and manual debugging.
        if hook_active {
            usize::try_from(config.defaults.hook_context_max_tokens)
                .expect("hook context token budget fits usize")
        } else {
            4000
        }
    });

    // Fresh assembly can still fail past the manifest check: the index rebuild
    // or a curated/canonical read can hit a mid-sync file on a cloud-backed
    // root. In hook mode those failures degrade to the last known-good cache,
    // exactly like an unreachable store; interactive commands surface the
    // underlying error so the store gets fixed instead of papered over.
    let fresh = rebuild_store_index(config, &resolved_store.name).and_then(|report| {
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
    });
    let output = match fresh {
        Ok(output) => output,
        Err(err) => {
            if hook_active
                && let Some(assembly) =
                    load_context_cache(config, &context_key, store_source.clone())?
            {
                eprintln!("warning: hook context degraded to cached fallback: {err}");
                return Ok(assembly);
            }
            return Err(err);
        }
    };
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
    let key = context_selection_key_from_assembly(assembly);
    let path = context_cache_path(&config.state_dir, &key);
    let entry = ContextCacheEntry {
        schema_version: 1,
        created_at: OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("RFC3339 formatting should not fail"),
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
    let digest = Sha256::digest(key.as_bytes());
    state_dir
        .join("context-cache")
        .join(format!("{digest:x}.json"))
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
) -> Result<Option<CliContextAssembly>> {
    let path = context_cache_path(&config.state_dir, key);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let entry: ContextCacheEntry = serde_json::from_str(&contents)?;
    if entry.schema_version != 1 || entry.key != key {
        return Ok(None);
    }
    // Cache fallback happens only after store resolution has enforced the
    // current agent policy. Matching the full context key keeps stale data tied
    // to the same selected store/project/scope/source set instead of treating
    // the cache as a general read source.
    if !context_cache_is_fresh(&entry.created_at, &config.defaults.context_cache_max_age) {
        return Ok(None);
    }

    let markdown = format!(
        "> Hive Memory context is stale offline cache from {}; stores: {}.\n\n{}",
        entry.created_at,
        entry.stores.join(","),
        entry.markdown
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
        project_hint: entry.project_hint,
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

fn context_selection_key_from_assembly(assembly: &CliContextAssembly) -> String {
    let agent_id = assembly.agent_id.as_deref().unwrap_or("unknown");
    let policy = ContextKeyPolicy {
        include_inbox: assembly.include_inbox,
        include_search_only: assembly.include_search_only,
        strategy: &assembly.strategy,
    };
    context_selection_key(
        agent_id,
        &assembly.stores,
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
