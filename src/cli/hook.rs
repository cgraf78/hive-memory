//! Agent lifecycle hook command adapters and orchestration.

use super::context::{ContextKeyPolicy, ContextSelection};
use super::sync::RefreshReport;
use crate::{
    CliContext, StoreAccess, hook_options, load_config, project_binding_store, resolve_agent_id,
    resolve_project_id, resolve_store,
};
use anyhow::Result;
use clap::{Args, Subcommand};
use hive_memory::config::Config;
use hive_memory::{
    classify, context as memory_context, hook as memory_hook, index, inject, path as memory_path,
    search, write,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Instant;
use time::OffsetDateTime;

/// Agent lifecycle hook events.
#[derive(Debug, Subcommand)]
pub(crate) enum HookCommand {
    /// Emit initial memory context for a new agent session.
    SessionStart(HookContextArgs),
    /// Inspect a submitted prompt for context changes and memory intent.
    PromptSubmit(HookPromptSubmitArgs),
    /// Handle a completed tool event.
    ToolComplete(HookToolCompleteArgs),
    /// Emit an end-of-session reminder when memory intent remains pending.
    Stop(HookStopArgs),
}

impl HookCommand {
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::SessionStart(args) => args.json,
            Self::PromptSubmit(args) => args.json,
            Self::ToolComplete(args) => args.json,
            Self::Stop(args) => args.json,
        }
    }
}

/// Shared hook context-selection arguments.
#[derive(Debug, Args)]
pub(crate) struct HookContextArgs {
    /// Active project path or file hint.
    #[arg(long)]
    project: Option<String>,
    /// Emit machine-readable hook actions.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm hook prompt-submit`.
#[derive(Debug, Args)]
pub(crate) struct HookPromptSubmitArgs {
    /// Active project path or file hint.
    #[arg(long)]
    project: Option<String>,
    /// Prompt text submitted to the agent.
    #[arg(long)]
    text: String,
    /// Emit machine-readable hook actions.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm hook tool-complete`.
#[derive(Debug, Args)]
pub(crate) struct HookToolCompleteArgs {
    /// Active project path or file hint.
    #[arg(long)]
    project: Option<String>,
    /// Tool exit/status code. Zero means success.
    #[arg(long)]
    status: i32,
    /// Emit machine-readable hook actions.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm hook stop`.
#[derive(Debug, Args)]
pub(crate) struct HookStopArgs {
    /// Emit machine-readable hook actions.
    #[arg(long)]
    json: bool,
}

fn load_cached_store_index(
    config: &Config,
    store_name: &str,
) -> Result<Option<index::LoadIndexReport>> {
    let store_config = &config.stores[store_name];
    let options = write::AtomicWriteOptions {
        fsync: config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    };
    Ok(index::load_cached_index(&index::LoadIndexInput {
        store_name,
        store_root: &store_config.root,
        cache_dir: &config.cache_dir,
        options,
        path_case: memory_path::resolve_case(&config.storage.case_sensitive, &store_config.root),
    })?)
}

fn load_fresh_store_index(
    config: &Config,
    store_name: &str,
) -> Result<Option<index::LoadIndexReport>> {
    let store_config = &config.stores[store_name];
    let options = write::AtomicWriteOptions {
        fsync: config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    };
    Ok(index::load_fresh_index(&index::LoadIndexInput {
        store_name,
        store_root: &store_config.root,
        cache_dir: &config.cache_dir,
        options,
        path_case: memory_path::resolve_case(&config.storage.case_sensitive, &store_config.root),
    })?)
}

enum PromptRecallIndex {
    Indexed(index::LoadIndexReport),
    CuratedOnly,
    Skip(&'static str),
}

fn load_prompt_recall_index(
    config: &Config,
    store_name: &str,
    curated_allowed: bool,
) -> Result<PromptRecallIndex> {
    let store_config = &config.stores[store_name];
    match load_fresh_store_index(config, store_name) {
        Ok(Some(report)) => Ok(PromptRecallIndex::Indexed(report)),
        Ok(None) if !store_config.root.is_dir() => {
            match load_cached_store_index(config, store_name) {
                Ok(Some(report)) => {
                    // `load_fresh_index` intentionally reports missing canonical
                    // roots as cache misses, not errors. The hook policy still
                    // distinguishes that offline case from a reachable stale cache:
                    // cache-only remembered recall is useful when the store root is
                    // unavailable, but stale indexed recall must not fire when the
                    // store can be inspected.
                    eprintln!(
                        "warning: prompt recall using cache-only indexed recall because store root is unavailable: {}",
                        store_config.root.display()
                    );
                    Ok(PromptRecallIndex::Indexed(report))
                }
                Ok(None) if curated_allowed => Ok(PromptRecallIndex::CuratedOnly),
                Ok(None) => Ok(PromptRecallIndex::Skip("index-not-fresh")),
                Err(err) if curated_allowed => {
                    eprintln!(
                        "warning: prompt recall using curated-only search because indexed cache fallback is unavailable: {err}"
                    );
                    Ok(PromptRecallIndex::CuratedOnly)
                }
                Err(err) => {
                    eprintln!("warning: prompt recall skipped: {err}");
                    Ok(PromptRecallIndex::Skip("index-unavailable"))
                }
            }
        }
        Ok(None) if curated_allowed => Ok(PromptRecallIndex::CuratedOnly),
        Ok(None) => Ok(PromptRecallIndex::Skip("index-not-fresh")),
        Err(err) => match load_cached_store_index(config, store_name) {
            Ok(Some(report)) => {
                // Freshness checks touch the canonical store root. When a
                // cloud/offline store is temporarily unavailable, prefer a
                // local cache-only recall over dropping remembered context
                // entirely; reachable-but-stale stores take the `Ok(None)` path
                // above and cannot serve stale indexed hits.
                eprintln!(
                    "warning: prompt recall using cache-only indexed recall because freshness check failed: {err}"
                );
                Ok(PromptRecallIndex::Indexed(report))
            }
            Ok(None) if curated_allowed => {
                eprintln!(
                    "warning: prompt recall using curated-only search because indexed recall is unavailable: {err}"
                );
                Ok(PromptRecallIndex::CuratedOnly)
            }
            Ok(None) => {
                eprintln!("warning: prompt recall skipped: {err}");
                Ok(PromptRecallIndex::Skip("index-unavailable"))
            }
            Err(cache_err) if curated_allowed => {
                eprintln!(
                    "warning: prompt recall using curated-only search because indexed recall is unavailable: {err}; cache fallback failed: {cache_err}"
                );
                Ok(PromptRecallIndex::CuratedOnly)
            }
            Err(cache_err) => {
                eprintln!(
                    "warning: prompt recall skipped: {err}; cache fallback failed: {cache_err}"
                );
                Ok(PromptRecallIndex::Skip("index-unavailable"))
            }
        },
    }
}

pub(crate) fn run(command: HookCommand, context: CliContext) -> Result<()> {
    match command {
        HookCommand::SessionStart(args) => run_session_start(args, context),
        HookCommand::PromptSubmit(args) => run_prompt_submit(args, context),
        HookCommand::ToolComplete(args) => run_tool_complete(args, context),
        HookCommand::Stop(args) => run_stop(args, context),
    }
}

/// Emit startup memory context for agent hooks.
///
/// The hook interface is deliberately policy-light for callers: dotfiles hooks
/// pass the project hint they already know, and `hm` resolves agent identity,
/// store affinity, source defaults, and context budgeting from config/env.
fn run_session_start(args: HookContextArgs, mut context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    if context.as_agent.is_none() {
        context.as_agent = std::env::var("HIVE_MEMORY_AGENT_ID").ok();
    }
    let mut warnings = Vec::new();
    let path_hint = args
        .project
        .or_else(|| std::env::var("HIVE_MEMORY_PROJECT").ok());
    let assembly = super::context::assemble_cli_context(
        &config,
        &context,
        ContextSelection {
            max_tokens: Some(usize::try_from(config.defaults.hook_context_max_tokens)?),
            include_inbox: false,
            include_search_only: false,
            explain: false,
            scopes: Vec::new(),
            sources: Vec::new(),
            project_id: std::env::var("HIVE_MEMORY_PROJECT_ID").ok(),
            path_hint: path_hint.clone(),
        },
    )?;
    if let Some(session_id) = hook_session_id(&mut warnings) {
        memory_hook::mark_startup_context(
            &config.state_dir,
            &session_id,
            hook_context_key(&config, &context, path_hint.as_deref())?,
            assembly
                .output
                .sections
                .iter()
                .map(|section| section.id.clone())
                .collect(),
            &hook_options(&config),
        )?;
    }

    let response = HookResponse {
        event: "session-start",
        actions: vec![HookAction::new("inject_context", assembly.output.markdown)],
        warnings,
        memory_pending: false,
        context_emitted: true,
        refresh: None,
        recall: None,
    };
    emit_hook_response(&response, args.json)?;

    Ok(())
}

/// Record explicit durable-memory intent from a prompt.
///
/// This hook is advisory. It does not write memory for the agent; it records a
/// session-local debt and returns a reminder action so the host integration can
/// keep the agent aware of the durable write it should make if the fact remains
/// relevant after the requested work.
fn run_prompt_submit(args: HookPromptSubmitArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let mut warnings = Vec::new();
    let mut actions = Vec::new();
    let mut memory_pending = false;
    let session_id = hook_session_id(&mut warnings);

    let path_hint = args
        .project
        .or_else(|| std::env::var("HIVE_MEMORY_PROJECT").ok());
    // Validate the same read policy used by possible context refresh below.
    // Prompt hooks should fail inside `hm` when project affinity is outside
    // agent policy, not leave shell adapters to discover that later.
    validate_hook_context_read_policy(&config, &context, path_hint.as_deref())?;
    let mut context_emitted = false;
    if let Some(action) = hook_context_action_if_changed(
        &config,
        &context,
        path_hint.as_deref(),
        session_id.as_deref(),
        false,
    )? {
        context_emitted = true;
        actions.push(action);
    }

    let recall = if context_emitted {
        Some(HookRecallReport::skipped("context-selection-changed"))
    } else {
        let (action, report) = hook_prompt_recall_action(
            &config,
            &context,
            path_hint.as_deref(),
            session_id.as_deref(),
            &args.text,
        )?;
        if let Some(action) = action {
            context_emitted = true;
            actions.push(action);
        }
        Some(report)
    };

    if memory_hook::prompt_has_memory_intent(&args.text) {
        let reminder = memory_intent_reminder();
        actions.push(HookAction::new("remind", reminder));
        memory_pending = true;

        if let Some(session_id) = session_id.as_deref() {
            memory_hook::mark_memory_pending(
                &config.state_dir,
                session_id,
                "prompt contained explicit durable-memory intent",
                &hook_options(&config),
            )?;
        }
    }

    let response = HookResponse {
        event: "prompt-submit",
        actions,
        warnings,
        memory_pending,
        context_emitted,
        refresh: None,
        recall,
    };
    emit_hook_response(&response, args.json)?;

    Ok(())
}

/// Consume session write receipts after a successful tool event.
///
/// Tool hooks are the first point where we can know whether the agent actually
/// ran `hm remember`/`hm note` after a prompt reminder. Receipts provide that
/// proof without parsing shell commands or trusting hook-side classifiers.
fn run_tool_complete(args: HookToolCompleteArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let mut warnings = Vec::new();
    let mut actions = Vec::new();
    let mut refresh = None;

    let session_id = hook_session_id(&mut warnings);
    let mut memory_pending = if let Some(session_id) = session_id.as_deref() {
        memory_hook::load_state(&config.state_dir, session_id)?.memory_pending
    } else {
        false
    };
    let mut context_emitted = false;

    if args.status == 0
        && let Some(session_id) = session_id.as_deref()
    {
        let receipts = memory_hook::load_write_receipts(&config.state_dir, session_id)?;
        let mut state = memory_hook::load_state(&config.state_dir, session_id)?;
        let unrefreshed_receipts = receipts.len().saturating_sub(state.refreshed_receipts);

        if unrefreshed_receipts > 0 {
            let receipt_project_id = receipts
                .iter()
                .skip(state.refreshed_receipts)
                .last()
                .and_then(|receipt| receipt.project_id.clone())
                .filter(|project_id| !project_id.trim().is_empty());

            let mut report = super::sync::perform(&config, false)?;
            report.record_receipts(unrefreshed_receipts);
            refresh = Some(report);

            state = memory_hook::mark_receipts_refreshed(
                &config.state_dir,
                session_id,
                receipts.len(),
                true,
                &hook_options(&config),
            )?;

            // Tool completion is a high-frequency hook. Ignore process cwd or
            // payload cwd here; in home-launched multi-project sessions those
            // hints are often stale. A successful memory write receipt carries
            // the project id that was actually written, so use that as the
            // only project-aware context-refresh signal.
            if let Some(project_id) = receipt_project_id
                && let Some(action) = hook_context_action_if_changed_for_project(
                    &config,
                    &context,
                    Some(project_id),
                    None,
                    Some(session_id),
                    false,
                )?
            {
                context_emitted = true;
                actions.push(action);
            }
        }

        memory_pending = state.memory_pending;
    }

    let response = HookResponse {
        event: "tool-complete",
        actions,
        warnings,
        memory_pending,
        context_emitted,
        refresh,
        recall: None,
    };
    emit_hook_response(&response, args.json)?;

    Ok(())
}

/// Remind at session end if explicit memory intent has not been satisfied.
fn run_stop(args: HookStopArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let mut warnings = Vec::new();
    let mut actions = Vec::new();
    let mut memory_pending = false;

    if let Some(session_id) = hook_session_id(&mut warnings) {
        let state = memory_hook::load_state(&config.state_dir, &session_id)?;
        memory_pending = state.memory_pending;
        if memory_pending {
            actions.push(HookAction::new("remind", stop_memory_reminder()));
        }
    }

    let response = HookResponse {
        event: "stop",
        actions,
        warnings,
        memory_pending,
        context_emitted: false,
        refresh: None,
        recall: None,
    };
    emit_hook_response(&response, args.json)?;
    maybe_spawn_classifier(&config, &context);

    Ok(())
}

fn maybe_spawn_classifier(config: &Config, context: &CliContext) {
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let store_name = resolve_store(
        config,
        context.store.as_deref(),
        None,
        agent_id.as_deref(),
        StoreAccess::Write,
    )
    .map(|resolved| resolved.name)
    .unwrap_or_else(|_| config.default_store.clone());

    if !classify::should_spawn(
        &config.classifier.mode,
        config.classifier_min_interval(),
        &config.state_dir,
        &store_name,
        OffsetDateTime::now_utc(),
    ) {
        return;
    }

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = std::process::Command::new(exe);
    if let Some(config_path) = &context.config_path {
        command.arg("--config").arg(config_path);
    }
    if let Some(store) = &context.store {
        command.arg("--store").arg(store);
    }
    if let Some(agent) = &context.as_agent {
        command.arg("--as-agent").arg(agent);
    }
    command
        .arg("classify")
        .arg("--auto")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

#[derive(Debug, Serialize)]
struct HookResponse {
    /// Hook event name so shell adapters can log or branch without inspecting args.
    event: &'static str,
    /// Ordered actions the hook runner should apply.
    actions: Vec<HookAction>,
    /// Non-fatal diagnostics that should be visible without failing the hook.
    warnings: Vec<String>,
    /// Whether the agent should be reminded that durable memory writes are pending.
    memory_pending: bool,
    /// Whether this response carries fresh context for prompt injection.
    context_emitted: bool,
    /// Refresh status when this hook ran post-write maintenance.
    refresh: Option<RefreshReport>,
    /// Prompt-specific recall diagnostics.
    recall: Option<HookRecallReport>,
}

#[derive(Debug, Serialize)]
struct HookRecallReport {
    /// Stable fingerprint for the prompt recall selection.
    query_fingerprint: Option<String>,
    /// Candidates returned by search before session dedupe and caps.
    candidate_count: usize,
    /// Memories selected for prompt-specific context.
    selected_count: usize,
    /// Selected memory ids.
    selected_ids: Vec<String>,
    /// Stable reason key.
    reason: &'static str,
    /// Whether the previous prompt recall cursor already covered this result.
    reused_previous: bool,
    /// Whether recall was skipped because it exceeded the hook budget.
    timed_out: bool,
    /// Retrieval duration in milliseconds.
    retrieval_ms: u128,
}

impl HookRecallReport {
    fn skipped(reason: &'static str) -> Self {
        Self {
            query_fingerprint: None,
            candidate_count: 0,
            selected_count: 0,
            selected_ids: Vec::new(),
            reason,
            reused_previous: false,
            timed_out: false,
            retrieval_ms: 0,
        }
    }
}

#[derive(Debug, Serialize)]
struct HookAction {
    /// Small action discriminator understood by dotfiles hook adapters.
    kind: &'static str,
    /// Markdown/data payload for this action.
    body: String,
}

impl HookAction {
    fn new(kind: &'static str, body: impl Into<String>) -> Self {
        Self {
            kind,
            body: body.into(),
        }
    }
}

fn emit_hook_response(response: &HookResponse, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(response)?);
        return Ok(());
    }

    if response.actions.is_empty() && response.warnings.is_empty() {
        println!("hook {}: no actions", response.event);
        return Ok(());
    }

    for warning in &response.warnings {
        println!("warn: {warning}");
    }
    for action in &response.actions {
        print!("{}: {}", action.kind, action.body);
        if !action.body.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

fn hook_session_id(warnings: &mut Vec<String>) -> Option<String> {
    match std::env::var("HIVE_MEMORY_SESSION_ID") {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => {
            warnings.push(
                "HIVE_MEMORY_SESSION_ID missing; hook memory-pending state is stateless".to_owned(),
            );
            None
        }
    }
}

fn hook_context_action_if_changed(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
    session_id: Option<&str>,
    emit_initial: bool,
) -> Result<Option<HookAction>> {
    hook_context_action_if_changed_for_project(
        config,
        context,
        None,
        path_hint,
        session_id,
        emit_initial,
    )
}

fn hook_context_action_if_changed_for_project(
    config: &Config,
    context: &CliContext,
    project_id: Option<String>,
    path_hint: Option<&str>,
    session_id: Option<&str>,
    emit_initial: bool,
) -> Result<Option<HookAction>> {
    let Some(session_id) = session_id else {
        return Ok(None);
    };

    // Long-lived agents can move between projects while the process stays
    // alive. Cache the resolved selection, not just "context was already sent",
    // so hooks can reinject when path/project/store policy changes.
    let context_key = hook_context_key_for_project(config, context, project_id.clone(), path_hint)?;
    let state = memory_hook::load_state(&config.state_dir, session_id)?;
    // SessionStart owns initial context injection. Prompt/tool hooks should only
    // reinject after an existing session selection changes; otherwise hook
    // runners that fire SessionStart and PromptSubmit close together can show
    // duplicate "Hive Memory Context" blocks before either process observes the
    // other's freshly written state.
    if state.context_key.is_none() && !emit_initial {
        return Ok(None);
    }
    if state.context_key.as_deref() == Some(context_key.as_str()) {
        return Ok(None);
    }

    let assembly = super::context::assemble_cli_context(
        config,
        context,
        ContextSelection {
            max_tokens: Some(usize::try_from(config.defaults.hook_context_max_tokens)?),
            include_inbox: false,
            include_search_only: false,
            explain: false,
            scopes: Vec::new(),
            sources: Vec::new(),
            project_id: project_id.or_else(|| std::env::var("HIVE_MEMORY_PROJECT_ID").ok()),
            path_hint: path_hint.map(str::to_owned),
        },
    )?;
    let section_ids = assembly
        .output
        .sections
        .iter()
        .map(|section| section.id.clone())
        .collect();
    memory_hook::mark_startup_context(
        &config.state_dir,
        session_id,
        context_key,
        section_ids,
        &hook_options(config),
    )?;

    Ok(Some(HookAction::new(
        "inject_context",
        assembly.output.markdown,
    )))
}

fn hook_prompt_recall_action(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
    session_id: Option<&str>,
    prompt: &str,
) -> Result<(Option<HookAction>, HookRecallReport)> {
    let Some(session_id) = session_id else {
        return Ok((None, HookRecallReport::skipped("no-session-id")));
    };
    let Some(query) = prompt_recall_query(prompt, path_hint) else {
        return Ok((None, HookRecallReport::skipped("budget-empty")));
    };

    let started = Instant::now();
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let project_id = resolve_project_id(None, path_hint)?;
    let project_binding = project_binding_store(config, project_id.as_deref())?;
    let resolved_store = resolve_store(
        config,
        context.store.as_deref(),
        project_binding.as_deref(),
        agent_id.as_deref(),
        StoreAccess::Read,
    )?;
    let store_name = resolved_store.name.clone();
    let store_config = &config.stores[store_name.as_str()];
    // Prompt recall is an automatic `hm search`, so it follows the same source
    // defaults. Raw inbox material remains opt-in through that policy.
    let sources = &config.defaults.search_sources;
    let curated_allowed = super::search::source_filter_includes_curated(sources);
    let cached_report = match load_prompt_recall_index(config, &store_name, curated_allowed)? {
        PromptRecallIndex::Indexed(report) => Some(report),
        PromptRecallIndex::CuratedOnly => None,
        PromptRecallIndex::Skip(reason) => {
            let mut recall = HookRecallReport::skipped(reason);
            recall.retrieval_ms = started.elapsed().as_millis();
            return Ok((None, recall));
        }
    };
    let empty_entries = Vec::new();
    let entries = cached_report
        .as_ref()
        .map(|report| report.entries.as_slice())
        .unwrap_or_else(|| empty_entries.as_slice());
    let curated_only_sources;
    let search_sources = if cached_report.is_some() {
        sources.as_slice()
    } else {
        // When the note index is stale/missing, remembered/raw recall would be
        // incomplete. Curated Markdown is read directly, so keep that part of
        // the configured recall surface available without pretending the JSONL
        // note corpus was searched.
        curated_only_sources = vec!["curated".to_owned()];
        curated_only_sources.as_slice()
    };
    let include_inbox = super::search::search_include_inbox(false, search_sources);
    let search_input = search::SearchInput {
        store_root: &store_config.root,
        entries,
        query: &query,
        scopes: &config.defaults.search_scopes,
        sources: search_sources,
        include_inbox,
        agent_id: agent_id.as_deref(),
        project_id: project_id.as_deref(),
        limit: 10,
    };
    // Prefer BM25 recall when the persistent index is already fresh; this is
    // where the prompt hook gains paraphrase/multi-session recall. Fall back to
    // the lexical scan when the index is stale/absent so the hook never pays for
    // a rebuild on its latency budget (refresh/tool-complete keeps it fresh).
    let hits =
        match super::search::tantivy_search_if_fresh(config, &store_name, search_input.clone()) {
            Some(hits) => hits,
            None => match search::search(search_input) {
                Ok(hits) => hits,
                Err(search::SearchError::EmptyQuery) => {
                    return Ok((None, HookRecallReport::skipped("budget-empty")));
                }
                Err(err) => {
                    let mut recall = HookRecallReport::skipped("index-unavailable");
                    recall.retrieval_ms = started.elapsed().as_millis();
                    eprintln!("warning: prompt recall skipped: {err}");
                    return Ok((None, recall));
                }
            },
        };

    let state = memory_hook::load_state(&config.state_dir, session_id)?;
    let known_ids = memory_hook::known_session_memory_ids(&state);
    let selected_entries = hits
        .iter()
        .filter(|hit| !known_ids.contains(&hit.entry.id))
        .take(3)
        .map(|hit| hit.entry.clone())
        .collect::<Vec<_>>();
    let selected_ids = selected_entries
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();
    let recall_key = prompt_recall_key(&query, project_id.as_deref(), &store_name, &selected_ids);
    let mut recall = HookRecallReport {
        query_fingerprint: Some(recall_key.clone()),
        candidate_count: hits.len(),
        selected_count: selected_ids.len(),
        selected_ids: selected_ids.clone(),
        reason: "selected",
        reused_previous: false,
        timed_out: false,
        retrieval_ms: started.elapsed().as_millis(),
    };

    if selected_entries.is_empty() {
        recall.reason = if hits.is_empty() {
            "below-threshold"
        } else {
            "unchanged"
        };
        recall.selected_count = 0;
        recall.selected_ids.clear();
        return Ok((None, recall));
    }
    if state.prompt_recall_key.as_deref() == Some(recall_key.as_str()) {
        recall.reason = "unchanged";
        recall.reused_previous = true;
        return Ok((None, recall));
    }

    let max_tokens = usize::try_from(config.defaults.hook_context_max_tokens)?.clamp(200, 1_200);
    let output = memory_context::assemble_selected_context(memory_context::ContextInput {
        store_name: store_name.as_str(),
        store_root: &store_config.root,
        entries: &selected_entries,
        scopes: &config.defaults.search_scopes,
        sources: search_sources,
        include_inbox,
        include_search_only: true,
        agent_id: agent_id.as_deref(),
        project_id: project_id.as_deref(),
        path_hint,
        max_tokens,
        inject_strategy: inject::Strategy::from_config(&config.defaults.context_strategy),
        explain: false,
    })?;
    if output.sections.is_empty() {
        recall.reason = "below-threshold";
        recall.selected_count = 0;
        recall.selected_ids.clear();
        return Ok((None, recall));
    }

    let emitted_ids = output
        .sections
        .iter()
        .map(|section| section.id.clone())
        .collect::<Vec<_>>();
    memory_hook::mark_prompt_recall(
        &config.state_dir,
        session_id,
        recall_key,
        emitted_ids.clone(),
        &hook_options(config),
    )?;
    recall.selected_count = emitted_ids.len();
    recall.selected_ids = emitted_ids;
    recall.retrieval_ms = started.elapsed().as_millis();

    Ok((
        Some(HookAction::new("inject_context", output.markdown)),
        recall,
    ))
}

fn prompt_recall_key(
    query: &str,
    project_id: Option<&str>,
    store_name: &str,
    ids: &[String],
) -> String {
    let mut digest = Sha256::new();
    digest.update(query.as_bytes());
    digest.update(b"\0");
    digest.update(project_id.unwrap_or_default().as_bytes());
    digest.update(b"\0");
    digest.update(store_name.as_bytes());
    for id in ids {
        digest.update(b"\0");
        digest.update(id.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn prompt_recall_query(prompt: &str, path_hint: Option<&str>) -> Option<String> {
    let mut code_terms = Vec::new();
    let mut plain_terms = Vec::new();
    for token in prompt
        .split(|ch: char| {
            ch.is_whitespace() || matches!(ch, ',' | ';' | ':' | '(' | ')' | '[' | ']')
        })
        .map(normalize_prompt_token)
        .filter(|token| !token.is_empty())
    {
        if is_prompt_stopword(&token) {
            continue;
        }
        if is_code_prompt_term(&token) {
            push_unique(&mut code_terms, token);
        } else if token.len() >= 4 {
            push_unique(&mut plain_terms, token);
        }
    }
    if code_terms.is_empty()
        && plain_terms.is_empty()
        && let Some(path_hint) = path_hint
        && let Some(file_name) = Path::new(path_hint)
            .file_name()
            .and_then(|name| name.to_str())
    {
        let token = normalize_prompt_token(file_name);
        if is_code_prompt_term(&token) {
            push_unique(&mut code_terms, token);
        }
    }

    let terms = if !code_terms.is_empty() {
        code_terms.into_iter().take(3).collect::<Vec<_>>()
    } else {
        plain_terms.into_iter().take(2).collect::<Vec<_>>()
    };
    (!terms.is_empty()).then(|| terms.join(" "))
}

fn normalize_prompt_token(token: &str) -> String {
    token
        .trim_matches(|ch: char| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '/' | '<' | '>'))
        })
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn is_code_prompt_term(token: &str) -> bool {
    matches!(token, "agents.md" | "cargo.toml" | "checkrun" | "sley")
        || token.contains('/')
        || token.contains('.')
        || token.contains('<')
}

fn is_prompt_stopword(token: &str) -> bool {
    matches!(
        token,
        "about"
            | "after"
            | "again"
            | "also"
            | "could"
            | "from"
            | "into"
            | "please"
            | "recall"
            | "remember"
            | "should"
            | "that"
            | "this"
            | "what"
            | "when"
            | "where"
            | "with"
            | "would"
    )
}

fn push_unique(terms: &mut Vec<String>, token: String) {
    if !terms.iter().any(|existing| existing == &token) {
        terms.push(token);
    }
}

fn hook_context_key(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
) -> Result<String> {
    hook_context_key_for_project(config, context, None, path_hint)
}

fn hook_context_key_for_project(
    config: &Config,
    context: &CliContext,
    project_id: Option<String>,
    path_hint: Option<&str>,
) -> Result<String> {
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let agent_label = agent_id.clone().unwrap_or_else(|| "unknown".to_owned());
    let project_id = resolve_project_id(project_id, path_hint)?;
    let project_binding = project_binding_store(config, project_id.as_deref())?;
    let resolved_store = resolve_store(
        config,
        context.store.as_deref(),
        project_binding.as_deref(),
        agent_id.as_deref(),
        StoreAccess::Read,
    )?;

    Ok(super::context::context_selection_key(
        &agent_label,
        &[resolved_store.name],
        project_id.as_deref(),
        path_hint,
        &config.defaults.search_scopes,
        &config.defaults.context_sources,
        ContextKeyPolicy {
            include_inbox: false,
            include_search_only: false,
            strategy: &config.defaults.context_strategy,
        },
    ))
}

/// Validate read-side hook policy without assembling or emitting context.
///
/// Prompt/tool hooks call this before doing auxiliary work so policy failures
/// are reported by `hm` consistently, while the shell hook remains a thin event
/// adapter with no duplicate store-affinity logic.
fn validate_hook_context_read_policy(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
) -> Result<()> {
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let project_id = resolve_project_id(None, path_hint)?;
    let project_binding = project_binding_store(config, project_id.as_deref())?;
    resolve_store(
        config,
        context.store.as_deref(),
        project_binding.as_deref(),
        agent_id.as_deref(),
        StoreAccess::Read,
    )?;
    Ok(())
}

fn memory_intent_reminder() -> &'static str {
    "Hive Memory: this prompt sounds like durable memory intent. If it remains useful, write it with `hm remember --scope project --text \"...\"` or `hm remember --text \"...\"`."
}

fn stop_memory_reminder() -> &'static str {
    "Hive Memory: durable memory intent is still pending. Before ending, write any lasting preference, decision, or project fact with `hm remember`."
}

#[cfg(test)]
mod tests {
    use super::prompt_recall_query;

    #[test]
    fn prompt_recall_query_ignores_recall_intent_words() {
        assert_eq!(
            prompt_recall_query(
                "Recall the Grafhome Cedar and Rego policy format comparison.",
                None
            )
            .as_deref(),
            Some("grafhome cedar")
        );
    }
}
