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
    /// Record that the host lost earlier context (compaction, clear, resume).
    ContextReset(HookResetArgs),
}

impl HookCommand {
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::SessionStart(args) => args.json,
            Self::PromptSubmit(args) => args.json,
            Self::ToolComplete(args) => args.json,
            Self::Stop(args) => args.json,
            Self::ContextReset(args) => args.json,
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
    #[command(flatten)]
    delivery: HookDeliveryArgs,
}

/// Delivery-tracking options for hooks that inject context.
///
/// All are optional. Without them a hook behaves exactly as before; adapters
/// opt in per runtime once they know the host's context lifetime, size limit,
/// and context-loss signals.
#[derive(Debug, Args)]
pub(crate) struct HookDeliveryArgs {
    /// Track which memories the model can see and skip ones already visible.
    /// The value says how long the host keeps this hook's context: until a
    /// context reset (`persistent`) or for the current turn only (`turn`).
    #[arg(long, value_enum)]
    delivery: Option<DeliveryArg>,
    /// Largest Markdown context, in bytes, the host shows the model whole.
    #[arg(long)]
    max_context_bytes: Option<usize>,
    /// The host lost earlier context; start a new context epoch first.
    #[arg(long, requires = "delivery")]
    reset: bool,
    /// How `--max-context-bytes` counts: `bytes` of Markdown, or `json` for
    /// bytes once escaped as a JSON string (hosts that limit the response).
    /// Tracked hooks only; untracked ones always count bytes.
    #[arg(long, value_enum, default_value_t = ContextSizeArg::Bytes, requires = "delivery")]
    context_size: ContextSizeArg,
}

/// CLI spelling of [`memory_context::SizeMeasure`].
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum ContextSizeArg {
    /// UTF-8 bytes of the Markdown.
    Bytes,
    /// Bytes once escaped as a JSON string.
    Json,
}

impl ContextSizeArg {
    fn measure(self) -> memory_context::SizeMeasure {
        match self {
            Self::Bytes => memory_context::SizeMeasure::Bytes,
            Self::Json => memory_context::SizeMeasure::JsonString,
        }
    }
}

/// CLI spelling of [`memory_hook::DeliveryLifetime`].
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum DeliveryArg {
    /// Context stays visible until the adapter reports a context reset.
    Persistent,
    /// Context is visible for the current turn only.
    Turn,
}

impl DeliveryArg {
    fn lifetime(self) -> memory_hook::DeliveryLifetime {
        match self {
            Self::Persistent => memory_hook::DeliveryLifetime::Persistent,
            Self::Turn => memory_hook::DeliveryLifetime::Turn,
        }
    }
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
    #[command(flatten)]
    delivery: HookDeliveryArgs,
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

/// Arguments for `hm hook context-reset`.
#[derive(Debug, Args)]
pub(crate) struct HookResetArgs {
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

pub(crate) fn load_cached_store_index(
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
        // Cache-only reads do not serialize paths; probing auto case behavior
        // here would touch the canonical mount on the hook hot path.
        path_case: memory_path::PathCase::Sensitive,
    })?)
}

enum PromptRecallIndex {
    Indexed(index::LoadIndexReport),
    Skip(&'static str),
}

fn load_prompt_recall_index(config: &Config, store_name: &str) -> Result<PromptRecallIndex> {
    if let Some(report) = load_cached_store_index(config, store_name)? {
        // Prompt hooks are latency-sensitive. Once a complete local projection
        // exists, use it without first proving freshness against a cloud mount;
        // refresh is deliberately outside the synchronous response path.
        return Ok(PromptRecallIndex::Indexed(report));
    }
    Ok(PromptRecallIndex::Skip("index-not-ready"))
}

pub(crate) fn run(command: HookCommand, context: CliContext) -> Result<()> {
    match command {
        HookCommand::SessionStart(args) => run_session_start(args, context),
        HookCommand::PromptSubmit(args) => run_prompt_submit(args, context),
        HookCommand::ToolComplete(args) => run_tool_complete(args, context),
        HookCommand::Stop(args) => run_stop(args, context),
        HookCommand::ContextReset(args) => run_context_reset(args, context),
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
    // Delivery tracking needs the session id before assembly so a reset
    // applies to the state this response is selected
    // against. Legacy mode keeps resolving it lazily (its warning appears
    // only when context is emitted).
    let tracker = DeliveryTracker::start(&config, &args.delivery)?;
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
    let schedule_refresh = assembly.stale;
    let refresh_target = assembly
        .stores
        .first()
        .cloned()
        .map(BackgroundRefreshTarget::One)
        .unwrap_or(BackgroundRefreshTarget::All);

    let mut deferred = None;
    let mut delivery = None;
    let (context_emitted, markdown) = if let Some(tracker) = &tracker {
        let context_key = hook_context_key(&config, &context, path_hint.as_deref())?;
        let selected = tracker.select(&assembly.output, assembly.stale);
        let mut report = tracker.report(Some(&selected));
        if selected.emitted {
            deferred = Some(tracker.update(&selected, Some(context_key), None));
        }
        report.emitted_ids = ids(&selected.output);
        delivery = Some(report);
        (selected.emitted, selected.output.markdown)
    } else {
        let output = fit_legacy_output(&assembly.output, args.delivery.max_context_bytes);
        let context_emitted = (!assembly.stale || !output.sections.is_empty())
            && !(args.delivery.max_context_bytes.is_some() && output.markdown.is_empty());
        if context_emitted && let Some(session_id) = hook_session_id(&mut warnings) {
            memory_hook::mark_startup_context(
                &config.state_dir,
                &session_id,
                hook_context_key(&config, &context, path_hint.as_deref())?,
                ids(&output),
                &hook_options(&config),
            )?;
        }
        (context_emitted, output.markdown)
    };

    let actions = if context_emitted {
        vec![HookAction::new("inject_context", markdown)]
    } else {
        Vec::new()
    };

    let response = HookResponse {
        event: "session-start",
        actions,
        warnings,
        memory_pending: false,
        context_emitted,
        refresh: None,
        recall: None,
        delivery,
    };
    emit_hook_response(&response, args.json)?;
    // Applied only after the response is written, so a failed write never
    // marks anything delivered.
    if let (Some(tracker), Some(update)) = (&tracker, deferred) {
        tracker.apply_now(update);
    }
    if schedule_refresh {
        maybe_spawn_background_refresh(&config, &context, refresh_target, None);
    }

    Ok(())
}

/// Apply the host size limit to legacy (untracked) output.
///
/// Legacy mode records every emitted id as sent, so it must emit only what
/// fits; an output that cannot be split stays whole, as before this option.
fn fit_legacy_output(
    output: &memory_context::ContextOutput,
    max_bytes: Option<usize>,
) -> memory_context::ContextOutput {
    match max_bytes {
        Some(max) => output
            .select_sections(|_| true, Some(memory_context::SizeLimit::bytes(max)))
            .map_or_else(|| output.clone(), |subset| subset.output),
        None => output.clone(),
    }
}

fn ids(output: &memory_context::ContextOutput) -> Vec<String> {
    output
        .sections
        .iter()
        .map(|section| section.id.clone())
        .collect()
}

/// Delivery tracking for one hook invocation (`--delivery`).
///
/// The tracker owns the rule that a memory counts as delivered once it is
/// emitted whole within the host's size limit: it filters already-visible
/// memories, fits the limit, and records bookkeeping after the response is
/// written. Memories dropped by the limit stay undelivered and are sent later.
struct DeliveryTracker<'a> {
    config: &'a Config,
    session_id: String,
    lifetime: memory_hook::DeliveryLifetime,
    max_bytes: Option<usize>,
    measure: memory_context::SizeMeasure,
    reset: bool,
    /// Session state after this hook's reset.
    state: memory_hook::HookState,
}

/// A tracked selection and the bookkeeping to apply once it is written.
type TrackedEmission = (TrackedSelection, memory_hook::DeliveryUpdate);

/// Selected context for one tracked hook response.
struct TrackedSelection {
    output: memory_context::ContextOutput,
    emitted: bool,
    filtered: usize,
    over_budget: usize,
    /// Memories too large for the limit, shown as pointer blocks instead.
    omitted: usize,
}

/// Drop already-visible memories from `output` and fit the host limit.
///
/// Output that cannot be split is emitted whole only when it fits the limit;
/// otherwise nothing is emitted (the host would hide or drop it). A memory too
/// large for the hook's whole limit (`limit.alone_max`) becomes a pointer block so it
/// cannot hold back the memories ranked below it. A header-only response is emitted when the
/// selection has no memories or all of them are already visible, so the model
/// still sees the project/path switch; it is not emitted when memories were
/// dropped by the limit (they are sent later) or a stale cache has nothing.
fn select_tracked<F>(
    output: &memory_context::ContextOutput,
    stale: bool,
    is_delivered: F,
    limit: Option<memory_context::SizeLimit>,
) -> TrackedSelection
where
    F: Fn(&memory_context::ContextSection) -> bool,
{
    let Some(subset) = output.select_sections(|section| !is_delivered(section), limit) else {
        let fits = limit.is_none_or(|limit| limit.measure.of(&output.markdown) <= limit.max);
        return TrackedSelection {
            emitted: fits && (!stale || !output.sections.is_empty()),
            output: output.clone(),
            filtered: 0,
            over_budget: if fits { 0 } else { output.sections.len() },
            omitted: 0,
        };
    };
    let emitted = if subset.output.markdown.is_empty() {
        false
    } else if !subset.output.sections.is_empty() || !subset.omitted.is_empty() {
        true
    } else {
        subset.over_budget == 0 && (!stale || subset.filtered > 0)
    };
    TrackedSelection {
        output: subset.output,
        emitted,
        filtered: subset.filtered,
        over_budget: subset.over_budget,
        omitted: subset.omitted.len(),
    }
}

impl<'a> DeliveryTracker<'a> {
    /// Start tracking when `--delivery` was given and a session id exists.
    ///
    /// Without a session id there is no state to track against, so the hook
    /// falls back to legacy behavior.
    fn start(config: &'a Config, args: &HookDeliveryArgs) -> Result<Option<Self>> {
        let Some(delivery) = args.delivery else {
            return Ok(None);
        };
        // Untracked fallback reports a missing session id itself.
        let Some(session_id) = std::env::var("HIVE_MEMORY_SESSION_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        let options = hook_options(config);
        if args.reset {
            // A failed reset must fail the hook: continuing would filter
            // against a delivered set the host no longer shows. The adapter
            // retries the reset on its next tracked hook.
            memory_hook::reset_context_epoch(&config.state_dir, &session_id, &options)?;
        }
        let state = memory_hook::load_state(&config.state_dir, &session_id)?;
        Ok(Some(Self {
            config,
            session_id,
            lifetime: delivery.lifetime(),
            max_bytes: args.max_context_bytes,
            measure: args.context_size.measure(),
            reset: args.reset,
            state,
        }))
    }

    fn is_delivered(&self, section: &memory_context::ContextSection) -> bool {
        self.state
            .has_delivered(&section.id, &section.body_digest())
    }

    fn select(&self, output: &memory_context::ContextOutput, stale: bool) -> TrackedSelection {
        select_tracked(
            output,
            stale,
            |section| self.is_delivered(section),
            self.limit(self.max_bytes),
        )
    }

    /// Size limit with `space` left in this response. "Too large" is always
    /// judged against the hook's whole limit, so a memory that only misses the
    /// space left waits for a later prompt instead of becoming a pointer.
    fn limit(&self, space: Option<usize>) -> Option<memory_context::SizeLimit> {
        space.map(|space| memory_context::SizeLimit {
            max: space,
            alone_max: self.max_bytes.unwrap_or(space),
            measure: self.measure,
            raw_max: usize::MAX,
        })
    }

    /// Bookkeeping for a selection, applied once it is delivered.
    ///
    /// A persistent selection key is recorded once nothing it covers was
    /// dropped by the size limit (otherwise later hooks send the rest).
    /// Turn-lifetime output records its key on emission: like untracked hooks
    /// it is sent once per change, fitted to the limit, although the host
    /// drops it on the next turn. Its memories never join the delivered set,
    /// so recall and later selections still offer them.
    fn update(
        &self,
        selected: &TrackedSelection,
        context_key: Option<String>,
        prompt_recall_key: Option<String>,
    ) -> memory_hook::DeliveryUpdate {
        let persistent = self.lifetime == memory_hook::DeliveryLifetime::Persistent;
        let complete = !persistent || selected.over_budget == 0;
        // The untracked fields record an emission only once nothing in it was
        // left out by the size limit: an untracked hook would have sent the
        // whole selection, so an untracked fallback must still send the rest.
        let emitted =
            (selected.emitted && selected.over_budget == 0).then(|| ids(&selected.output));
        let memories = if persistent {
            selected
                .output
                .sections
                .iter()
                .map(|section| memory_hook::DeliveredMemory {
                    id: section.id.clone(),
                    digest: section.body_digest(),
                })
                .collect()
        } else {
            Vec::new()
        };
        memory_hook::DeliveryUpdate {
            epoch: self.state.context_epoch,
            memories,
            untracked_context_key: context_key.clone().filter(|_| emitted.is_some()),
            untracked_context_ids: context_key
                .as_ref()
                .and(emitted.clone())
                .unwrap_or_default(),
            untracked_recall_key: prompt_recall_key.clone().filter(|_| emitted.is_some()),
            untracked_recall_ids: prompt_recall_key.as_ref().and(emitted).unwrap_or_default(),
            turn_context_ids: (!persistent && selected.emitted && context_key.is_some())
                .then(|| ids(&selected.output)),
            context_key: context_key.filter(|_| complete),
            prompt_recall_key: prompt_recall_key.filter(|_| complete),
            turn_recall_ids: Vec::new(),
        }
    }

    /// Apply bookkeeping after the response was written. Best effort: losing
    /// an update only means its memories are offered again.
    fn apply_now(&self, update: memory_hook::DeliveryUpdate) {
        if let Err(err) = memory_hook::apply_delivery(
            &self.config.state_dir,
            &self.session_id,
            update,
            &hook_options(self.config),
        ) {
            eprintln!("warning: Hive Memory delivery bookkeeping skipped: {err}");
        }
    }

    fn report(&self, selected: Option<&TrackedSelection>) -> HookDeliveryReport {
        HookDeliveryReport {
            lifetime: self.lifetime.as_str(),
            reset: self.reset,
            emitted_ids: Vec::new(),
            skipped_delivered: selected.map_or(0, |selected| selected.filtered),
            skipped_over_budget: selected.map_or(0, |selected| selected.over_budget),
            omitted: selected.map_or(0, |selected| selected.omitted),
        }
    }
}

/// Refresh the local projection after returning stale-while-revalidate data.
///
/// Lifecycle hooks use this path automatically so a long-lived session sees
/// other writers without paying canonical I/O in the foreground. The child is
/// detached, periodic attempts are rate-limited, and a repeated receipt cursor
/// is suppressed through the watchdog window. A new cursor launches immediately.
#[derive(Debug)]
enum BackgroundRefreshTarget {
    One(String),
    All,
}

fn maybe_spawn_background_refresh(
    config: &Config,
    context: &CliContext,
    target: BackgroundRefreshTarget,
    receipt_request: Option<(&str, usize)>,
) {
    let refresh_key = match &target {
        BackgroundRefreshTarget::One(store_name) => config
            .stores
            .get(store_name)
            .map(|store| index::store_cache_key(store_name, &store.root))
            .unwrap_or_else(|| "all-stores".to_owned()),
        BackgroundRefreshTarget::All => "all-stores".to_owned(),
    };
    // The same lock used for session-state read-modify-write also serializes
    // receipt attempt publication. Contention is a duplicate or a nearby state
    // update, so fail soft and let a later hook retry rather than spawning.
    let _receipt_lock = if let Some((session_id, _)) = receipt_request {
        match memory_hook::try_state_lock(&config.state_dir, session_id) {
            Ok(Some(lock)) => Some(lock),
            _ => return,
        }
    } else {
        None
    };
    let (stamp, retry_after, attempt_body) = match receipt_request {
        Some((session_id, cursor)) => (
            memory_hook::receipt_refresh_attempt_path(&config.state_dir, session_id),
            std::time::Duration::from_secs(super::sync::BACKGROUND_REFRESH_WATCHDOG_SECS),
            cursor.to_string().into_bytes(),
        ),
        None => (
            config
                .state_dir
                .join("background-refresh")
                .join(format!("{refresh_key}.last-attempt")),
            std::time::Duration::from_secs(60),
            Vec::new(),
        ),
    };
    let recent_attempt = std::fs::metadata(&stamp)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < retry_after);
    let same_request = receipt_request.is_none()
        || std::fs::read(&stamp).is_ok_and(|contents| contents == attempt_body);
    if recent_attempt && same_request {
        return;
    }
    let Some(parent) = stamp.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err()
        || write::write_atomic(&stamp, &attempt_body, &hook_options(config)).is_err()
    {
        return;
    }

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = std::process::Command::new(exe);
    if let Some(config_path) = &context.config_path {
        command.arg("--config").arg(config_path);
    }
    if let BackgroundRefreshTarget::One(store) = &target {
        command.arg("--store").arg(store);
    }
    if let Some(agent) = &context.as_agent {
        command.arg("--as-agent").arg(agent);
    }
    command
        .arg("refresh")
        .arg("--quiet")
        .arg("--background")
        .env("HIVE_MEMORY_HOOK_ACTIVE", "1")
        .env_remove("HIVE_MEMORY_BACKGROUND_REFRESH")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if command.spawn().is_err() {
        let _ = std::fs::remove_file(stamp);
    }
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
    let tracker = DeliveryTracker::start(&config, &args.delivery)?;
    let session_id = match &tracker {
        Some(tracker) => Some(tracker.session_id.clone()),
        None => hook_session_id(&mut warnings),
    };

    let path_hint = args
        .project
        .or_else(|| std::env::var("HIVE_MEMORY_PROJECT").ok());
    // Validate the same read policy used by possible context refresh below.
    // Prompt hooks should fail inside `hm` when project affinity is outside
    // agent policy, not leave shell adapters to discover that later.
    let periodic_refresh_store =
        validate_hook_context_read_policy(&config, &context, path_hint.as_deref())?;
    let prepared = prepare_prompt_recall(
        &config,
        &context,
        path_hint.as_deref(),
        session_id.as_deref(),
        tracker.as_ref().map(|tracker| &tracker.state),
        &args.text,
    )?;
    let mut context_emitted = false;
    let mut deferred = None;
    let mut delivery = None;
    let (recall, refresh_store) = if let Some(tracker) = &tracker {
        let (selection, recall, refresh_store) =
            tracked_prompt_selection(&config, &context, path_hint.as_deref(), tracker, &prepared)?;
        let mut report = selection.report;
        let context_transition = selection.update.context_key.is_some();
        if !selection.body.is_empty() {
            actions.push(HookAction::new("inject_context", selection.body));
            context_emitted = true;
            report.emitted_ids = selection.emitted_ids;
        }
        if context_emitted || context_transition {
            deferred = Some(selection.update);
        }
        let recall = Some(recall);
        delivery = Some(report);
        (recall, refresh_store)
    } else {
        let (context_action, refresh_store) = hook_context_action_if_changed(
            &config,
            &context,
            path_hint.as_deref(),
            session_id.as_deref(),
            &prepared,
            args.delivery.max_context_bytes,
        )?;
        if let Some(action) = context_action {
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
                &prepared,
                args.delivery.max_context_bytes,
            )?;
            if let Some(action) = action {
                context_emitted = true;
                actions.push(action);
            }
            Some(report)
        };
        (recall, refresh_store)
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
        delivery,
    };
    emit_hook_response(&response, args.json)?;
    // Applied only after the response is written, so a failed write never
    // marks anything delivered.
    if let (Some(tracker), Some(update)) = (&tracker, deferred) {
        tracker.apply_now(update);
    }
    maybe_spawn_background_refresh(
        &config,
        &context,
        BackgroundRefreshTarget::One(refresh_store.unwrap_or(periodic_refresh_store)),
        None,
    );

    Ok(())
}

/// Record that the host lost earlier context and start a new epoch.
///
/// Adapters call this from the host's context-loss events (for example
/// `PreCompact`). Afterwards nothing is suppressed as already visible, and the
/// next context-emitting hook sends the session's context again.
fn run_context_reset(args: HookResetArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let mut warnings = Vec::new();
    if let Some(session_id) = hook_session_id(&mut warnings) {
        memory_hook::reset_context_epoch(&config.state_dir, &session_id, &hook_options(&config))?;
    }
    let response = HookResponse {
        event: "context-reset",
        actions: Vec::new(),
        warnings,
        memory_pending: false,
        context_emitted: false,
        refresh: None,
        recall: None,
        delivery: None,
    };
    emit_hook_response(&response, args.json)
}

/// Consume session write receipts after a successful tool event.
///
/// Tool hooks are the first point where we can know whether the agent actually
/// ran `hm remember`/`hm note` after a prompt reminder. Receipts provide that
/// proof without parsing shell commands or trusting hook-side classifiers.
fn run_tool_complete(args: HookToolCompleteArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let mut warnings = Vec::new();
    let actions = Vec::new();
    let refresh = None;

    let session_id = hook_session_id(&mut warnings);
    let mut memory_pending = if let Some(session_id) = session_id.as_deref() {
        memory_hook::load_state(&config.state_dir, session_id)?.memory_pending
    } else {
        false
    };
    let context_emitted = false;
    let mut refresh_target = None;
    let mut refresh_request = None;

    if args.status == 0
        && let Some(session_id) = session_id.as_deref()
    {
        let receipts = memory_hook::load_write_receipts(&config.state_dir, session_id)?;
        let mut state = memory_hook::load_state(&config.state_dir, session_id)?;
        let unrefreshed_receipts = receipts.len().saturating_sub(state.refreshed_receipts);

        if unrefreshed_receipts > 0 {
            let stores = receipts
                .iter()
                .skip(state.refreshed_receipts)
                .map(|receipt| receipt.store.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            refresh_target = Some(if stores.len() == 1 {
                BackgroundRefreshTarget::One(stores.first().expect("one receipt store").to_string())
            } else {
                BackgroundRefreshTarget::All
            });
            refresh_request = Some((session_id, receipts.len()));
            state = memory_hook::mark_memory_satisfied(
                &config.state_dir,
                session_id,
                &hook_options(&config),
            )?;
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
        delivery: None,
    };
    emit_hook_response(&response, args.json)?;
    if let Some(target) = refresh_target {
        maybe_spawn_background_refresh(&config, &context, target, refresh_request);
    }

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
        delivery: None,
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
    /// Delivery-tracking diagnostics; present only with `--delivery`.
    #[serde(skip_serializing_if = "Option::is_none")]
    delivery: Option<HookDeliveryReport>,
}

#[derive(Debug, Serialize)]
struct HookDeliveryReport {
    /// Host context lifetime this hook was told: `persistent` or `turn`.
    lifetime: &'static str,
    /// Whether this hook started a new context epoch first.
    reset: bool,
    /// Memory ids in the emitted context.
    emitted_ids: Vec<String>,
    /// Memories skipped because they are already visible in this epoch.
    skipped_delivered: usize,
    /// Memories dropped to fit the host's size limit.
    skipped_over_budget: usize,
    /// Memories too large for the limit, emitted as pointer blocks.
    omitted: usize,
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

/// Prompt topic prepared before any delivery mode chooses context or recall.
///
/// A prompt can emit only normal context (not recall), especially on turn-lived
/// hosts. Preparing once keeps that path from bypassing topic changes or using
/// a stale topic merely because recall did not run.
struct PreparedPromptRecall {
    query: Option<String>,
    continuation: bool,
    visible: RecallKnown,
    store_name: String,
    project_id: Option<String>,
}

fn prepare_prompt_recall(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
    session_id: Option<&str>,
    state: Option<&memory_hook::HookState>,
    prompt: &str,
) -> Result<PreparedPromptRecall> {
    let project_id = resolve_project_id(None, path_hint)?;
    let project_binding = project_binding_store(config, project_id.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let resolved = resolve_store(
        config,
        context.store.as_deref(),
        project_binding.as_deref(),
        agent_id.as_deref(),
        StoreAccess::Read,
    )?;
    let store_name = resolved.name;
    let store_key = index::store_cache_key(&store_name, &config.stores[&store_name].root);
    let tracked = state.is_some();
    let loaded;
    let state = if let Some(state) = state {
        state
    } else {
        loaded = match session_id {
            Some(session_id) => memory_hook::load_state(&config.state_dir, session_id)?,
            None => memory_hook::HookState::default(),
        };
        &loaded
    };
    let visible = if tracked {
        RecallKnown::Tracked {
            delivered: state.delivered.clone(),
            recalled: std::collections::BTreeSet::new(),
            selection: state.turn_context_ids.iter().cloned().collect(),
        }
    } else {
        RecallKnown::Ids(memory_hook::known_session_memory_ids(state))
    };
    let previous = state.recall_topic.as_ref().filter(|topic| {
        topic.is_valid() && topic.store_key == store_key && topic.project_id == project_id
    });
    let (mut query, continuation) = match prompt_query(prompt, path_hint) {
        PromptQuery::Subject(query) => (Some(query), false),
        PromptQuery::Generic => (previous.map(|topic| topic.query.clone()), true),
        // An explicit subject outside the bounded representation must not turn
        // into a continuation of an unrelated previous subject.
        PromptQuery::Unusable => (None, false),
    };
    let topic = query.as_ref().map(|query| memory_hook::RecallTopic {
        query: query.clone(),
        store_key,
        project_id: project_id.clone(),
    });
    if topic != state.recall_topic
        && let Some(session_id) = session_id
    {
        let updated = memory_hook::set_recall_topic(
            &config.state_dir,
            session_id,
            state.context_epoch,
            topic,
            &hook_options(config),
        )?;
        if updated.context_epoch != state.context_epoch {
            query = None;
        }
    }
    Ok(PreparedPromptRecall {
        query,
        continuation,
        visible,
        store_name,
        project_id,
    })
}

/// Qualify prompt-time context before delivery dedupe or size packing.
///
/// SessionStart deliberately keeps its broad stable context. A later prompt
/// must not drain unrelated sections that merely missed the startup byte cap.
/// Use the same lexical/entity evidence as recall; context has no BM25 result
/// proving an anchored partial match, so it cannot use that fallback.
fn prompt_context_output(
    config: &Config,
    context: &CliContext,
    output: &memory_context::ContextOutput,
    context_key: String,
    prepared: &PreparedPromptRecall,
) -> Result<Option<(memory_context::ContextOutput, String)>> {
    let filtered_key = format!(
        "{context_key}:prompt:{}",
        hive_memory::hash::sha256_hex(prepared.query.as_deref().unwrap_or_default().as_bytes())
    );
    let empty = || {
        let mut output = output.clone();
        output.sections.clear();
        output.markdown.clear();
        output.estimated_tokens = 0;
        Some((output, filtered_key.clone()))
    };
    let Some(query) = prepared.query.as_deref() else {
        // Even a subjectless project visit ends the previous turn selection.
        // Record a state-only transition without completing the broad startup
        // key, so a later concrete topic can still recover withheld memories.
        return Ok(empty());
    };
    let mut candidates = Vec::new();
    let stores = output
        .sections
        .iter()
        .map(|section| section.store.as_str())
        .chain(std::iter::once(prepared.store_name.as_str()))
        .collect::<std::collections::BTreeSet<_>>();
    for store in stores {
        let PromptRecallIndex::Indexed(report) = load_prompt_recall_index(config, store)? else {
            continue;
        };
        let registry = &report.projection.entity_registry;
        let evidence = search::RecallQuery::parse(query, registry)?;
        let sources = &config.defaults.search_sources;
        let agent_id = resolve_agent_id(context.as_agent.clone());
        let include_inbox = super::search::search_include_inbox(false, sources);
        let superseded = memory_context::explicitly_superseded_ids(
            &report.projection.entries,
            sources,
            include_inbox,
            agent_id.as_deref(),
        );
        let eligible = search::search_local_index(
            search::SearchInput {
                store_root: &config.stores[store].root,
                entries: &report.projection.entries,
                query,
                scopes: &config.defaults.search_scopes,
                sources,
                include_inbox,
                agent_id: agent_id.as_deref(),
                project_id: prepared.project_id.as_deref(),
                limit: usize::MAX,
            },
            registry,
            &report.projection.project_aliases,
        )?
        .into_iter()
        .filter(|hit| !superseded.contains(&hit.entry.id))
        .map(|hit| hit.entry.id)
        .chain(
            output
                .sections
                .iter()
                .filter(|section| section.store == store)
                .map(|section| section.id.clone()),
        )
        .collect::<std::collections::BTreeSet<_>>();
        // The normal assembly and recall search own their respective read
        // policies. Only candidates eligible on one of those surfaces may set
        // the bar; a forbidden or expired lexical match must not suppress a
        // legitimate entity-alias memory.
        for entry in report
            .projection
            .entries
            .iter()
            .filter(|entry| eligible.contains(&entry.id))
        {
            let support = evidence.evidence(entry);
            if support >= search::RecallEvidence::Entity {
                candidates.push((
                    store.to_owned(),
                    entry.id.clone(),
                    support,
                    prepared.visible.visible(entry),
                ));
            }
        }
    }
    // Include recall-only references and already-visible records in the bar.
    // Their absence from normal context must not promote a host-only match.
    let best = candidates.iter().map(|(_, _, support, _)| *support).max();
    if prepared.continuation
        && candidates
            .iter()
            .any(|(_, _, support, visible)| Some(*support) == best && *visible)
    {
        // Context backfill must obey the same continuation rule as recall.
        // Otherwise it can drain the unseen tail after recall abstains.
        return Ok(empty());
    }
    let supported = candidates
        .into_iter()
        .filter(|(_, _, support, _)| Some(*support) == best)
        .map(|(store, id, _, _)| (store, id))
        .collect::<std::collections::BTreeSet<_>>();
    let keep = |section: &memory_context::ContextSection| {
        supported.contains(&(section.store.clone(), section.id.clone()))
    };
    if !output.sections.iter().any(keep) {
        return Ok(empty());
    }
    let Some(subset) = output.select_sections(keep, None) else {
        // Unsplittable output cannot prove that every emitted fact is relevant.
        return Ok(None);
    };
    if subset.filtered == 0 {
        return Ok(Some((subset.output, context_key)));
    }
    // Completing a filtered subset must not complete the broad startup key:
    // another explicit topic can still need one of the withheld sections.
    Ok(Some((subset.output, filtered_key)))
}

fn hook_context_action_if_changed(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
    session_id: Option<&str>,
    prepared: &PreparedPromptRecall,
    max_bytes: Option<usize>,
) -> Result<(Option<HookAction>, Option<String>)> {
    let Some(session_id) = session_id else {
        return Ok((None, None));
    };

    // Long-lived agents can move between projects while the process stays
    // alive. Cache the resolved selection, not just "context was already sent",
    // so hooks can reinject when path/project/store policy changes.
    let context_key = hook_context_key_for_project(config, context, None, path_hint)?;
    let state = memory_hook::load_state(&config.state_dir, session_id)?;
    if state.context_key.as_deref() == Some(context_key.as_str()) {
        return Ok((None, None));
    }

    let assembly = assemble_hook_context(config, context, None, path_hint)?;
    let refresh_store = assembly
        .stale
        .then(|| assembly.stores.first().cloned())
        .flatten();
    if assembly.stale && assembly.output.sections.is_empty() {
        return Ok((None, refresh_store));
    }
    let Some((output, context_key)) =
        prompt_context_output(config, context, &assembly.output, context_key, prepared)?
    else {
        return Ok((None, refresh_store));
    };
    if state.context_key.as_deref() == Some(context_key.as_str()) {
        return Ok((None, refresh_store));
    }
    if output.markdown.is_empty() && output.sections.is_empty() {
        // A relevance-filtered visit still changes the active selection. Its
        // empty body does not deliver memory or replace the ids already sent.
        memory_hook::mark_context_key(
            &config.state_dir,
            session_id,
            context_key,
            &hook_options(config),
        )?;
        return Ok((None, refresh_store));
    }
    let output = fit_legacy_output(&output, max_bytes);
    if max_bytes.is_some() && output.markdown.is_empty() {
        return Ok((None, refresh_store));
    }
    memory_hook::mark_startup_context(
        &config.state_dir,
        session_id,
        context_key,
        ids(&output),
        &hook_options(config),
    )?;

    Ok((
        Some(HookAction::new("inject_context", output.markdown)),
        refresh_store,
    ))
}

fn assemble_hook_context(
    config: &Config,
    context: &CliContext,
    project_id: Option<String>,
    path_hint: Option<&str>,
) -> Result<super::context::CliContextAssembly> {
    super::context::assemble_cli_context(
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
    )
}

/// Tracked counterpart of [`hook_context_action_if_changed`].
///
/// Returns the selection for a changed context key with its bookkeeping, or
/// `None` when the key is unchanged. Memories already visible in this epoch
/// are skipped, so returning to an earlier project re-sends nothing the model
/// still has. A reset clears the key, so the next prompt re-sends context.
fn tracked_context_selection(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
    tracker: &DeliveryTracker,
    prepared: &PreparedPromptRecall,
    in_block: &[memory_context::ContextSection],
    max_bytes: Option<usize>,
) -> Result<(Option<TrackedEmission>, Option<String>)> {
    let context_key = hook_context_key_for_project(config, context, None, path_hint)?;
    if tracker.state.delivered_context_key.as_deref() == Some(context_key.as_str()) {
        return Ok((None, None));
    }
    let assembly = assemble_hook_context(config, context, None, path_hint)?;
    let refresh_store = assembly
        .stale
        .then(|| assembly.stores.first().cloned())
        .flatten();
    if assembly.stale && assembly.output.sections.is_empty() {
        return Ok((None, refresh_store));
    }
    let Some((output, context_key)) =
        prompt_context_output(config, context, &assembly.output, context_key, prepared)?
    else {
        return Ok((None, refresh_store));
    };
    if tracker.state.delivered_context_key.as_deref() == Some(context_key.as_str()) {
        return Ok((None, refresh_store));
    }
    let in_block = in_block
        .iter()
        .map(|section| (section.id.as_str(), section.body_digest()))
        .collect::<Vec<_>>();
    // "Too large" is judged against the hook's whole limit: a memory that only
    // misses the space left after recall waits for the next prompt.
    let mut selected = select_tracked(
        &output,
        assembly.stale,
        |section| {
            tracker.is_delivered(section)
                || in_block
                    .iter()
                    .any(|(id, digest)| *id == section.id && *digest == section.body_digest())
        },
        tracker.limit(max_bytes),
    );
    // In-block recall duplicates consume no extra space, but they are not
    // already-visible memories. Keep the public delivery count precise.
    selected.filtered = output
        .sections
        .iter()
        .filter(|section| tracker.is_delivered(section))
        .count();
    if selected.output.sections.is_empty() && selected.omitted == 0 {
        // A relevant selection wholly visible already (or in this prompt's
        // recall block) adds no memory. Do not emit a second header-only block.
        selected.emitted = false;
        selected.output.markdown.clear();
    }
    let mut update = tracker.update(&selected, Some(context_key.clone()), None);
    if !selected.emitted && selected.over_budget == 0 {
        // Every relevant section is already visible or carried by this same
        // response's recall. A legacy fallback must observe that completed
        // selection too, even though no extra context body was necessary.
        update.untracked_context_key = Some(context_key);
        update.untracked_context_ids = ids(&output);
    }
    if tracker.lifetime == memory_hook::DeliveryLifetime::Turn && !selected.emitted {
        // Visiting a selection with no new visible sections still ends the
        // previous turn's selection. Returning to it later must send it again.
        update.turn_context_ids = Some(Vec::new());
    }
    Ok((Some((selected, update)), refresh_store))
}

/// Context and recall a tracked prompt-submit emits as one block.
struct TrackedPromptSelection {
    body: String,
    emitted_ids: Vec<String>,
    update: memory_hook::DeliveryUpdate,
    report: HookDeliveryReport,
}

/// Select a tracked prompt-submit response.
///
/// Persistent hosts run prompt recall first, on every prompt, as untracked
/// hooks do: prompt-specific memory takes precedence over sending the rest of
/// a context selection, which fills the remaining budget and skips what recall
/// carries. Turn-lifetime hosts follow the untracked order exactly: a changed
/// selection is sent instead of recall, once per change and fitted to the
/// limit, and recall runs on the other prompts.
fn tracked_prompt_selection(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
    tracker: &DeliveryTracker,
    prepared: &PreparedPromptRecall,
) -> Result<(TrackedPromptSelection, HookRecallReport, Option<String>)> {
    let mut selection = TrackedPromptSelection {
        body: String::new(),
        emitted_ids: Vec::new(),
        update: memory_hook::DeliveryUpdate {
            epoch: tracker.state.context_epoch,
            ..memory_hook::DeliveryUpdate::default()
        },
        report: tracker.report(None),
    };
    if tracker.lifetime == memory_hook::DeliveryLifetime::Turn {
        let (context_selection, refresh_store) = tracked_context_selection(
            config,
            context,
            path_hint,
            tracker,
            prepared,
            &[],
            tracker.max_bytes,
        )?;
        if let Some((selected, update)) = context_selection {
            selection.report = tracker.report(Some(&selected));
            selection.update = update;
            if selected.emitted {
                selection.body = selected.output.markdown.clone();
                selection.emitted_ids = ids(&selected.output);
                return Ok((
                    selection,
                    HookRecallReport::skipped("context-selection-changed"),
                    refresh_store,
                ));
            }
        }
        let (recalled, recall) = tracked_prompt_recall(
            config,
            context,
            path_hint,
            tracker,
            prepared,
            tracker.max_bytes,
        )?;
        if let Some((recalled, update)) = recalled {
            selection.report.skipped_over_budget += recalled.over_budget;
            selection.report.omitted += recalled.omitted;
            selection.body = recalled.output.markdown.clone();
            selection.emitted_ids = ids(&recalled.output);
            merge_recall_update(&mut selection.update, update);
        }
        return Ok((selection, recall, refresh_store));
    }

    let (recalled, recall) = tracked_prompt_recall(
        config,
        context,
        path_hint,
        tracker,
        prepared,
        tracker.max_bytes,
    )?;
    let recall_sections = recalled
        .as_ref()
        .map(|(selected, _)| selected.output.sections.clone())
        .unwrap_or_default();
    let recall_len = recalled.as_ref().map_or(0, |(selected, _)| {
        tracker.measure.of(&selected.output.markdown) + tracker.measure.of("\n")
    });
    let (context_selection, refresh_store) = tracked_context_selection(
        config,
        context,
        path_hint,
        tracker,
        prepared,
        &recall_sections,
        tracker.max_bytes.map(|max| max.saturating_sub(recall_len)),
    )?;
    if let Some((selected, update)) = context_selection {
        selection.report = tracker.report(Some(&selected));
        selection.update = update;
        if selected.emitted {
            selection.body.push_str(&selected.output.markdown);
            selection.emitted_ids.extend(ids(&selected.output));
        }
    }
    if let Some((recalled, update)) = recalled {
        if !selection.body.is_empty() {
            selection.body.push('\n');
        }
        selection.body.push_str(&recalled.output.markdown);
        selection.emitted_ids.extend(ids(&recalled.output));
        merge_recall_update(&mut selection.update, update);
        selection.report.skipped_over_budget += recalled.over_budget;
        selection.report.omitted += recalled.omitted;
    }
    Ok((selection, recall, refresh_store))
}
/// Combine recall bookkeeping without losing a state-only context transition.
fn merge_recall_update(
    target: &mut memory_hook::DeliveryUpdate,
    recall: memory_hook::DeliveryUpdate,
) {
    target.memories.extend(recall.memories);
    target.prompt_recall_key = recall.prompt_recall_key;
    target.untracked_recall_key = recall.untracked_recall_key;
    target.untracked_recall_ids = recall.untracked_recall_ids;
    target.turn_recall_ids.extend(recall.turn_recall_ids);
}

/// Memories a prompt recall must not repeat.
enum RecallKnown {
    /// Legacy: ids emitted anywhere in the session.
    Ids(std::collections::BTreeSet<String>),
    /// Tracked: memories visible in the current epoch, plus ids turn recall
    /// already emitted this epoch (gone from view, but recall moves past
    /// them the way untracked recall does) and the ids of the last turn
    /// context-change block (skipped like untracked recall skips the
    /// current selection).
    Tracked {
        delivered: Vec<memory_hook::DeliveredMemory>,
        recalled: std::collections::BTreeSet<String>,
        selection: std::collections::BTreeSet<String>,
    },
}

impl RecallKnown {
    fn len(&self) -> usize {
        match self {
            Self::Ids(ids) => ids.len(),
            Self::Tracked {
                delivered,
                recalled,
                selection,
            } => delivered.len() + recalled.len() + selection.len(),
        }
    }

    /// Whether recall should move past `entry`.
    fn contains(&self, entry: &index::IndexEntry) -> bool {
        match self {
            Self::Ids(ids) => ids.contains(&entry.id),
            Self::Tracked { recalled, .. } => recalled.contains(&entry.id) || self.visible(entry),
        }
    }

    /// Whether `entry` is still in the model's view or the current
    /// selection, so repeating it is never useful.
    fn visible(&self, entry: &index::IndexEntry) -> bool {
        match self {
            Self::Ids(ids) => ids.contains(&entry.id),
            Self::Tracked {
                delivered,
                selection,
                ..
            } => {
                selection.contains(&entry.id)
                    || delivered.iter().any(|memory| memory.id == entry.id)
                        && delivered.contains(&memory_hook::DeliveredMemory {
                            id: entry.id.clone(),
                            digest: memory_context::raw_body_digest(&entry.body),
                        })
            }
        }
    }
}

/// Session facts prompt recall reads, loaded only once recall reaches them.
struct RecallSession {
    known: RecallKnown,
    /// Previous recall selection; a repeat of it is skipped. `None` disables
    /// that check (turn-lifetime context is gone by the next prompt).
    previous_key: Option<String>,
}

/// One prompt recall selection, before any session bookkeeping.
struct RecallSelection {
    output: memory_context::ContextOutput,
    recall_key: String,
    /// Selected memories dropped by the size limit.
    over_budget: usize,
    /// Selected memories too large for the limit, shown as pointers.
    omitted: Vec<String>,
}

fn hook_prompt_recall_action(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
    session_id: Option<&str>,
    prepared: &PreparedPromptRecall,
    max_bytes: Option<usize>,
) -> Result<(Option<HookAction>, HookRecallReport)> {
    let Some(session_id) = session_id else {
        return Ok((None, HookRecallReport::skipped("no-session-id")));
    };
    let (selection, mut recall) = select_prompt_recall(
        config,
        context,
        path_hint,
        prepared,
        max_bytes.map(memory_context::SizeLimit::bytes),
        || {
            let state = memory_hook::load_state(&config.state_dir, session_id)?;
            Ok(RecallSession {
                known: RecallKnown::Ids(memory_hook::known_session_memory_ids(&state)),
                previous_key: state.prompt_recall_key,
            })
        },
    )?;
    let Some(selection) = selection else {
        return Ok((None, recall));
    };
    let started = Instant::now();
    let emitted_ids = ids(&selection.output);
    memory_hook::mark_prompt_recall(
        &config.state_dir,
        session_id,
        selection.recall_key,
        emitted_ids,
        &hook_options(config),
    )?;
    recall.retrieval_ms += started.elapsed().as_millis();

    Ok((
        Some(HookAction::new("inject_context", selection.output.markdown)),
        recall,
    ))
}

/// Tracked prompt recall: skips memories visible in this epoch and returns
/// bookkeeping instead of writing it. `max_bytes` is the size budget for
/// recall.
fn tracked_prompt_recall(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
    tracker: &DeliveryTracker,
    prepared: &PreparedPromptRecall,
    max_bytes: Option<usize>,
) -> Result<(Option<TrackedEmission>, HookRecallReport)> {
    let (selection, recall) = select_prompt_recall(
        config,
        context,
        path_hint,
        prepared,
        tracker.limit(max_bytes),
        || {
            Ok(RecallSession {
                known: RecallKnown::Tracked {
                    delivered: tracker.state.delivered.clone(),
                    recalled: tracker.state.turn_recall_ids.iter().cloned().collect(),
                    selection: tracker.state.turn_context_ids.iter().cloned().collect(),
                },
                previous_key: match tracker.lifetime {
                    memory_hook::DeliveryLifetime::Persistent => {
                        tracker.state.delivered_recall_key.clone()
                    }
                    memory_hook::DeliveryLifetime::Turn => None,
                },
            })
        },
    )?;
    let Some(selection) = selection else {
        return Ok((None, recall));
    };
    let selected = TrackedSelection {
        output: selection.output,
        emitted: true,
        filtered: 0,
        over_budget: selection.over_budget,
        omitted: selection.omitted.len(),
    };
    let mut update = tracker.update(&selected, None, Some(selection.recall_key));
    if tracker.lifetime == memory_hook::DeliveryLifetime::Turn {
        // Untracked recall counts every hit it showed, pointers included, so
        // a later prompt moves on to the next hits; turn recall does too.
        update.turn_recall_ids = ids(&selected.output);
        update.turn_recall_ids.extend(selection.omitted);
    }
    Ok((Some((selected, update)), recall))
}

fn select_prompt_recall<F>(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
    prepared: &PreparedPromptRecall,
    limit: Option<memory_context::SizeLimit>,
    load_session: F,
) -> Result<(Option<RecallSelection>, HookRecallReport)>
where
    F: FnOnce() -> Result<RecallSession>,
{
    let started = Instant::now();
    let Some(query) = prepared.query.as_deref() else {
        return Ok((None, HookRecallReport::skipped("budget-empty")));
    };
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let project_id = &prepared.project_id;
    let store_name = &prepared.store_name;
    let store_config = &config.stores[store_name.as_str()];
    let session = load_session()?;
    // Prompt recall is an automatic `hm search`, so it follows the same source
    // defaults. Raw inbox material remains opt-in through that policy.
    let sources = &config.defaults.search_sources;
    let cached_report = match load_prompt_recall_index(config, store_name)? {
        PromptRecallIndex::Indexed(report) => report,
        PromptRecallIndex::Skip(reason) => {
            let mut recall = HookRecallReport::skipped(reason);
            recall.retrieval_ms = started.elapsed().as_millis();
            return Ok((None, recall));
        }
    };
    let entries = cached_report.projection.entries.as_slice();
    let include_inbox = super::search::search_include_inbox(false, sources);
    // Recall injects its hits into the agent, so explicit corrections hide
    // their targets the way `hm context` does, not the way interactive search
    // does. Search resolves supersession only among query-matched hits and
    // keeps an old fact when the query names a word only that fact contains,
    // which a query built from prompt wording does by accident: a prompt about
    // pushing a branch re-injected a corrected workflow memory because only the
    // old text said "branch", and its correction, already visible in the
    // session, was skipped before rendering could resolve the link. Explicit
    // `hm search` keeps that historical exception.
    let superseded = memory_context::explicitly_superseded_ids(
        entries,
        sources,
        include_inbox,
        agent_id.as_deref(),
    );
    let search_input = search::SearchInput {
        store_root: &store_config.root,
        entries,
        query,
        scopes: &config.defaults.search_scopes,
        sources,
        include_inbox,
        agent_id: agent_id.as_deref(),
        project_id: project_id.as_deref(),
        // Search beyond memories already emitted in this session. Applying the
        // old fixed cap before session dedupe permanently hid rank 11+ after the
        // first few prompts even though those memories remained eligible.
        // Superseded hits are dropped below, so search past those as well.
        limit: session
            .known
            .len()
            .saturating_add(superseded.len())
            .saturating_add(3)
            .max(10),
    };
    // Prefer BM25 recall when the persistent index is already fresh; this is
    // where the prompt hook gains paraphrase/multi-session recall. Fall back to
    // the lexical scan when the index is stale/absent so the hook never pays for
    // a rebuild on its latency budget (refresh/tool-complete keeps it fresh).
    let registry = &cached_report.projection.entity_registry;
    let project_aliases = &cached_report.projection.project_aliases;
    let mut hits = match super::search::tantivy_search_local_if_fresh(
        config,
        store_name,
        &cached_report.entries,
        search_input.clone(),
        registry,
        project_aliases,
    ) {
        Some(hits) => hits,
        None => match search::search_local_index(search_input, registry, project_aliases) {
            Ok(hits) => hits,
            Err(search::SearchError::EmptyQuery) => {
                return Ok((None, HookRecallReport::skipped("budget-empty")));
            }
            Err(err) => {
                let mut recall = HookRecallReport::skipped("index-unavailable");
                recall.retrieval_ms = started.elapsed().as_millis();
                eprintln!("warning: local prompt recall skipped: {err}");
                return Ok((None, recall));
            }
        },
    };

    hits.retain(|hit| !superseded.contains(&hit.entry.id));

    let evidence = search::RecallQuery::parse(query, registry)?;
    let support = |hit: &search::SearchHit| {
        let support = evidence.evidence(&hit.entry);
        if support == search::RecallEvidence::Anchored && hit.trace.retrieval == 0 {
            search::RecallEvidence::None
        } else {
            support
        }
    };
    let best = hits
        .iter()
        .map(support)
        .max()
        .unwrap_or(search::RecallEvidence::None);
    // Qualify the complete candidate set BEFORE excluding visible memories.
    // A lexical answer outranks incidental host/entity overlap, and a useful
    // anchored BM25 paraphrase never becomes an endless novelty fallback.
    hits.retain(|hit| best > search::RecallEvidence::None && support(hit) == best);
    if prepared.continuation && hits.iter().any(|hit| session.known.visible(&hit.entry)) {
        // A conversational followup supplies no new subject. Once the best
        // evidence is visible, moving down the unseen tail spends context on
        // weaker interpretations of the same query. Turn-only recall ids do
        // not count as visible, so an expired turn can still recover its facts.
        let mut recall = HookRecallReport::skipped("topic-already-visible");
        recall.candidate_count = hits.len();
        recall.retrieval_ms = started.elapsed().as_millis();
        return Ok((None, recall));
    }

    // Incremental prompt recall emits at most three NEW memories at a time. The
    // search above still considers the complete local projection and expands
    // past memories already seen in this session; this cap bounds one hook
    // payload rather than limiting which memories remain searchable, and later
    // prompts can advance to the next eligible results.
    let pick = |skip: &dyn Fn(&index::IndexEntry) -> bool| {
        hits.iter()
            .filter(|hit| !skip(&hit.entry))
            .take(3)
            .map(|hit| hit.entry.clone())
            .collect::<Vec<_>>()
    };
    let mut selected_entries = pick(&|entry| session.known.contains(entry));
    if selected_entries.is_empty() {
        // Turn recall already showed every hit this epoch. Its context is
        // gone after each turn, so repeat the top hits rather than nothing.
        selected_entries = pick(&|entry| session.known.visible(entry));
    }
    let selected_ids = selected_entries
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();
    let recall_key = prompt_recall_key(query, project_id.as_deref(), store_name, &selected_ids);
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
    if session.previous_key.as_deref() == Some(recall_key.as_str()) {
        recall.reason = "unchanged";
        recall.reused_previous = true;
        return Ok((None, recall));
    }

    // Prompt-submit runs synchronously on every turn, so its incremental context
    // uses an independently bounded per-prompt envelope. The floor leaves room
    // for one useful section and the ceiling bounds per-prompt context churn;
    // neither changes the configured budget used by other context surfaces.
    let max_tokens = usize::try_from(config.defaults.hook_context_max_tokens)?.clamp(200, 1_200);
    // With a host size limit, render every selected hit (at most three) and
    // let `select_sections` pack them: an oversized top hit then becomes a
    // pointer instead of stopping assembly before the hits below it. The
    // per-prompt envelope stays in raw bytes, as untracked recall counts it;
    // only the host limit uses the host's measure.
    let (assembly_tokens, cap) = match limit {
        Some(limit) => (
            usize::MAX / 8,
            Some(memory_context::SizeLimit {
                max: limit.max,
                alone_max: limit.max,
                measure: limit.measure,
                raw_max: max_tokens.saturating_mul(4),
            }),
        ),
        None => (max_tokens, None),
    };
    let context_input = memory_context::ContextInput {
        store_name: store_name.as_str(),
        store_root: &store_config.root,
        entries: &selected_entries,
        scopes: &config.defaults.search_scopes,
        sources,
        include_inbox,
        include_search_only: true,
        agent_id: agent_id.as_deref(),
        project_id: project_id.as_deref(),
        path_hint,
        max_tokens: assembly_tokens,
        inject_strategy: inject::Strategy::from_config(&config.defaults.context_strategy),
        explain: false,
    };
    let output = memory_context::assemble_selected_local_index_context(
        context_input,
        &cached_report.projection.project_aliases,
    )?;
    let (output, over_budget, omitted) =
        match cap.and_then(|cap| output.select_sections(|_| true, Some(cap))) {
            Some(subset) => (subset.output, subset.over_budget, subset.omitted),
            None => (output, 0, Vec::new()),
        };
    if output.sections.is_empty() && omitted.is_empty() {
        recall.reason = "below-threshold";
        recall.selected_count = 0;
        recall.selected_ids.clear();
        return Ok((None, recall));
    }

    let emitted_ids = ids(&output);
    recall.selected_count = emitted_ids.len();
    recall.selected_ids = emitted_ids;
    recall.retrieval_ms = started.elapsed().as_millis();

    Ok((
        Some(RecallSelection {
            output,
            recall_key,
            over_budget,
            omitted,
        }),
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
    hive_memory::hash::lower_hex(&digest.finalize())
}

/// Whether a prompt supplies a usable subject or truly continues the last one.
enum PromptQuery {
    Generic,
    Subject(String),
    Unusable,
}

#[cfg(test)]
fn prompt_recall_query(prompt: &str, path_hint: Option<&str>) -> Option<String> {
    match prompt_query(prompt, path_hint) {
        PromptQuery::Subject(query) => Some(query),
        PromptQuery::Generic | PromptQuery::Unusable => None,
    }
}

fn prompt_query(prompt: &str, path_hint: Option<&str>) -> PromptQuery {
    // Normalize only after the shared detectors inspect the original spelling:
    // lowercasing hides AWS key shapes, and punctuation removal can hide an
    // assignment. Unusable clears an old topic rather than inheriting it.
    if !hive_memory::secret::detect(prompt).is_empty() {
        return PromptQuery::Unusable;
    }
    let mut code_terms = Vec::new();
    let mut plain_terms = Vec::new();
    // Request constraints after "while" describe how to do the work, rather
    // than a new recall subject. Keep those words out of the saved topic.
    let lower = prompt
        .to_ascii_lowercase()
        .replace('’', "'")
        .replace("i don't understand", " ")
        .replace("i do not understand", " ");
    let subject = lower
        .split_once(" while ")
        .map_or(lower.as_str(), |(head, _)| head);
    let normalized = subject
        .split(|ch: char| {
            ch.is_whitespace() || matches!(ch, ',' | ';' | ':' | '(' | ')' | '[' | ']')
        })
        .map(normalize_prompt_token)
        .filter(|token| !token.is_empty())
        .filter(|token| !is_prompt_stopword(token) && !is_task_intent_term(token))
        .collect::<Vec<_>>()
        .join(" ");
    for token in search::query_terms(&normalized) {
        if is_code_prompt_term(&token) {
            push_unique(&mut code_terms, token);
        } else if token.len() >= 2 && token.chars().any(char::is_alphabetic) {
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

    // This is a bounded relevance sketch for one automatic recall, not a cap on
    // the indexed memory corpus. Concrete code/path terms are more discriminating
    // than task prose, so prefer up to three of them and fill a four-term query
    // with plain terms. Plain-language subjects get the same four-term budget:
    // keeping only their first two words discarded concrete subjects in favor
    // of framing such as "another" or "whether".
    // Manual search and the normal context surfaces keep their existing inputs.
    let terms = if !code_terms.is_empty() {
        let mut terms = code_terms.into_iter().take(3).collect::<Vec<_>>();
        terms.extend(
            plain_terms
                .into_iter()
                .take(4usize.saturating_sub(terms.len())),
        );
        terms
    } else {
        plain_terms.into_iter().take(4).collect::<Vec<_>>()
    };
    if terms.is_empty()
        || terms.iter().all(|term| {
            matches!(
                term.as_str(),
                "pr" | "pull" | "request" | "commit" | "branch" | "live" | "ready"
            )
        })
    {
        // Workflow-only followups describe the next action on the current
        // subject. A concrete qualifier, such as "PR watch", remains a topic.
        return PromptQuery::Generic;
    }
    let mut query = String::new();
    for term in terms {
        if query.len() + term.len() + usize::from(!query.is_empty())
            > memory_hook::RECALL_TOPIC_MAX_BYTES
        {
            continue;
        }
        if !query.is_empty() {
            query.push(' ');
        }
        query.push_str(&term);
    }
    if query.is_empty() {
        PromptQuery::Unusable
    } else if !search::query_has_subject(&query) {
        PromptQuery::Generic
    } else {
        PromptQuery::Subject(query)
    }
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

fn is_task_intent_term(token: &str) -> bool {
    matches!(
        token,
        "analyze"
            | "analyse"
            | "see"
            | "create"
            | "merge"
            | "deploy"
            | "check"
            | "debug"
            | "debugging"
            | "describe"
            | "explain"
            | "find"
            | "fix"
            | "fixing"
            | "help"
            | "inspect"
            | "investigate"
            | "look"
            | "looking"
            | "make"
            | "making"
            | "need"
            | "review"
            | "show"
            | "summarise"
            | "summarize"
            | "tell"
            | "take"
            | "try"
            | "trying"
            | "understand"
            | "use"
            | "using"
            | "want"
    )
}

fn is_prompt_stopword(token: &str) -> bool {
    if let Some((pronoun, suffix)) = token.split_once('\'')
        && matches!(
            pronoun,
            "i" | "we" | "you" | "they" | "it" | "he" | "she" | "that"
        )
        && matches!(suffix, "m" | "re" | "ve" | "ll" | "d" | "s")
    {
        return true;
    }
    matches!(
        token,
        "about"
            | "lots"
            | "lot"
            | "one"
            | "ok"
            | "yes"
            | "sure"
            | "my"
            | "me"
            | "we"
            | "our"
            | "you"
            | "us"
            | "another"
            | "anything"
            | "actually"
            | "any"
            | "else"
            | "whether"
            | "useful"
            | "related"
            | "those"
            | "these"
            | "them"
            | "their"
            | "there"
            | "your"
            | "ours"
            | "more"
            | "other"
            | "like"
            | "keeping"
            | "conversational"
            | "framing"
            | "changes"
            | "change"
            | "side"
            | "effects"
            | "unexpected"
            | "surprising"
            | "safe"
            | "safely"
            | "made"
            | "three"
            | "okay"
            | "done"
            | "green"
            | "scrub"
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

    let store = &config.stores[resolved_store.name.as_str()];
    let store_key = index::store_cache_key(&resolved_store.name, &store.root);
    Ok(super::context::context_selection_key(
        &agent_label,
        &[store_key],
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
/// Prompt hooks call this before doing auxiliary work so policy failures are
/// reported by `hm` consistently, while the shell hook remains a thin event
/// adapter with no duplicate store-affinity logic.
fn validate_hook_context_read_policy(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
) -> Result<String> {
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let project_id = resolve_project_id(None, path_hint)?;
    let project_binding = project_binding_store(config, project_id.as_deref())?;
    Ok(resolve_store(
        config,
        context.store.as_deref(),
        project_binding.as_deref(),
        agent_id.as_deref(),
        StoreAccess::Read,
    )?
    .name)
}

fn memory_intent_reminder() -> &'static str {
    "Hive Memory: this prompt sounds like durable memory intent. If it remains useful, write it with `hm remember --scope project --text \"...\"` or `hm remember --text \"...\"`."
}

fn stop_memory_reminder() -> &'static str {
    "Hive Memory: durable memory intent is still pending. Before ending, write any lasting preference, decision, or project fact with `hm remember`."
}

#[cfg(test)]
mod tests {
    use super::{TrackedSelection, prompt_recall_query, select_tracked};
    use hive_memory::context::{ContextOutput, ContextSection, SizeLimit, SizeMeasure, TrustLevel};

    fn section(id: &str, body: &str) -> ContextSection {
        ContextSection {
            id: id.to_owned(),
            store: "personal".to_owned(),
            scope: "global".to_owned(),
            project_id: None,
            trust: TrustLevel::Remembered,
            audience: Vec::new(),
            source_path: format!("inbox/notes/{id}.md"),
            body: body.to_owned(),
            block: format!("<memory id=\"{id}\">\n{body}\n</memory>\n\n"),
            estimated_tokens: 1,
        }
    }

    fn output(sections: Vec<ContextSection>) -> ContextOutput {
        let mut markdown = "Hive Memory Context\n\n".to_owned();
        for section in &sections {
            markdown.push_str(&section.block);
        }
        ContextOutput {
            markdown,
            sections,
            decisions: Vec::new(),
            estimated_tokens: 1,
            warnings: Vec::new(),
        }
    }

    fn select(
        output: &ContextOutput,
        stale: bool,
        delivered: &[&str],
        max: Option<usize>,
    ) -> TrackedSelection {
        select_tracked(
            output,
            stale,
            |section| delivered.contains(&section.id.as_str()),
            max.map(SizeLimit::bytes),
        )
    }

    #[test]
    fn tracked_selection_without_memories_keeps_the_header() {
        let selected = select(&output(Vec::new()), false, &[], None);
        assert!(selected.emitted);
        let stale = select(&output(Vec::new()), true, &[], None);
        assert!(!stale.emitted);
    }

    #[test]
    fn tracked_selection_all_delivered_still_shows_the_header() {
        let out = output(vec![section("a", "one"), section("b", "two")]);
        let selected = select(&out, false, &["a", "b"], None);
        assert!(selected.emitted);
        assert!(selected.output.sections.is_empty());
        assert_eq!(selected.output.markdown, "Hive Memory Context\n\n");
        assert_eq!(selected.filtered, 2);
        // Even from a stale cache: the switch is real, nothing is missing.
        assert!(select(&out, true, &["a", "b"], None).emitted);
    }

    #[test]
    fn tracked_selection_reports_memories_left_for_later() {
        let out = output(vec![
            section("a", "one"),
            section("b", &"y".repeat(60)),
            section("c", "three"),
        ]);
        let max = out.markdown.len() - out.sections[2].block.len();
        let selected = select(&out, false, &[], Some(max));
        assert!(selected.emitted);
        assert_eq!(selected.output.sections.len(), 2);
        assert_eq!(selected.over_budget, 1);
        assert_eq!(selected.omitted, 0);
    }

    #[test]
    fn tracked_selection_header_only_after_budget_drop_is_not_emitted() {
        let out = output(vec![section("big", &"x".repeat(400))]);
        let selected = select(&out, false, &[], Some(60));
        assert!(!selected.emitted);
        assert_eq!(selected.over_budget, 1);
    }

    #[test]
    fn oversized_memory_becomes_a_pointer_and_does_not_block_later_ones() {
        let out = output(vec![
            section("big", &"x".repeat(2000)),
            section("small", "fits"),
        ]);
        let max = 600;
        let selected = select(&out, false, &[], Some(max));
        assert!(selected.emitted);
        assert_eq!(selected.omitted, 1);
        assert_eq!(selected.over_budget, 0);
        let ids = selected
            .output
            .sections
            .iter()
            .map(|section| section.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["small"]);
        assert!(selected.output.markdown.len() <= max);
        assert!(selected.output.markdown.contains("omitted=\"too-large\""));
        assert!(
            selected
                .output
                .markdown
                .contains("hm get --store personal big")
        );
    }

    #[test]
    fn memory_that_fits_the_whole_limit_waits_instead_of_becoming_a_pointer() {
        let out = output(vec![
            section("first", &"a".repeat(300)),
            section("second", &"b".repeat(300)),
        ]);
        let full = out.markdown.len() - 1;
        // Only the space left after other output (for example recall) is
        // too small for "second"; it fits the hook's whole limit, so it is
        // left for the next prompt rather than reduced to a pointer.
        let left = out.markdown.len() - out.sections[1].block.len();
        let limit = SizeLimit {
            max: left,
            alone_max: full,
            measure: SizeMeasure::Bytes,
            raw_max: usize::MAX,
        };
        let selected = select_tracked(&out, false, |_| false, Some(limit));
        assert_eq!(selected.omitted, 0);
        assert_eq!(selected.over_budget, 1);
        assert_eq!(selected.output.sections.len(), 1);
    }

    #[test]
    fn json_measure_counts_escaped_size_when_packing() {
        let out = output(vec![
            section("plain", "fits either way"),
            section("quoted", &"\"".repeat(100)),
        ]);
        // Room for both blocks as raw bytes, but not once the quotes in the
        // second are escaped for a JSON response.
        let max = out.markdown.len() + 10;
        let bytes = select_tracked(&out, false, |_| false, Some(SizeLimit::bytes(max)));
        assert_eq!(bytes.output.sections.len(), 2);
        let json = select_tracked(
            &out,
            false,
            |_| false,
            Some(SizeLimit {
                max,
                alone_max: max * 2,
                measure: SizeMeasure::JsonString,
                raw_max: usize::MAX,
            }),
        );
        assert_eq!(json.output.sections.len(), 1);
        assert_eq!(json.over_budget, 1);
        assert!(SizeMeasure::JsonString.of(&json.output.markdown) <= max);
    }

    #[test]
    fn unsplittable_output_is_emitted_only_when_it_fits() {
        let mut out = output(vec![section("a", "one")]);
        out.sections[0].block.clear();
        let fits = select(&out, false, &[], Some(out.markdown.len()));
        assert!(fits.emitted);
        assert_eq!(fits.over_budget, 0);
        let too_big = select(&out, false, &[], Some(out.markdown.len() - 1));
        assert!(!too_big.emitted);
        assert_eq!(too_big.over_budget, 1);
    }

    #[test]
    fn prompt_recall_query_ignores_recall_intent_words() {
        assert_eq!(
            prompt_recall_query(
                "Recall the Grafhome Cedar and Rego policy format comparison.",
                None
            )
            .as_deref(),
            Some("grafhome cedar rego policy")
        );
    }

    #[test]
    fn prompt_recall_query_skips_leading_task_intent() {
        assert_eq!(
            prompt_recall_query("Help analyze the deployment timeout.", None).as_deref(),
            Some("deployment timeout")
        );
        assert_eq!(
            prompt_recall_query("Find the deployment timeout.", None).as_deref(),
            Some("deployment timeout")
        );
        assert_eq!(
            prompt_recall_query("Explain the deployment timeout.", None).as_deref(),
            Some("deployment timeout")
        );
        assert_eq!(
            prompt_recall_query("In src/foo.rs, explain the deployment timeout.", None).as_deref(),
            Some("src/foo.rs deployment timeout")
        );
        assert_eq!(
            prompt_recall_query("I need help debugging the deployment timeout.", None).as_deref(),
            Some("deployment timeout")
        );
        assert_eq!(
            prompt_recall_query("I'm trying to understand the deployment timeout.", None)
                .as_deref(),
            Some("deployment timeout")
        );
        assert_eq!(
            prompt_recall_query("Can you take a look into the deployment timeout.", None)
                .as_deref(),
            Some("deployment timeout")
        );
    }
}
