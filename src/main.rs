//! `hm` command-line entry point.
//!
//! Keep this binary thin: the CLI is the user-facing shell contract, while
//! reusable policy and data handling live in the library so hooks and future
//! embedded callers do not need to shell out to themselves.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use hive_memory::config::{Config, ConfigPaths, EventSidecarPolicy, Sensitivity, StoreConfig};
use hive_memory::{
    capture, classify, config, context as memory_context, curation, doctor, eval as memory_eval,
    event, hook as memory_hook, id, index, inject, llm, memory, note, outbox, path as memory_path,
    project, reconcile, retrieval, search, secret, store, write, write_classify,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{self, Display};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Instant, SystemTime};
use time::OffsetDateTime;

// Clap derives user-facing help from doc comments, so keep implementation
// rationale as normal comments and reserve CLI docs for actual help text.
//
// Subcommands will be added here as the implementation grows. Keeping the
// struct explicit from the start gives smoke tests a stable place to verify the
// binary name, version, and help text.
/// Vendor-neutral shared memory infrastructure for AI agents.
#[derive(Debug, Parser)]
#[command(name = "hm")]
#[command(version = hive_memory::version::cli())]
#[command(about = "Vendor-neutral shared memory infrastructure for AI agents.")]
struct Cli {
    /// Main config file to load.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Active store alias for commands that read or write one store.
    #[arg(long, global = true)]
    store: Option<String>,
    /// Agent identity used for store-affinity policy.
    #[arg(long, global = true)]
    as_agent: Option<String>,
    /// Command to run.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Top-level command groups.
///
/// Keep each branch as a narrow adapter over library APIs. Policy and storage
/// behavior belongs in `hive_memory`, where hooks and tests can reuse it.
#[derive(Debug, Subcommand)]
enum Command {
    /// Manage memory stores.
    #[command(subcommand)]
    Stores(StoresCommand),
    /// Remember a durable fact/preference/context note.
    Remember(WriteMemoryArgs),
    /// Write a lower-confidence raw note.
    Note(WriteMemoryArgs),
    /// Search remembered memory.
    Search(SearchArgs),
    /// Assemble agent-readable memory context.
    Context(ContextArgs),
    /// Report store/index freshness without mutating memory.
    SyncStatus(SyncStatusArgs),
    /// Correct the persisted memory kind on an existing record.
    Retag(RetagArgs),
    /// Run the background LLM classification pass now.
    Classify(ClassifyArgs),
    /// Extract durable facts from a conversation and stage them as raw inbox
    /// notes for later review/promotion. Never writes canonical memory.
    Capture(CaptureArgs),
    /// Reconcile a candidate fact against existing memory (mem0-style
    /// ADD/UPDATE/DELETE/NOOP) and apply it via remember + supersedes.
    Reconcile(ReconcileArgs),
    /// Refresh local outbox and indexes.
    Refresh(RefreshArgs),
    /// Flush local outbox writes to reachable stores.
    Flush(FlushArgs),
    /// Manage local outbox writes.
    #[command(subcommand)]
    Outbox(OutboxCommand),
    /// Resolve project identity and local project policy.
    #[command(subcommand)]
    Projects(ProjectsCommand),
    /// Run agent lifecycle hook policy.
    #[command(subcommand)]
    Hook(HookCommand),
    /// Run top-level diagnostics.
    Doctor(DoctorArgs),
    /// Promote a raw inbox note into curated memory.
    Promote(PromoteArgs),
    /// Inspect raw inbox notes.
    #[command(subcommand)]
    Inbox(InboxCommand),
    /// Capture retrieval misses and bad hits as eval fixture cases.
    #[command(subcommand)]
    Eval(EvalCommand),
}

/// Store lifecycle commands.
#[derive(Debug, Subcommand)]
enum StoresCommand {
    /// Initialize a store root with a manifest and canonical directories.
    Init(StoreInitArgs),
    /// List configured stores and root availability.
    List(StoreListArgs),
    /// Show one configured store, defaulting to the global default store.
    Show(StoreShowArgs),
    /// Run store diagnostics.
    Doctor(StoreDoctorArgs),
    /// Run schema migrators when a future schema is available.
    Migrate(StoreMigrateArgs),
}

/// Project identity commands.
#[derive(Debug, Subcommand)]
enum ProjectsCommand {
    /// List local project-to-store bindings.
    List(ProjectListArgs),
    /// Show local policy and aliases for one project.
    Show(ProjectShowArgs),
    /// Resolve a path/file hint to a stable project id.
    Resolve(ProjectResolveArgs),
    /// Bind a project to a local preferred store.
    Bind(ProjectBindArgs),
    /// Remove a local project store binding.
    Unbind(ProjectUnbindArgs),
    /// Record that an old project id now maps to a new project id.
    Alias(ProjectAliasArgs),
}

/// Local outbox commands.
#[derive(Debug, Subcommand)]
enum OutboxCommand {
    /// Flush local outbox writes to reachable stores.
    Flush(FlushArgs),
}

/// Raw inbox triage commands.
#[derive(Debug, Subcommand)]
enum InboxCommand {
    /// List raw inbox notes that still need triage.
    List(InboxListArgs),
    /// List unpromoted raw notes older than N days.
    Stale(InboxStaleArgs),
    /// Show one raw inbox note.
    Show(InboxShowArgs),
}

/// Eval fixture helper commands.
#[derive(Debug, Subcommand)]
enum EvalCommand {
    /// Run a retrieval corpus and report A/B metrics.
    Retrieval(EvalRetrievalArgs),
    /// Capture a recall miss as a retrieval eval case.
    CaptureMiss(EvalCaptureMissArgs),
    /// Capture an irrelevant retrieval hit as a retrieval eval case.
    CaptureBadHit(EvalCaptureBadHitArgs),
}

/// Arguments for `hm eval retrieval`.
#[derive(Debug, Args)]
struct EvalRetrievalArgs {
    /// TOML corpus file containing records and retrieval_case labels.
    #[arg(long)]
    corpus: PathBuf,
    /// Search limit used for each retrieval case.
    #[arg(long, default_value_t = 5)]
    limit: usize,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Shared arguments for `hm eval capture-*`.
#[derive(Debug, Args)]
struct EvalCaptureCommonArgs {
    /// Prompt or query that exposed the retrieval behavior.
    #[arg(long)]
    prompt: String,
    /// Optional human-readable case name. Defaults to a prompt-derived name.
    #[arg(long)]
    name: Option<String>,
    /// Feature bucket this case should score.
    #[arg(long, default_value = "semantic")]
    feature: String,
    /// Project id the query should run under, when project-scoped.
    #[arg(long)]
    project_id: Option<String>,
    /// Append the generated case to this TOML fixture file.
    #[arg(long)]
    to: Option<PathBuf>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm eval capture-miss`.
#[derive(Debug, Args)]
struct EvalCaptureMissArgs {
    #[command(flatten)]
    common: EvalCaptureCommonArgs,
    /// Subject id that should have been retrieved. Repeat for multiple labels.
    #[arg(long, required = true)]
    expected: Vec<String>,
    /// Subject id that must not be retrieved. Repeat for multiple labels.
    #[arg(long)]
    forbidden: Vec<String>,
}

/// Arguments for `hm eval capture-bad-hit`.
#[derive(Debug, Args)]
struct EvalCaptureBadHitArgs {
    #[command(flatten)]
    common: EvalCaptureCommonArgs,
    /// Subject id that was incorrectly retrieved. Repeat for multiple labels.
    #[arg(long, required = true)]
    bad: Vec<String>,
    /// Subject id that should be retrieved, if known. Repeat for multiple labels.
    #[arg(long)]
    expected: Vec<String>,
}

/// Arguments for `hm inbox list`.
#[derive(Debug, Args)]
struct InboxListArgs {
    /// Include notes that already have a promotion event.
    #[arg(long)]
    all: bool,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm inbox stale`.
#[derive(Debug, Args)]
struct InboxStaleArgs {
    /// Minimum age in days for unpromoted notes.
    #[arg(long)]
    days: i64,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm inbox show`.
#[derive(Debug, Args)]
struct InboxShowArgs {
    /// Raw note id to show.
    note_id: String,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm promote`.
#[derive(Debug, Args)]
struct PromoteArgs {
    /// Raw note id to promote.
    note_id: String,
    /// Store-relative curated target path.
    #[arg(long, default_value = curation::DEFAULT_PROMOTION_TARGET)]
    to: PathBuf,
    /// Preserve the source body instead of converting it to a bullet.
    #[arg(long, conflicts_with = "as_bullet")]
    verbatim: bool,
    /// Convert the source body to a bullet. This is the default.
    #[arg(long)]
    as_bullet: bool,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm projects resolve`.
#[derive(Debug, Args)]
struct ProjectResolveArgs {
    /// Path, file, or directory hint as an option, matching other agent-facing commands.
    #[arg(long, value_name = "PATH", conflicts_with = "path")]
    project: Option<PathBuf>,
    /// Path, file, or directory hint. Defaults to HIVE_MEMORY_PROJECT, then CWD.
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,
    /// Explicit project id override.
    #[arg(long)]
    project_id: Option<String>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm projects list`.
#[derive(Debug, Args)]
struct ProjectListArgs {
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm projects show`.
#[derive(Debug, Args)]
struct ProjectShowArgs {
    /// Project id to inspect. Defaults to resolving the current project hint.
    project_id: Option<String>,
    /// Path, file, or directory hint used when no project id is provided.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm projects bind`.
#[derive(Debug, Args)]
struct ProjectBindArgs {
    /// Path, file, or directory hint for the project to bind.
    path: PathBuf,
    /// Store alias to prefer for this project on this machine.
    #[arg(long)]
    store: String,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm projects unbind`.
#[derive(Debug, Args)]
struct ProjectUnbindArgs {
    /// Path, file, or directory hint for the project to unbind.
    path: PathBuf,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm projects alias`.
#[derive(Debug, Args)]
struct ProjectAliasArgs {
    /// Prior project id to preserve.
    old_id: String,
    /// Canonical/current project id.
    new_id: String,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm stores init`.
///
/// The CLI captures explicit user intent only. Identity generation, directory
/// layout, and atomic manifest writes are delegated to the store library.
#[derive(Debug, Args)]
struct StoreInitArgs {
    /// Local alias/human name to write into the store manifest.
    name: String,
    /// Filesystem root to initialize.
    #[arg(long)]
    root: PathBuf,
    /// Optional human description to include in the manifest.
    #[arg(long)]
    description: Option<String>,
    /// Store sensitivity policy to record in the manifest.
    #[arg(long, default_value = "private", value_parser = parse_sensitivity)]
    sensitivity: Sensitivity,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm stores list`.
#[derive(Debug, Args)]
struct StoreListArgs {
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm stores show`.
#[derive(Debug, Args)]
struct StoreShowArgs {
    /// Store alias to show. Defaults to config.default_store.
    name: Option<String>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm stores doctor`.
#[derive(Debug, Args)]
struct StoreDoctorArgs {
    /// Store alias to diagnose. Defaults to all configured stores.
    name: Option<String>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm doctor`.
#[derive(Debug, Args)]
struct DoctorArgs {
    /// Run the hook/update-safe subset.
    #[arg(long)]
    quick: bool,
    /// Perform safe layout repairs before reporting diagnostics.
    #[arg(long)]
    fix: bool,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// JSON envelope used only when `hm doctor --fix --json` is requested.
#[derive(Debug, Serialize)]
struct DoctorFixOutput<'a> {
    /// Mutations performed or deliberately skipped by the fix pass.
    fixes: &'a doctor::DoctorFixReport,
    /// Diagnostic report after repairs have been attempted.
    doctor: &'a doctor::DoctorReport,
}

#[derive(Debug, Serialize)]
struct StoreInitOutput {
    name: String,
    root: String,
    store_id: String,
    sensitivity: String,
}

#[derive(Debug, Serialize)]
struct ProjectBindOutput {
    project_id: String,
    store: String,
    binding: String,
}

#[derive(Debug, Serialize)]
struct ProjectUnbindOutput {
    project_id: String,
    removed: bool,
    binding: Option<String>,
}

/// Arguments for `hm stores migrate`.
#[derive(Debug, Args)]
struct StoreMigrateArgs {
    /// Check what would migrate without changing stores.
    #[arg(long)]
    dry_run: bool,
    /// Store alias to migrate. Defaults to all configured stores.
    #[arg(long)]
    store: Option<String>,
}

/// Arguments for `hm remember` and `hm note`.
#[derive(Debug, Args)]
struct WriteMemoryArgs {
    /// Markdown body to write.
    #[arg(long)]
    text: String,
    /// Memory scope. Defaults to config.defaults.write_scope.
    #[arg(long)]
    scope: Option<String>,
    /// Use config.defaults.write_scope without automatic scope inference.
    #[arg(long, conflicts_with = "scope")]
    no_infer_scope: bool,
    /// Writer confidence.
    #[arg(long, default_value = "medium", value_parser = parse_confidence)]
    confidence: note::Confidence,
    /// Optional project identity.
    #[arg(long)]
    project_id: Option<String>,
    /// Project path or file hint used to derive project identity.
    #[arg(long)]
    project: Option<PathBuf>,
    /// Optional short subject.
    #[arg(long)]
    subject: Option<String>,
    /// Optional explicit memory kind driving session-start inject selection.
    #[arg(long, value_parser = parse_memory_kind)]
    kind: Option<note::MemoryKind>,
    /// RFC3339 timestamp when this memory starts being current.
    #[arg(long)]
    valid_from: Option<String>,
    /// RFC3339 timestamp when this memory stops being current.
    #[arg(long)]
    valid_to: Option<String>,
    /// Memory id superseded by this write. Repeat for multiple ids.
    #[arg(long)]
    supersedes: Vec<String>,
    /// Store without automatic kind inference when `--kind` is omitted.
    #[arg(long, conflicts_with = "kind")]
    no_infer_kind: bool,
    /// Optional comma-separated tags.
    #[arg(long, value_delimiter = ',')]
    tags: Vec<String>,
    /// Optional permitted agents for agent-private writes.
    #[arg(long, value_delimiter = ',')]
    audience: Vec<String>,
    /// Use the writer agent as the agent-private audience.
    #[arg(long)]
    audience_writer_only: bool,
    /// Optional source kind, such as session, hook, or import.
    #[arg(long)]
    source_kind: Option<String>,
    /// Optional source reference.
    #[arg(long)]
    source_ref: Option<String>,
    /// Write a JSON sidecar for `hm note` regardless of config defaults.
    #[arg(long, conflicts_with = "no_event")]
    event: bool,
    /// Skip the JSON sidecar for `hm note` regardless of config defaults.
    #[arg(long)]
    no_event: bool,
    /// Permit detected secrets only when config and a secret store allow it.
    #[arg(long)]
    allow_secret_write: bool,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm search`.
#[derive(Debug, Args)]
struct SearchArgs {
    /// Case-insensitive substring query.
    query: String,
    /// Maximum hits to show.
    #[arg(long, default_value_t = 20)]
    limit: usize,
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
    /// Include structured scoring diagnostics.
    #[arg(long)]
    explain: bool,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm context`.
#[derive(Debug, Args)]
struct ContextArgs {
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

/// Arguments for `hm sync-status`.
#[derive(Debug, Args)]
struct SyncStatusArgs {
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm retag`.
#[derive(Debug, Args)]
struct RetagArgs {
    /// Memory record id to correct.
    id: String,
    /// New kind: preference, project-fact, incident, reference, or `none` to
    /// clear the tag and fall back to read-time classification.
    #[arg(long)]
    kind: String,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm classify`.
#[derive(Debug, Args)]
struct ClassifyArgs {
    /// Respect mode/interval/lock policy for hook-spawned runs.
    #[arg(long, conflicts_with = "pending")]
    auto: bool,
    /// Judge through the configured backend without persisting verdicts or stamps.
    #[arg(long, conflicts_with = "pending")]
    dry_run: bool,
    /// Show pending classifier records without invoking a backend.
    #[arg(long)]
    pending: bool,
    /// Override the per-run batch limit.
    #[arg(long)]
    limit: Option<u32>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm capture`.
#[derive(Debug, Args)]
struct CaptureArgs {
    /// Conversation text to extract facts from. Reads stdin when omitted.
    #[arg(long)]
    text: Option<String>,
    /// Show the extracted candidate facts without writing anything.
    #[arg(long)]
    dry_run: bool,
    /// Optional provenance reference recorded on each staged note.
    #[arg(long)]
    source_ref: Option<String>,
    /// Reconcile each extracted fact into durable memory (mem0-style
    /// add/update/delete) instead of staging low-confidence inbox notes.
    #[arg(long, conflicts_with = "dry_run")]
    promote: bool,
    /// Number of most-similar existing memories to weigh per fact when
    /// `--promote` is set (default 5).
    #[arg(long, default_value_t = 5)]
    limit: usize,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm reconcile`.
#[derive(Debug, Args)]
struct ReconcileArgs {
    /// Candidate fact to reconcile. Reads stdin when omitted.
    #[arg(long)]
    text: Option<String>,
    /// Decide and print the operation without writing anything.
    #[arg(long)]
    dry_run: bool,
    /// Number of most-similar existing memories to weigh (default 5).
    #[arg(long, default_value_t = 5)]
    limit: usize,
    /// Optional provenance reference recorded on a written record.
    #[arg(long)]
    source_ref: Option<String>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm refresh`.
#[derive(Debug, Args)]
struct RefreshArgs {
    /// Suppress the summary line.
    #[arg(long)]
    quiet: bool,
    /// Run even when future receipt tracking would otherwise skip work.
    #[arg(long)]
    force: bool,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm flush`.
#[derive(Debug, Args)]
struct FlushArgs {
    /// Suppress the human summary line.
    #[arg(long)]
    quiet: bool,
    /// Bind one unbound outbox item id to the selected --store before flushing.
    #[arg(long)]
    bind: Option<String>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Agent lifecycle hook events.
#[derive(Debug, Subcommand)]
enum HookCommand {
    /// Emit initial memory context for a new agent session.
    SessionStart(HookContextArgs),
    /// Inspect a submitted prompt for context changes and memory intent.
    PromptSubmit(HookPromptSubmitArgs),
    /// Handle a completed tool event.
    ToolComplete(HookToolCompleteArgs),
    /// Emit an end-of-session reminder when memory intent remains pending.
    Stop(HookStopArgs),
}

/// Shared hook context-selection arguments.
#[derive(Debug, Args)]
struct HookContextArgs {
    /// Active project path or file hint.
    #[arg(long)]
    project: Option<String>,
    /// Emit machine-readable hook actions.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm hook prompt-submit`.
#[derive(Debug, Args)]
struct HookPromptSubmitArgs {
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
struct HookToolCompleteArgs {
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
struct HookStopArgs {
    /// Emit machine-readable hook actions.
    #[arg(long)]
    json: bool,
}

fn main() {
    let cli = Cli::parse();
    let json = cli.wants_json();
    if let Err(err) = run(cli) {
        emit_cli_error(&err, json);
        std::process::exit(exit_code(&err));
    }
}

fn run(cli: Cli) -> Result<()> {
    let hook_active = matches!(cli.command, Some(Command::Hook(_)));
    let context = CliContext {
        config_path: cli.config,
        store: cli.store,
        as_agent: cli.as_agent,
        hook_active,
    };
    match cli.command {
        Some(Command::Stores(command)) => run_stores(command, context),
        Some(Command::Remember(args)) => run_write_memory(note::EntryKind::Remember, args, context),
        Some(Command::Note(args)) => run_write_memory(note::EntryKind::Note, args, context),
        Some(Command::Search(args)) => run_search(args, context),
        Some(Command::Context(args)) => run_context(args, context),
        Some(Command::SyncStatus(args)) => run_sync_status(args, context),
        Some(Command::Retag(args)) => run_retag(args, context),
        Some(Command::Classify(args)) => run_classify(args, context),
        Some(Command::Capture(args)) => run_capture(args, context),
        Some(Command::Reconcile(args)) => run_reconcile(args, context),
        Some(Command::Refresh(args)) => run_refresh(args, context),
        Some(Command::Flush(args)) => run_flush(args, context),
        Some(Command::Outbox(OutboxCommand::Flush(args))) => run_flush(args, context),
        Some(Command::Projects(command)) => run_projects(command, context),
        Some(Command::Hook(command)) => run_hook(command, context),
        Some(Command::Doctor(args)) => run_doctor(args, context),
        Some(Command::Promote(args)) => run_promote(args, context),
        Some(Command::Inbox(command)) => run_inbox(command, context),
        Some(Command::Eval(command)) => run_eval(command, context),
        None => Ok(()),
    }
}

impl Cli {
    /// Return whether this invocation has opted into structured CLI output.
    ///
    /// Error rendering happens after command dispatch fails, so it cannot ask
    /// the already-failed command how to report diagnostics. Keep this list in
    /// lockstep with command structs that expose `--json`; those commands owe
    /// callers a stable JSON success *and* failure envelope.
    fn wants_json(&self) -> bool {
        match &self.command {
            Some(Command::Stores(StoresCommand::Init(args))) => args.json,
            Some(Command::Stores(StoresCommand::List(args))) => args.json,
            Some(Command::Stores(StoresCommand::Show(args))) => args.json,
            Some(Command::Stores(StoresCommand::Doctor(args))) => args.json,
            Some(Command::Remember(args)) | Some(Command::Note(args)) => args.json,
            Some(Command::Search(args)) => args.json,
            Some(Command::Context(args)) => args.json,
            Some(Command::SyncStatus(args)) => args.json,
            Some(Command::Retag(args)) => args.json,
            Some(Command::Classify(args)) => args.json,
            Some(Command::Refresh(args)) => args.json,
            Some(Command::Flush(args)) | Some(Command::Outbox(OutboxCommand::Flush(args))) => {
                args.json
            }
            Some(Command::Projects(ProjectsCommand::Resolve(args))) => args.json,
            Some(Command::Projects(ProjectsCommand::Bind(args))) => args.json,
            Some(Command::Projects(ProjectsCommand::Unbind(args))) => args.json,
            Some(Command::Hook(HookCommand::SessionStart(args))) => args.json,
            Some(Command::Hook(HookCommand::PromptSubmit(args))) => args.json,
            Some(Command::Hook(HookCommand::ToolComplete(args))) => args.json,
            Some(Command::Hook(HookCommand::Stop(args))) => args.json,
            Some(Command::Doctor(args)) => args.json,
            Some(Command::Promote(args)) => args.json,
            Some(Command::Inbox(InboxCommand::List(args))) => args.json,
            Some(Command::Inbox(InboxCommand::Stale(args))) => args.json,
            Some(Command::Inbox(InboxCommand::Show(args))) => args.json,
            Some(Command::Eval(EvalCommand::CaptureMiss(args))) => args.common.json,
            Some(Command::Eval(EvalCommand::CaptureBadHit(args))) => args.common.json,
            _ => false,
        }
    }
}

/// Operational failure for a store backend that may become reachable later.
///
/// Hooks need to distinguish this from malformed config or unsafe policy
/// failures. A backend-unavailable error can usually be retried after a mount,
/// sync client, or network path comes back, and JSON hook adapters can present
/// that as stale/missing memory context rather than as a broken installation.
#[derive(Debug)]
struct BackendUnavailable {
    message: String,
}

impl Display for BackendUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for BackendUnavailable {}

/// Privacy or safety policy refusal.
///
/// These errors mean `hm` deliberately refused to read or write data because
/// doing so could cross an agent/store boundary, expose secret material, or
/// create memory with unclear audience. Callers should not retry unchanged.
#[derive(Debug)]
struct PrivacyRefusal {
    message: String,
}

impl Display for PrivacyRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for PrivacyRefusal {}

#[derive(Debug, Serialize)]
struct JsonErrorOutput<'a> {
    ok: bool,
    error: JsonErrorBody<'a>,
}

#[derive(Debug, Serialize)]
struct JsonErrorBody<'a> {
    code: &'a str,
    message: String,
    details: serde_json::Value,
}

fn emit_cli_error(err: &anyhow::Error, json: bool) {
    if json {
        // JSON commands should never fall back to anyhow's human text. Hook
        // adapters and agent integrations branch on `error.code`, while
        // `message` remains suitable for logs and terminal display.
        let output = JsonErrorOutput {
            ok: false,
            error: JsonErrorBody {
                code: error_code(err),
                message: err.to_string(),
                details: serde_json::json!({}),
            },
        };
        eprintln!(
            "{}",
            serde_json::to_string_pretty(&output).expect("serialize JSON error")
        );
    } else {
        eprintln!("Error: {err}");
    }
}

/// Return the stable machine code for an operational CLI error.
fn error_code(err: &anyhow::Error) -> &'static str {
    if err.downcast_ref::<BackendUnavailable>().is_some() {
        "backend_unavailable"
    } else if err.downcast_ref::<PrivacyRefusal>().is_some() {
        "privacy_refusal"
    } else if err.downcast_ref::<config::ConfigError>().is_some() {
        "config_error"
    } else {
        "error"
    }
}

/// Return the process status for an operational CLI error.
fn exit_code(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<BackendUnavailable>().is_some() {
        5
    } else if err.downcast_ref::<PrivacyRefusal>().is_some() {
        4
    } else if err.downcast_ref::<config::ConfigError>().is_some() {
        3
    } else {
        1
    }
}

struct CliContext {
    config_path: Option<PathBuf>,
    store: Option<String>,
    as_agent: Option<String>,
    hook_active: bool,
}

fn hook_active(context: &CliContext) -> bool {
    context.hook_active || std::env::var("HIVE_MEMORY_HOOK_ACTIVE").ok().as_deref() == Some("1")
}

fn run_doctor(args: DoctorArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let fix_report = if args.fix {
        Some(doctor::fix(doctor::DoctorFixInput { config: &config }))
    } else {
        None
    };
    let report = doctor::run(doctor::DoctorInput {
        config: &config,
        quick: args.quick,
    });

    if args.json {
        if let Some(fixes) = fix_report.as_ref() {
            println!(
                "{}",
                serde_json::to_string_pretty(&DoctorFixOutput {
                    fixes,
                    doctor: &report
                })?
            );
        } else {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    } else {
        if let Some(fixes) = fix_report.as_ref() {
            println!(
                "doctor fix: {} (fixed={} skipped={} failed={})",
                if fixes.ok { "ok" } else { "fail" },
                fixes.summary.fixed,
                fixes.summary.skipped,
                fixes.summary.failed
            );
            for action in &fixes.actions {
                println!("{}: {}", action.status, action.message);
                println!("  path: {}", action.path);
            }
        }
        println!(
            "doctor: {} (errors={} warnings={})",
            if report.ok { "ok" } else { "fail" },
            report.summary.errors,
            report.summary.warnings
        );
        for check in &report.checks {
            if check.severity == doctor::DoctorSeverity::Info {
                continue;
            }
            println!("{}: {}", check.severity, check.message);
            for path in &check.paths {
                println!("  path: {path}");
            }
        }
    }

    if !report.ok {
        anyhow::bail!("doctor found errors");
    }
    if let Some(fixes) = fix_report
        && !fixes.ok
    {
        anyhow::bail!("doctor fix failed");
    }
    Ok(())
}

fn run_stores(command: StoresCommand, context: CliContext) -> Result<()> {
    match command {
        StoresCommand::Init(args) => {
            let options = store::StoreInitOptions {
                name: args.name,
                root: args.root,
                description: args.description,
                sensitivity: args.sensitivity,
            };
            let root = options.root.clone();
            let manifest = store::init_store(&options)?;
            if args.json {
                let output = StoreInitOutput {
                    name: manifest.store.name,
                    root: root.display().to_string(),
                    store_id: manifest.store.id,
                    sensitivity: manifest.store.sensitivity.to_string(),
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!(
                    "initialized store {} at {}",
                    manifest.store.name,
                    root.display()
                );
            }
            Ok(())
        }
        StoresCommand::List(args) => {
            let config = load_config(context.config_path.as_deref())?;
            list_stores(&config, resolve_agent_id(context.as_agent), args.json)
        }
        StoresCommand::Show(args) => {
            let config = load_config(context.config_path.as_deref())?;
            show_store(
                &config,
                args.name.as_deref(),
                resolve_agent_id(context.as_agent),
                args.json,
            )?;
            Ok(())
        }
        StoresCommand::Doctor(args) => {
            let config = load_config(context.config_path.as_deref())?;
            run_store_doctor(&config, args.name.as_deref(), args.json)
        }
        StoresCommand::Migrate(args) => {
            let config = load_config(context.config_path.as_deref())?;
            run_store_migrate(&config, args.store.as_deref(), args.dry_run)
        }
    }
}

fn run_projects(command: ProjectsCommand, context: CliContext) -> Result<()> {
    match command {
        ProjectsCommand::List(args) => run_project_list(args, context),
        ProjectsCommand::Show(args) => run_project_show(args, context),
        ProjectsCommand::Resolve(args) => run_project_resolve(args, context),
        ProjectsCommand::Bind(args) => run_project_bind(args, context),
        ProjectsCommand::Unbind(args) => run_project_unbind(args, context),
        ProjectsCommand::Alias(args) => run_project_alias(args, context),
    }
}

fn run_project_list(args: ProjectListArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let bindings = project::list_bindings(&config.data_dir)?;

    if args.json {
        let output = ProjectListOutput {
            data_dir: config.data_dir.display().to_string(),
            bindings,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("data_dir: {}", config.data_dir.display());
        println!("bindings: {}", bindings.len());
        for binding in bindings {
            println!("  {} -> {}", binding.project_id, binding.store);
        }
    }

    Ok(())
}

fn run_project_show(args: ProjectShowArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let project_id = if let Some(project_id) = args.project_id {
        project_id
    } else {
        let hint = args
            .path
            .or_else(|| std::env::var("HIVE_MEMORY_PROJECT").ok().map(PathBuf::from))
            .unwrap_or_default();
        project::resolve_project(project::ResolveProjectInput {
            hint,
            explicit_project_id: None,
            env_project_id: std::env::var("HIVE_MEMORY_PROJECT_ID").ok(),
        })?
        .project_id
    };
    let binding = project::load_binding(&config.data_dir, &project_id)?;
    let agent_id = resolve_agent_id(context.as_agent);
    // Like `projects resolve`, this reports a checkout's effective store. Humans
    // may inspect any configured store, including a non-default bound store;
    // asserted agents are still held to their read policy.
    let store = resolve_store_with_policy(
        &config,
        context.store.as_deref(),
        binding.as_ref().map(|binding| binding.store.as_str()),
        agent_id.as_deref(),
        StoreAccess::Read,
        NoIdentityPolicy::AllowAnyStore,
    )?;
    let store_config = &config.stores[store.name.as_str()];
    let aliases = project::related_project_ids(&store_config.root, &project_id)?
        .into_iter()
        .filter(|related| related != &project_id)
        .collect::<Vec<_>>();

    if args.json {
        let output = ProjectShowOutput {
            project_id,
            binding,
            effective_store: store.name,
            store_source: store.source.to_string(),
            related_project_ids: aliases,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("project_id: {}", project_id);
        match &binding {
            Some(binding) => println!("binding: {}", binding.store),
            None => println!("binding: none"),
        }
        println!("store: {}", store.name);
        println!("store_source: {}", store.source);
        if aliases.is_empty() {
            println!("related_project_ids: none");
        } else {
            println!("related_project_ids:");
            for alias in aliases {
                println!("  {alias}");
            }
        }
    }

    Ok(())
}

fn run_project_resolve(args: ProjectResolveArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let hint = args
        .project
        .or(args.path)
        .or_else(|| std::env::var("HIVE_MEMORY_PROJECT").ok().map(PathBuf::from))
        .unwrap_or_default();
    let project = project::resolve_project(project::ResolveProjectInput {
        hint,
        explicit_project_id: args.project_id,
        env_project_id: std::env::var("HIVE_MEMORY_PROJECT_ID").ok(),
    })?;
    let agent_id = resolve_agent_id(context.as_agent);
    let binding = project::load_binding(&config.data_dir, &project.project_id)?;
    // `projects resolve` reports the effective store for a checkout. A human
    // (no identity) may resolve any configured store, including a non-default
    // bound store; an asserted agent is still held to its read policy so a
    // binding cannot bless a store outside the agent's allowlist.
    let store = resolve_store_with_policy(
        &config,
        context.store.as_deref(),
        binding.as_ref().map(|binding| binding.store.as_str()),
        agent_id.as_deref(),
        StoreAccess::Read,
        NoIdentityPolicy::AllowAnyStore,
    )?;

    if args.json {
        let output = ProjectResolveOutput {
            project_id: project.project_id,
            project_root: project.project_root.display().to_string(),
            project_hint: project.project_hint.display().to_string(),
            project_source: project.source.to_string(),
            store: store.name,
            store_source: store.source.to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("project_id: {}", project.project_id);
        println!("project_root: {}", project.project_root.display());
        println!("project_hint: {}", project.project_hint.display());
        println!("project_source: {}", project.source);
        println!("store: {}", store.name);
        println!("store_source: {}", store.source);
    }

    Ok(())
}

fn run_project_bind(args: ProjectBindArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let project = project::resolve_project(project::ResolveProjectInput {
        hint: args.path,
        explicit_project_id: None,
        env_project_id: std::env::var("HIVE_MEMORY_PROJECT_ID").ok(),
    })?;
    let agent_id = resolve_agent_id(context.as_agent);
    // A binding can affect both read and write commands. When an active agent
    // identity is present, validate both sides now so a local affinity file
    // cannot bless a store the agent would be unable to use safely.
    //
    // `AllowAnyStore` keeps the human path intact: `hm projects bind PATH
    // --store work` records a local, machine-private affinity decision ("this
    // checkout belongs to work on this host") and a human may bind ANY
    // configured store regardless of the global default. The no-identity
    // default-store fail-closed protects memory read/write commands from a
    // restricted agent dropping `--as-agent`; it must not block this local-data
    // write. Agent store affinity is still enforced at memory use time, so a
    // binding can never bypass affinity.
    resolve_store_with_policy(
        &config,
        Some(args.store.as_str()),
        None,
        agent_id.as_deref(),
        StoreAccess::Read,
        NoIdentityPolicy::AllowAnyStore,
    )?;
    resolve_store_with_policy(
        &config,
        Some(args.store.as_str()),
        None,
        agent_id.as_deref(),
        StoreAccess::Write,
        NoIdentityPolicy::AllowAnyStore,
    )?;
    let binding = project::ProjectBinding {
        project_id: project.project_id.clone(),
        store: args.store,
    };
    let path = project::save_binding(&config.data_dir, &binding, &hook_options(&config))?;

    if args.json {
        let output = ProjectBindOutput {
            project_id: project.project_id,
            store: binding.store,
            binding: path.display().to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("project_id: {}", project.project_id);
        println!("store: {}", binding.store);
        println!("binding: {}", path.display());
    }
    Ok(())
}

fn run_project_unbind(args: ProjectUnbindArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let project = project::resolve_project(project::ResolveProjectInput {
        hint: args.path,
        explicit_project_id: None,
        env_project_id: std::env::var("HIVE_MEMORY_PROJECT_ID").ok(),
    })?;
    let removed = project::remove_binding(&config.data_dir, &project.project_id)?;

    if args.json {
        let output = ProjectUnbindOutput {
            project_id: project.project_id,
            removed: removed.is_some(),
            binding: removed.map(|path| path.display().to_string()),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("project_id: {}", project.project_id);
        println!("removed: {}", removed.is_some());
        if let Some(path) = removed {
            println!("binding: {}", path.display());
        }
    }
    Ok(())
}

fn run_project_alias(args: ProjectAliasArgs, context: CliContext) -> Result<()> {
    if args.old_id == args.new_id {
        anyhow::bail!("old and new project ids must differ");
    }
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent);
    let store = resolve_store(
        &config,
        context.store.as_deref(),
        None,
        agent_id.as_deref(),
        StoreAccess::Write,
    )?;
    let store_config = &config.stores[store.name.as_str()];
    read_store_manifest(&config, &store.name, store_config)?;
    let path = project::add_alias(
        &store_config.root,
        &args.old_id,
        &args.new_id,
        &hook_options(&config),
    )?;

    if args.json {
        let output = ProjectAliasOutput {
            old_id: args.old_id,
            new_id: args.new_id,
            store: store.name,
            store_source: store.source.to_string(),
            path: path.display().to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("old_id: {}", args.old_id);
        println!("new_id: {}", args.new_id);
        println!("store: {}", store.name);
        println!("store_source: {}", store.source);
        println!("aliases: {}", path.display());
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct ProjectResolveOutput {
    project_id: String,
    project_root: String,
    project_hint: String,
    project_source: String,
    store: String,
    store_source: String,
}

#[derive(Debug, Serialize)]
struct ProjectAliasOutput {
    old_id: String,
    new_id: String,
    store: String,
    store_source: String,
    path: String,
}

#[derive(Debug, Serialize)]
struct ProjectListOutput {
    data_dir: String,
    bindings: Vec<project::ProjectBinding>,
}

#[derive(Debug, Serialize)]
struct ProjectShowOutput {
    project_id: String,
    binding: Option<project::ProjectBinding>,
    effective_store: String,
    store_source: String,
    related_project_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct InboxListOutput {
    store: String,
    items: Vec<curation::InboxItem>,
}

#[derive(Debug, Serialize)]
struct InboxShowOutput {
    store: String,
    item: curation::InboxItem,
    body: String,
}

fn run_inbox(command: InboxCommand, context: CliContext) -> Result<()> {
    match command {
        InboxCommand::List(args) => run_inbox_list(args, context),
        InboxCommand::Stale(args) => run_inbox_stale(args, context),
        InboxCommand::Show(args) => run_inbox_show(args, context),
    }
}

fn run_inbox_list(args: InboxListArgs, context: CliContext) -> Result<()> {
    let (store_name, store_root, report) = inbox_context(&context, StoreAccess::Read)?;
    let items = curation::list_inbox(curation::InboxListInput {
        store_root: &store_root,
        entries: &report.entries,
        include_promoted: args.all,
        stale_before: None,
    })?;
    print_inbox_list(&store_name, items, args.json)
}

fn run_inbox_stale(args: InboxStaleArgs, context: CliContext) -> Result<()> {
    if args.days < 0 {
        anyhow::bail!("--days must be non-negative");
    }
    let cutoff = OffsetDateTime::now_utc() - time::Duration::days(args.days);
    let (store_name, store_root, report) = inbox_context(&context, StoreAccess::Read)?;
    let items = curation::list_inbox(curation::InboxListInput {
        store_root: &store_root,
        entries: &report.entries,
        include_promoted: false,
        stale_before: Some(cutoff),
    })?;
    print_inbox_list(&store_name, items, args.json)
}

fn run_inbox_show(args: InboxShowArgs, context: CliContext) -> Result<()> {
    let (store_name, store_root, report) = inbox_context(&context, StoreAccess::Read)?;
    let (item, parsed) = curation::show_inbox_item(&store_root, &report.entries, &args.note_id)?;
    if args.json {
        let output = InboxShowOutput {
            store: store_name,
            item,
            body: parsed.body,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("store: {store_name}");
        println!("id: {}", item.entry.id);
        println!("promoted: {}", item.promoted);
        println!("note: {}", item.entry.note_path);
        println!("created_at: {}", item.entry.created_at);
        println!();
        print!("{}", parsed.body);
        if !parsed.body.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

fn run_eval(command: EvalCommand, _context: CliContext) -> Result<()> {
    match command {
        EvalCommand::Retrieval(args) => run_eval_retrieval(args),
        EvalCommand::CaptureMiss(args) => run_eval_capture_miss(args),
        EvalCommand::CaptureBadHit(args) => run_eval_capture_bad_hit(args),
    }
}

fn run_eval_retrieval(args: EvalRetrievalArgs) -> Result<()> {
    let report = memory_eval::run_retrieval_eval(memory_eval::RetrievalEvalInput {
        corpus_path: args.corpus,
        limit: args.limit,
    })?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_retrieval_eval_report(&report, args.limit);
    }
    Ok(())
}

fn print_retrieval_eval_report(report: &memory_eval::RetrievalEvalReport, limit: usize) {
    println!("corpus: {}", report.corpus);
    for candidate in &report.candidates {
        println!("candidate: {}", candidate.name);
        for metric in &candidate.features {
            println!(
                "  {} cases={} recall@{}={:.3} precision@{}={:.3} mrr={:.3} forbidden_hits={} p95_ms={}",
                metric.feature,
                metric.cases,
                limit,
                metric.recall_at_k,
                limit,
                metric.precision_at_k,
                metric.mrr,
                metric.forbidden_hits,
                metric.p95_ms
            );
        }
    }
}

fn run_eval_capture_miss(args: EvalCaptureMissArgs) -> Result<()> {
    let snippet = render_retrieval_case(EvalRetrievalCaseInput {
        common: &args.common,
        expected: &args.expected,
        forbidden: &args.forbidden,
        note: "Captured from hm eval capture-miss; verify labels before relying on this case.",
    })?;
    emit_eval_capture(&args.common, snippet)
}

fn run_eval_capture_bad_hit(args: EvalCaptureBadHitArgs) -> Result<()> {
    let snippet = render_retrieval_case(EvalRetrievalCaseInput {
        common: &args.common,
        expected: &args.expected,
        forbidden: &args.bad,
        note: "Captured from hm eval capture-bad-hit; verify labels before relying on this case.",
    })?;
    emit_eval_capture(&args.common, snippet)
}

struct EvalRetrievalCaseInput<'a> {
    common: &'a EvalCaptureCommonArgs,
    expected: &'a [String],
    forbidden: &'a [String],
    note: &'a str,
}

#[derive(Debug, Serialize)]
struct EvalCaptureOutput {
    snippet: String,
    path: Option<String>,
    appended: bool,
}

fn render_retrieval_case(input: EvalRetrievalCaseInput<'_>) -> Result<String> {
    if input.common.prompt.trim().is_empty() {
        anyhow::bail!("--prompt must not be empty");
    }
    if input.common.feature.trim().is_empty() {
        anyhow::bail!("--feature must not be empty");
    }

    let name = input
        .common
        .name
        .clone()
        .unwrap_or_else(|| captured_case_name(&input.common.prompt));
    let mut snippet = String::new();
    snippet.push_str("[[retrieval_case]]\n");
    snippet.push_str(&format!("name = {}\n", toml_string(&name)?));
    snippet.push_str(&format!(
        "feature = {}\n",
        toml_string(&input.common.feature)?
    ));
    snippet.push_str(&format!("query = {}\n", toml_string(&input.common.prompt)?));
    if let Some(project_id) = input.common.project_id.as_deref() {
        if project_id.trim().is_empty() {
            anyhow::bail!("--project-id must not be empty when provided");
        }
        snippet.push_str(&format!("project_id = {}\n", toml_string(project_id)?));
    }
    snippet.push_str(&format!(
        "expected = {}\n",
        toml_string_list(input.expected)?
    ));
    snippet.push_str(&format!(
        "forbidden = {}\n",
        toml_string_list(input.forbidden)?
    ));
    snippet.push_str("target_recall_at_5 = 1.0\n");
    snippet.push_str("target_precision_at_5 = 1.0\n");
    snippet.push_str(&format!("note = {}\n", toml_string(input.note)?));
    Ok(snippet)
}

fn emit_eval_capture(common: &EvalCaptureCommonArgs, snippet: String) -> Result<()> {
    let mut appended = false;
    if let Some(path) = &common.to {
        append_eval_snippet(path, &snippet)?;
        appended = true;
    }

    if common.json {
        let output = EvalCaptureOutput {
            snippet,
            path: common.to.as_ref().map(|path| path.display().to_string()),
            appended,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print!("{snippet}");
        if !snippet.ends_with('\n') {
            println!();
        }
        if let Some(path) = &common.to {
            eprintln!("appended: {}", path.display());
        }
    }
    Ok(())
}

fn append_eval_snippet(path: &Path, snippet: &str) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let needs_separator = file.metadata()?.len() > 0;
    if needs_separator {
        writeln!(file)?;
    }
    file.write_all(snippet.as_bytes())?;
    Ok(())
}

fn captured_case_name(prompt: &str) -> String {
    let mut words = prompt
        .split_whitespace()
        .filter_map(|word| {
            let normalized = word
                .trim_matches(|ch: char| !ch.is_ascii_alphanumeric())
                .to_ascii_lowercase();
            (!normalized.is_empty()).then_some(normalized)
        })
        .take(8)
        .collect::<Vec<_>>();
    if words.is_empty() {
        words.push("prompt".to_owned());
    }
    format!("captured {}", words.join(" "))
}

fn toml_string(value: &str) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn toml_string_list(values: &[String]) -> Result<String> {
    let rendered = values
        .iter()
        .map(|value| toml_string(value))
        .collect::<Result<Vec<_>>>()?;
    Ok(format!("[{}]", rendered.join(", ")))
}

fn run_promote(args: PromoteArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent);
    let writer_agent_id = agent_id.clone().unwrap_or_else(|| "human".to_owned());
    let resolved_store = resolve_store(
        &config,
        context.store.as_deref(),
        None,
        agent_id.as_deref(),
        StoreAccess::Write,
    )?;
    resolve_store(
        &config,
        Some(resolved_store.name.as_str()),
        None,
        agent_id.as_deref(),
        StoreAccess::Read,
    )?;
    let store_config = &config.stores[resolved_store.name.as_str()];
    let manifest = read_store_manifest(&config, &resolved_store.name, store_config)?;
    let report = rebuild_store_index(&config, &resolved_store.name)?;
    let verbatim = if args.as_bullet { false } else { args.verbatim };
    let promotion = curation::promote(curation::PromotionInput {
        store_root: &store_config.root,
        manifest: &manifest,
        entries: &report.entries,
        note_id: &args.note_id,
        target: &args.to,
        verbatim,
        agent_id: &writer_agent_id,
        host_id: &resolve_host_id(&config),
        user_id: &config.user_id,
        options: hook_options(&config),
    })?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&promotion)?);
    } else {
        println!("store: {}", resolved_store.name);
        println!("note: {}", promotion.note_id);
        println!("target: {}", promotion.target_full_path.display());
        println!("promoted: {}", promotion.promoted);
        if let Some(path) = promotion.event_path {
            println!("event: {}", path.display());
        }
    }
    Ok(())
}

fn inbox_context(
    context: &CliContext,
    access: StoreAccess,
) -> Result<(String, PathBuf, index::LoadIndexReport)> {
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let resolved_store = resolve_store(
        &config,
        context.store.as_deref(),
        None,
        agent_id.as_deref(),
        access,
    )?;
    let store_root = config.stores[resolved_store.name.as_str()].root.clone();
    let report = rebuild_store_index(&config, &resolved_store.name)?;
    Ok((resolved_store.name, store_root, report))
}

fn print_inbox_list(store_name: &str, items: Vec<curation::InboxItem>, json: bool) -> Result<()> {
    if json {
        let output = InboxListOutput {
            store: store_name.to_owned(),
            items,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("store: {store_name}");
        println!("items: {}", items.len());
        for item in items {
            println!(
                "{}\t{}\t{}\t{}",
                item.entry.id,
                if item.promoted { "promoted" } else { "pending" },
                item.entry.created_at,
                item.entry.note_path
            );
        }
    }
    Ok(())
}

fn parse_sensitivity(input: &str) -> std::result::Result<Sensitivity, String> {
    Sensitivity::from_str(input)
        .map_err(|_| "expected one of: public, internal, private, secret".to_owned())
}

fn parse_confidence(input: &str) -> std::result::Result<note::Confidence, String> {
    match input {
        "low" => Ok(note::Confidence::Low),
        "medium" => Ok(note::Confidence::Medium),
        "high" => Ok(note::Confidence::High),
        _ => Err("expected one of: low, medium, high".to_owned()),
    }
}

fn parse_memory_kind(input: &str) -> std::result::Result<note::MemoryKind, String> {
    match input {
        "preference" => Ok(note::MemoryKind::Preference),
        "project-fact" => Ok(note::MemoryKind::ProjectFact),
        "incident" => Ok(note::MemoryKind::Incident),
        "reference" => Ok(note::MemoryKind::Reference),
        _ => Err("expected one of: preference, project-fact, incident, reference".to_owned()),
    }
}

fn memory_kind_label(kind: note::MemoryKind) -> &'static str {
    note::kind_label(kind)
}

#[derive(Debug, Clone, Copy)]
struct WriteKindDecision {
    kind: Option<note::MemoryKind>,
    inferred: bool,
    reason: Option<&'static str>,
}

#[derive(Debug, Clone)]
struct WriteScopeDecision {
    scope: String,
    inferred: bool,
    reason: Option<&'static str>,
}

fn resolve_write_scope(
    entry_kind: note::EntryKind,
    explicit: Option<&str>,
    no_infer: bool,
    default_scope: &str,
    project_id: Option<&str>,
    explicit_kind: Option<note::MemoryKind>,
    body: &str,
) -> WriteScopeDecision {
    if let Some(scope) = explicit {
        return WriteScopeDecision {
            scope: scope.to_owned(),
            inferred: false,
            reason: None,
        };
    }
    if no_infer || entry_kind != note::EntryKind::Remember {
        return WriteScopeDecision {
            scope: default_scope.to_owned(),
            inferred: false,
            reason: None,
        };
    }

    match write_classify::infer_scope(write_classify::InferScopeInput {
        project_id,
        explicit_kind,
        body,
    }) {
        Some(inference) => WriteScopeDecision {
            scope: inference.scope.to_owned(),
            inferred: true,
            reason: Some(inference.reason),
        },
        None => WriteScopeDecision {
            scope: default_scope.to_owned(),
            inferred: false,
            reason: None,
        },
    }
}

fn resolve_write_kind(
    entry_kind: note::EntryKind,
    explicit: Option<note::MemoryKind>,
    no_infer: bool,
    scope: &str,
    project_id: Option<&str>,
    body: &str,
) -> WriteKindDecision {
    if explicit.is_some() {
        return WriteKindDecision {
            kind: explicit,
            inferred: false,
            reason: None,
        };
    }
    if no_infer || entry_kind != note::EntryKind::Remember {
        return WriteKindDecision {
            kind: None,
            inferred: false,
            reason: None,
        };
    }

    match write_classify::infer_kind(write_classify::InferKindInput {
        scope,
        project_id,
        body,
    }) {
        Some(inference) => WriteKindDecision {
            kind: Some(inference.kind),
            inferred: true,
            reason: Some(inference.reason),
        },
        None => WriteKindDecision {
            kind: None,
            inferred: false,
            reason: None,
        },
    }
}

/// Load CLI-selected config and report non-fatal warnings.
///
/// The path resolution policy lives in `ConfigPaths`; this function only
/// connects that library contract to terminal diagnostics.
fn load_config(config_path: Option<&std::path::Path>) -> Result<Config> {
    let paths = ConfigPaths::resolve(config_path)?;
    let loaded = paths.load()?;
    for warning in &loaded.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(loaded.config)
}

/// Read a store manifest and remember the observed identity for offline writes.
///
/// The cache is only a future enqueue hint. It never weakens flush safety:
/// pending outbox items still carry the expected manifest id and `hm flush`
/// refuses to publish when the reachable store has a different id.
fn read_store_manifest(
    config: &Config,
    store_name: &str,
    store_config: &StoreConfig,
) -> Result<store::StoreManifest, store::StoreError> {
    let manifest = store::read_manifest(&store_config.root)?;
    if let Err(err) = outbox::record_store_identity(
        &config.data_dir,
        store_name,
        &manifest.store.id,
        &hook_options(config),
    ) {
        eprintln!("warning: failed to record store identity cache: {err}");
    }
    Ok(manifest)
}

fn known_store_identity(
    config: &Config,
    store_name: &str,
    store_config: &StoreConfig,
) -> Result<Option<String>> {
    // Configured `expected_id` is stronger than the observational cache because
    // it is a user-declared alias binding. The cache only helps a previously
    // seen store keep accepting offline writes when its root is unavailable.
    if let Some(expected_id) = &store_config.expected_id {
        return Ok(Some(expected_id.clone()));
    }
    Ok(outbox::cached_store_identity(&config.data_dir, store_name)?)
}

/// Resolve project identity only when the caller supplied project context.
///
/// Most commands should not guess a project from process CWD just because `hm`
/// happened to start inside a directory. Hooks and long-lived agents can move
/// across projects, so callers must provide a path hint or explicit/env project
/// id before project-scoped filtering or local store affinity applies.
fn resolve_project_id(
    explicit_project_id: Option<String>,
    project_hint: Option<&str>,
) -> Result<Option<String>> {
    let env_project_id = std::env::var("HIVE_MEMORY_PROJECT_ID").ok();
    let project_hint = project_hint.filter(|value| !value.trim().is_empty());
    if let Some(project_id) = explicit_project_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        && project_hint.is_none()
    {
        return Ok(Some(project_id.to_owned()));
    }
    if let Some(project_id) = env_project_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        && explicit_project_id.is_none()
        && project_hint.is_none()
    {
        return Ok(Some(project_id.to_owned()));
    }

    let hint = project_hint
        .map(PathBuf::from)
        .or_else(|| std::env::var("HIVE_MEMORY_PROJECT").ok().map(PathBuf::from));

    if explicit_project_id.is_none() && env_project_id.is_none() && hint.is_none() {
        return Ok(None);
    }

    Ok(Some(
        project::resolve_project(project::ResolveProjectInput {
            hint: hint.unwrap_or_default(),
            explicit_project_id,
            env_project_id,
        })?
        .project_id,
    ))
}

/// Return the local project binding store, if this invocation has a project id.
///
/// The caller still passes the returned alias through `resolve_store`; loading a
/// binding is only affinity discovery, not authorization.
fn project_binding_store(config: &Config, project_id: Option<&str>) -> Result<Option<String>> {
    let Some(project_id) = project_id else {
        return Ok(None);
    };
    Ok(project::load_binding(&config.data_dir, project_id)?.map(|binding| binding.store))
}

fn run_write_memory(
    entry_kind: note::EntryKind,
    args: WriteMemoryArgs,
    context: CliContext,
) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let writer_agent_id = agent_id.clone().unwrap_or_else(|| "human".to_owned());
    let project_hint = args
        .project
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    // Store affinity can come from a local project binding, so resolve project
    // identity before choosing the write store. This keeps work/personal routing
    // centralized in `hm` instead of requiring hook scripts or agents to infer it.
    let project_id = resolve_project_id(args.project_id.clone(), project_hint.as_deref())?;
    let scope_decision = resolve_write_scope(
        entry_kind,
        args.scope.as_deref(),
        args.no_infer_scope,
        &config.defaults.write_scope,
        project_id.as_deref(),
        args.kind,
        &args.text,
    );
    let scope = scope_decision.scope;
    let kind_decision = resolve_write_kind(
        entry_kind,
        args.kind,
        args.no_infer_kind,
        &scope,
        project_id.as_deref(),
        &args.text,
    );
    validate_memory_kind_context(kind_decision.kind, &scope, project_id.as_deref())?;
    let project_binding = project_binding_store(&config, project_id.as_deref())?;
    let resolved_store = resolve_store(
        &config,
        context.store.as_deref(),
        project_binding.as_deref(),
        agent_id.as_deref(),
        StoreAccess::Write,
    )?;
    let store_config = &config.stores[resolved_store.name.as_str()];
    validate_secret_write(
        &config,
        &context,
        store_config,
        args.allow_secret_write,
        &args.text,
    )?;
    let created_at = OffsetDateTime::now_utc();
    let host_id = resolve_host_id(&config);
    let audience = resolve_audience(&args, &scope, &writer_agent_id)?;
    validate_optional_rfc3339("--valid-from", args.valid_from.as_deref())?;
    validate_optional_rfc3339("--valid-to", args.valid_to.as_deref())?;
    validate_validity_window(args.valid_from.as_deref(), args.valid_to.as_deref())?;
    let supersedes = normalize_supersedes(args.supersedes)?;
    let should_write_event = match entry_kind {
        note::EntryKind::Remember => true,
        note::EntryKind::Note => {
            args.event
                || (!args.no_event && config.defaults.event_sidecar == EventSidecarPolicy::Always)
        }
    };
    let options = write::AtomicWriteOptions {
        fsync: config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    };
    let write_input = MemoryWriteFields {
        entry_kind,
        created_at,
        agent_id: writer_agent_id,
        host_id,
        user_id: config.user_id.clone(),
        session_id: std::env::var("HIVE_MEMORY_SESSION_ID").ok(),
        scope: scope.clone(),
        confidence: args.confidence,
        body: args.text,
        project_id: project_id.clone(),
        subject: args.subject,
        kind: kind_decision.kind,
        valid_from: args.valid_from,
        valid_to: args.valid_to,
        supersedes,
        tags: args.tags,
        audience: audience.clone(),
        source_kind: args.source_kind,
        source_ref: args.source_ref,
        write_event: should_write_event,
        options,
    };
    // Canonical writes stay the first choice. The outbox is used only when the
    // selected store is temporarily unreachable and policy explicitly permits a
    // local durable fallback; policy or manifest errors must not be hidden as
    // offline work.
    let outcome = match read_store_manifest(&config, &resolved_store.name, store_config) {
        Ok(manifest) => write_canonical_memory(&store_config.root, &manifest, write_input)?,
        Err(store::StoreError::Io { .. }) if config.offline.write_fallback_enabled() => {
            enqueue_outbox_memory(
                &config,
                store_config,
                &resolved_store.name,
                known_store_identity(&config, &resolved_store.name, store_config)?,
                write_input,
            )?
        }
        Err(store::StoreError::Io { .. }) => {
            return Err(BackendUnavailable {
                message: format!(
                    "store {} is unavailable and offline fallback is disabled",
                    resolved_store.name
                ),
            }
            .into());
        }
        Err(err) => return Err(err.into()),
    };

    if args.json {
        let output = WriteMemoryJson {
            id: outcome.id.clone(),
            store: resolved_store.name.clone(),
            store_id: outcome.store_id.clone(),
            store_source: resolved_store.source.to_string(),
            scope: scope.clone(),
            project_id: project_id.clone(),
            scope_inferred: scope_decision.inferred,
            scope_reason: scope_decision.reason.map(str::to_owned),
            audience: audience.clone(),
            kind: kind_decision.kind,
            kind_inferred: kind_decision.inferred,
            kind_reason: kind_decision.reason.map(str::to_owned),
            note_path: outcome.note_path.display().to_string(),
            event_path: outcome
                .event_path
                .as_ref()
                .map(|path| path.display().to_string()),
            created: true,
            duplicate_of: None,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("id: {}", outcome.id);
        println!("store: {}", resolved_store.name);
        if scope_decision.inferred {
            println!("scope: {} (inferred)", scope);
        }
        if let Some(kind) = kind_decision.kind {
            let suffix = if kind_decision.inferred {
                " (inferred)"
            } else {
                ""
            };
            println!("kind: {}{suffix}", memory_kind_label(kind));
        }
        println!("note: {}", outcome.note_path.display());
        if let Some(path) = &outcome.event_path {
            println!("event: {}", path.display());
        }
        if let Some(path) = &outcome.outbox_path {
            println!("outbox: {}", path.display());
        }
    }
    append_session_write_receipt(
        &config,
        &resolved_store.name,
        &scope,
        project_id,
        &outcome.id,
    );
    Ok(())
}

struct MemoryWriteFields {
    entry_kind: note::EntryKind,
    created_at: OffsetDateTime,
    agent_id: String,
    host_id: String,
    user_id: String,
    session_id: Option<String>,
    scope: String,
    confidence: note::Confidence,
    body: String,
    project_id: Option<String>,
    subject: Option<String>,
    kind: Option<note::MemoryKind>,
    valid_from: Option<String>,
    valid_to: Option<String>,
    supersedes: Vec<String>,
    tags: Vec<String>,
    audience: Vec<String>,
    source_kind: Option<String>,
    source_ref: Option<String>,
    write_event: bool,
    options: write::AtomicWriteOptions,
}

struct MemoryWriteOutcome {
    id: String,
    store_id: String,
    note_path: PathBuf,
    event_path: Option<PathBuf>,
    outbox_path: Option<PathBuf>,
}

fn write_canonical_memory(
    store_root: &std::path::Path,
    manifest: &store::StoreManifest,
    input: MemoryWriteFields,
) -> Result<MemoryWriteOutcome> {
    let result = memory::write_record(memory::WriteRecordInput {
        root: store_root,
        manifest,
        entry_kind: input.entry_kind,
        created_at: input.created_at,
        agent_id: input.agent_id,
        host_id: input.host_id,
        user_id: input.user_id,
        session_id: input.session_id,
        scope: input.scope,
        confidence: input.confidence,
        body: input.body,
        project_id: input.project_id,
        subject: input.subject,
        kind: input.kind,
        valid_from: input.valid_from,
        valid_to: input.valid_to,
        supersedes: input.supersedes,
        tags: input.tags,
        audience: input.audience,
        source_kind: input.source_kind,
        source_ref: input.source_ref,
        write_event: input.write_event,
        options: input.options,
    })?;

    Ok(MemoryWriteOutcome {
        id: result.id,
        store_id: manifest.store.id.clone(),
        note_path: result.note_path,
        event_path: result.event_path,
        outbox_path: None,
    })
}

fn enqueue_outbox_memory(
    config: &Config,
    store_config: &StoreConfig,
    store_name: &str,
    expected_store_id: Option<String>,
    input: MemoryWriteFields,
) -> Result<MemoryWriteOutcome> {
    let id = id::new_write_id(&id::WriteIdContext {
        host_id: input.host_id.clone(),
        agent_id: input.agent_id.clone(),
    });
    let write_event = input.write_event;
    let state = if expected_store_id.is_some() {
        outbox::OutboxState::Pending
    } else {
        outbox::OutboxState::Unbound
    };
    let payload_store_id = expected_store_id
        .clone()
        .unwrap_or_else(|| "unbound".to_owned());
    // Notes/events require a non-empty store id by schema. A never-seen
    // offline store has no trustworthy manifest id yet, so use an explicit
    // placeholder and keep the outbox state `unbound`; `hm flush --bind`
    // rewrites the payload before anything can be published canonically.
    // Outbox metadata is read before payloads are committed to the store, so it
    // must use the same store-relative path contract as canonical events and
    // indexes instead of relying on later filesystem discovery to normalize it.
    let path_case = memory_path::resolve_case(&config.storage.case_sensitive, &store_config.root);
    let note_relative_path = note::note_relative_path(&id, input.created_at);
    let event_relative_path = event::event_relative_path(&id, input.created_at);
    let note_input = note::NoteWriteInput {
        entry_kind: input.entry_kind,
        store_id: payload_store_id.clone(),
        store_name: store_name.to_owned(),
        created_at: input.created_at,
        agent_id: input.agent_id.clone(),
        host_id: input.host_id.clone(),
        scope: input.scope.clone(),
        confidence: input.confidence,
        body: input.body.clone(),
        user_id: Some(input.user_id.clone()),
        session_id: input.session_id.clone(),
        project_id: input.project_id.clone(),
        subject: input.subject.clone(),
        tags: input.tags.clone(),
        source_kind: input.source_kind.clone(),
        source_ref: input.source_ref.clone(),
        related_event_id: write_event.then(|| id.clone()),
        expires_at: None,
        valid_from: input.valid_from.clone(),
        valid_to: input.valid_to.clone(),
        supersedes: input.supersedes.clone(),
        kind: input.kind,
        classified: None,
        audience: input.audience.clone(),
    };
    let note = note::render_note(&note::MarkdownNote {
        front_matter: note_input.front_matter(id.clone())?,
        body: input.body.clone(),
    })?;
    let event = if write_event {
        Some(event::render_event(&event::MemoryEvent::observation(
            event::EventObservationInput {
                id: id.clone(),
                store_id: payload_store_id.clone(),
                store_name: store_name.to_owned(),
                created_at: input.created_at,
                agent_id: input.agent_id,
                host_id: input.host_id,
                user_id: Some(input.user_id),
                session_id: input.session_id,
                scope: input.scope,
                project_id: input.project_id,
                subject: input.subject,
                tags: input.tags,
                confidence: input.confidence,
                valid_from: input.valid_from,
                valid_to: input.valid_to,
                supersedes: input.supersedes,
                kind: input.kind,
                classified: None,
                audience: input.audience,
                body: input.body,
                note_path: Some(note_relative_path.clone()),
                source: input.source_kind.map(|kind| event::EventSource {
                    kind,
                    r#ref: input.source_ref,
                }),
            },
        )?)?)
    } else {
        None
    };
    let report = outbox::enqueue(outbox::EnqueueInput {
        data_dir: &config.data_dir,
        store: store_name,
        id: &id,
        expected_store_id: expected_store_id.clone(),
        final_note_path: store_relative_path_string(&note_relative_path, path_case),
        note: note.into_bytes(),
        final_event_path: write_event
            .then(|| store_relative_path_string(&event_relative_path, path_case)),
        event: event.map(String::into_bytes),
        state,
        options: input.options,
    })?;

    Ok(MemoryWriteOutcome {
        id,
        store_id: payload_store_id,
        note_path: store_config.root.join(note_relative_path),
        event_path: write_event.then(|| store_config.root.join(event_relative_path)),
        outbox_path: Some(report.item_dir),
    })
}

fn store_relative_path_string(path: &std::path::Path, path_case: memory_path::PathCase) -> String {
    memory_path::relative_string(path, path_case)
}

#[derive(Debug, Serialize)]
struct WriteMemoryJson {
    id: String,
    store: String,
    store_id: String,
    store_source: String,
    scope: String,
    project_id: Option<String>,
    scope_inferred: bool,
    scope_reason: Option<String>,
    audience: Vec<String>,
    kind: Option<note::MemoryKind>,
    kind_inferred: bool,
    kind_reason: Option<String>,
    note_path: String,
    event_path: Option<String>,
    created: bool,
    duplicate_of: Option<String>,
}

fn run_search(args: SearchArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent);
    let project_id = resolve_project_id(args.project_id, args.project.as_deref())?;
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

    let scopes = if args.scope.is_empty() {
        config.defaults.search_scopes.clone()
    } else {
        args.scope
    };
    let sources = if args.source.is_empty() {
        config.defaults.context_sources.clone()
    } else {
        args.source
    };
    let include_inbox = args.include_inbox
        || sources
            .iter()
            .any(|source| source == "inbox" || source == "all");

    let search_input = search::SearchInput {
        store_root: &store_config.root,
        entries: &report.entries,
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
) -> Result<Vec<search::SearchHit>> {
    if config
        .defaults
        .search_backend
        .trim()
        .eq_ignore_ascii_case("tantivy")
    {
        match tantivy_search(config, store_name, store_root, input.clone()) {
            Ok(hits) => return Ok(hits),
            Err(err) => {
                eprintln!(
                    "warning: full-text search backend unavailable ({err}); using lexical search"
                );
            }
        }
    }
    Ok(search::search(input)?)
}

/// Open (or create) the store's persistent Tantivy index, refresh it from the
/// current entries when their fingerprint changed, and run a policy-filtered
/// BM25 search. The index lives under the disposable cache dir, keyed by store.
fn tantivy_search(
    config: &Config,
    store_name: &str,
    store_root: &Path,
    input: search::SearchInput<'_>,
) -> std::result::Result<Vec<search::SearchHit>, search::SearchError> {
    let dir = config.cache_dir.join("search").join(store_name);
    let index = retrieval::SearchIndex::open_or_create_in_dir(&dir)
        .map_err(|err| search::SearchError::Retrieval(err.to_string()))?;
    let fingerprint = search::entries_fingerprint(input.entries);
    if !index.is_fresh(&fingerprint) {
        let documents = search::search_documents(store_root, input.entries)?;
        index
            .rebuild_tagged(&documents, Some(&fingerprint))
            .map_err(|err| search::SearchError::Retrieval(err.to_string()))?;
    }
    search::search_indexed(input, &index)
}

/// Hook-safe BM25 search: query the persistent index ONLY when it is already
/// fresh for `input.entries`. Never rebuilds — the prompt-submit hook must not
/// pay for a full index rebuild on its latency budget. Returns `None` (so the
/// caller falls back to lexical) when the backend is off, the index is
/// stale/absent, or the engine errors. The index is kept fresh out of band by
/// `hm refresh` (tool-complete) and interactive `hm search`.
fn tantivy_search_if_fresh(
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
fn refresh_tantivy_index(
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
    body_phrase: usize,
    body_terms: usize,
    metadata_phrase: usize,
    metadata_terms: usize,
    combined_phrase: usize,
    combined_terms: usize,
    entity: usize,
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
        body_phrase: trace.body_phrase,
        body_terms: trace.body_terms,
        metadata_phrase: trace.metadata_phrase,
        metadata_terms: trace.metadata_terms,
        combined_phrase: trace.combined_phrase,
        combined_terms: trace.combined_terms,
        entity: trace.entity,
        total: trace.total(),
    }
}

fn print_score_trace(trace: &search::SearchScoreTrace) {
    println!(
        "score_trace: body_phrase={} body_terms={} metadata_phrase={} metadata_terms={} combined_phrase={} combined_terms={} entity={} total={}",
        trace.body_phrase,
        trace.body_terms,
        trace.metadata_phrase,
        trace.metadata_terms,
        trace.combined_phrase,
        trace.combined_terms,
        trace.entity,
        trace.total()
    );
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

fn run_context(args: ContextArgs, context: CliContext) -> Result<()> {
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

struct ContextSelection {
    /// Explicit token budget. Missing means command-mode or hook-mode defaults.
    max_tokens: Option<usize>,
    /// Explicitly opt into lower-confidence raw inbox notes.
    include_inbox: bool,
    /// Explicitly render records the relevance strategy classifies as search-only.
    include_search_only: bool,
    /// Capture candidate-level selection decisions for JSON debugging.
    explain: bool,
    /// Scope filter from CLI/hook policy. Empty defers to config defaults.
    scopes: Vec<String>,
    /// Source filter from CLI/hook policy. Empty defers to config defaults.
    sources: Vec<String>,
    /// Project identity override. Missing can still resolve from env.
    project_id: Option<String>,
    /// Human path/project hint to render in the context header.
    path_hint: Option<String>,
}

struct CliContextAssembly {
    output: memory_context::ContextOutput,
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
fn assemble_cli_context(
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

#[derive(Debug, Serialize)]
struct SyncStatusJsonOutput {
    store: String,
    store_source: String,
    store_id: Option<String>,
    manifest_schema_version: Option<u32>,
    root: PathBuf,
    reachable: bool,
    manifest_error: Option<String>,
    index_path: PathBuf,
    index_exists: bool,
    index_modified_at: Option<String>,
    newest_note_at: Option<String>,
    newest_event_at: Option<String>,
    newest_canonical_at: Option<String>,
    index_stale: bool,
    cloud_conflict_files: usize,
    hosts: Vec<HostSyncStatus>,
}

/// Per-host activity summary derived from the local index.
#[derive(Debug, Serialize)]
struct HostSyncStatus {
    /// Host identity recorded on the indexed writes.
    host_id: String,
    /// RFC3339 timestamp of the newest indexed record from this host. Absent
    /// only when no row for the host carries a parseable timestamp.
    last_seen_at: Option<String>,
    /// Number of indexed records written by this host.
    records: usize,
}

/// Aggregate per-host last-seen activity from the existing scoped index.
///
/// Local-only checks cannot see that a remote machine's writes stopped
/// arriving through cloud sync; a per-host last-seen derived from synced
/// records is the cheap signal that one machine has gone silent. Reads the
/// same index file search and context use, deliberately without rebuilding:
/// the diagnostic stays read-only. A missing or unreadable index yields no
/// host rows; `index_exists`/`index_stale` already describe the cache state.
fn host_sync_status(index_path: &Path) -> Vec<HostSyncStatus> {
    let Ok(entries) = index::read_index(index_path) else {
        return Vec::new();
    };
    #[derive(Default)]
    struct Accumulator {
        last_seen: Option<(OffsetDateTime, String)>,
        records: usize,
    }
    let mut hosts = std::collections::BTreeMap::<String, Accumulator>::new();
    for entry in entries {
        // Rows from a pre-v4 cache schema carry no host identity; the
        // fingerprint bump rebuilds them on the next warm path.
        if entry.host_id.is_empty() {
            continue;
        }
        let slot = hosts.entry(entry.host_id).or_default();
        slot.records += 1;
        // Compare parsed timestamps, not strings: RFC3339 fractional-second
        // lengths make lexicographic order unreliable.
        if let Ok(created_at) = OffsetDateTime::parse(
            &entry.created_at,
            &time::format_description::well_known::Rfc3339,
        ) && slot
            .last_seen
            .as_ref()
            .is_none_or(|(best, _)| created_at > *best)
        {
            slot.last_seen = Some((created_at, entry.created_at));
        }
    }
    hosts
        .into_iter()
        .map(|(host_id, accumulator)| HostSyncStatus {
            host_id,
            last_seen_at: accumulator.last_seen.map(|(_, raw)| raw),
            records: accumulator.records,
        })
        .collect()
}

/// JSON envelope for `hm retag`.
#[derive(Debug, Serialize)]
struct RetagJsonOutput {
    /// Corrected record id.
    id: String,
    /// Store that holds the record.
    store: String,
    /// Kind carried before the rewrite.
    previous_kind: Option<&'static str>,
    /// Kind persisted by the rewrite; absent when cleared.
    kind: Option<&'static str>,
    /// Store-relative note path that was rewritten.
    note_path: String,
    /// Whether a paired event sidecar was rewritten too.
    event_updated: bool,
}

/// JSON envelope for `hm classify --pending`.
#[derive(Debug, Serialize)]
struct ClassifyPendingJsonOutput {
    /// Store inspected for pending records.
    store: String,
    /// Whether a backend was invoked. Always false for this read-only mode.
    backend_invoked: bool,
    /// Pending records before applying the display limit.
    pending: usize,
    /// Optional display cap supplied by `--limit`.
    limit: Option<u32>,
    /// Pending records shown in this response.
    records: Vec<ClassifyPendingRecord>,
}

/// One pending classifier record in JSON output.
#[derive(Debug, Serialize)]
struct ClassifyPendingRecord {
    /// Durable memory record id.
    id: String,
    /// Store-relative note path.
    note_path: String,
    /// Current persisted kind, when any.
    kind: Option<&'static str>,
    /// Memory scope.
    scope: String,
    /// Project id for project-scoped records.
    project_id: Option<String>,
}

/// Correct the persisted kind on one existing record.
///
/// This is the recovery path for wrong write-time inference: kind is a
/// persisted search-only/always-on verdict, and without a retag command the
/// only fix is hand-editing cloud-synced Markdown. The rewrite goes through
/// the same note+event pair as ordinary writes so the index (which prefers
/// event metadata) converges on the corrected value at the next rebuild.
fn run_retag(args: RetagArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let resolved_store = resolve_store(
        &config,
        context.store.as_deref(),
        None,
        agent_id.as_deref(),
        StoreAccess::Write,
    )?;
    let store_config = &config.stores[resolved_store.name.as_str()];
    let kind = match args.kind.as_str() {
        "none" => None,
        other => Some(
            parse_memory_kind(other)
                .map_err(|message| anyhow::anyhow!("invalid --kind: {message}, or none"))?,
        ),
    };

    let report = rebuild_store_index(&config, &resolved_store.name)?;
    let entry = report
        .entries
        .iter()
        .find(|entry| entry.id == args.id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no memory record with id {} in store {}",
                args.id,
                resolved_store.name
            )
        })?;
    // Same invariant as write time: kind and scope must not disagree, or a
    // project fact would classify as project-scoped without a project filter.
    validate_memory_kind_context(kind, &entry.scope, entry.project_id.as_deref())?;

    let options = write::AtomicWriteOptions {
        fsync: config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    };
    let classified = match kind {
        Some(_) => memory::ClassifiedUpdate::Set(note::ClassifiedBy {
            source: note::ClassifierSource::Manual,
            backend: None,
            at: now_rfc3339(),
            verdict_version: 0,
            confidence: None,
        }),
        None => memory::ClassifiedUpdate::Clear,
    };
    let result = memory::retag_record(memory::RetagRecordInput {
        root: &store_config.root,
        note_path: &entry.note_path,
        kind,
        classified,
        options,
    })?;

    if args.json {
        let output = RetagJsonOutput {
            id: result.id,
            store: resolved_store.name.clone(),
            previous_kind: result.previous_kind.map(memory_kind_label),
            kind: result.kind.map(memory_kind_label),
            note_path: entry.note_path.clone(),
            event_updated: result.event_path.is_some(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("id: {}", result.id);
        println!("store: {}", resolved_store.name);
        println!(
            "kind: {} -> {}",
            result.previous_kind.map_or("none", memory_kind_label),
            result.kind.map_or("none", memory_kind_label)
        );
        if result.event_path.is_some() {
            println!("event: updated");
        }
    }
    Ok(())
}

fn run_capture(args: CaptureArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let writer_agent_id = agent_id.clone().unwrap_or_else(|| "human".to_owned());

    // Resolve a model backend the same way the classifier does, so capture honors
    // the same [classifier] config and PATH preference order.
    let backend = llm::detect(
        config.classifier.backend.as_deref(),
        &config.classifier.command,
        config.classifier.model.as_deref(),
        None,
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "no usable model backend; configure [classifier] or install codex/claude/gemini"
        )
    })?;

    let conversation = match &args.text {
        Some(text) => text.clone(),
        None => {
            let mut buffer = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)?;
            buffer
        }
    };
    if conversation.trim().is_empty() {
        anyhow::bail!("no conversation text provided");
    }

    let timeout = std::time::Duration::from_secs(config.classifier.timeout_seconds);
    let facts = capture::extract(&backend, &conversation, timeout)
        .map_err(|err| anyhow::anyhow!("capture extraction failed: {err}"))?;

    if args.dry_run {
        if args.json {
            println!("{}", serde_json::to_string_pretty(&facts)?);
        } else {
            println!(
                "captured {} candidate fact(s) (dry run, nothing written):",
                facts.len()
            );
            for fact in &facts {
                println!("- {fact}");
            }
        }
        return Ok(());
    }

    if args.promote {
        return promote_captured_facts(PromoteCaptureInput {
            config: &config,
            context: &context,
            agent_id: agent_id.as_deref(),
            writer_agent_id: &writer_agent_id,
            backend: &backend,
            timeout,
            args: &args,
            facts: &facts,
        });
    }

    // Stage each fact as a raw inbox note. Inbox notes are excluded from context
    // by default, so capture can never silently change agent behavior; promoting
    // a staged note into durable memory is a separate, reviewed step.
    let resolved_store = resolve_store(
        &config,
        context.store.as_deref(),
        None,
        agent_id.as_deref(),
        StoreAccess::Write,
    )?;
    let store_config = &config.stores[resolved_store.name.as_str()];
    let manifest = read_store_manifest(&config, &resolved_store.name, store_config)?;
    let host_id = resolve_host_id(&config);
    let session_id = std::env::var("HIVE_MEMORY_SESSION_ID").ok();
    let options = write::AtomicWriteOptions {
        fsync: config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    };

    let mut written = Vec::new();
    for fact in &facts {
        let result = memory::write_record(memory::WriteRecordInput {
            root: &store_config.root,
            manifest: &manifest,
            entry_kind: note::EntryKind::Note,
            created_at: OffsetDateTime::now_utc(),
            agent_id: writer_agent_id.clone(),
            host_id: host_id.clone(),
            user_id: config.user_id.clone(),
            session_id: session_id.clone(),
            scope: config.defaults.write_scope.clone(),
            confidence: note::Confidence::Low,
            body: fact.clone(),
            project_id: None,
            subject: None,
            kind: None,
            valid_from: None,
            valid_to: None,
            supersedes: Vec::new(),
            tags: vec!["capture".to_owned()],
            audience: Vec::new(),
            source_kind: Some("capture".to_owned()),
            source_ref: args.source_ref.clone(),
            write_event: true,
            options: options.clone(),
        })?;
        written.push(result.id);
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&written)?);
    } else {
        println!(
            "staged {} captured fact(s) as inbox notes in store {}",
            written.len(),
            resolved_store.name
        );
    }
    Ok(())
}

/// Inputs for promoting a batch of captured facts into durable memory.
struct PromoteCaptureInput<'a> {
    config: &'a Config,
    context: &'a CliContext,
    agent_id: Option<&'a str>,
    writer_agent_id: &'a str,
    backend: &'a llm::Backend,
    timeout: std::time::Duration,
    args: &'a CaptureArgs,
    facts: &'a [String],
}

/// Reconcile each captured fact into durable memory (mem0-style
/// add/update/delete) using one shared index snapshot. Secret-looking facts are
/// skipped, never written. Reports per-fact operations.
fn promote_captured_facts(input: PromoteCaptureInput<'_>) -> Result<()> {
    let resolved_store = resolve_store(
        input.config,
        input.context.store.as_deref(),
        None,
        input.agent_id,
        StoreAccess::Write,
    )?;
    let store_config = &input.config.stores[resolved_store.name.as_str()];
    let manifest = read_store_manifest(input.config, &resolved_store.name, store_config)?;
    // Rebuild once so every candidate reconciles against the same snapshot; new
    // writes within the batch are not visible to later candidates, which keeps
    // the decision input deterministic for a single capture call.
    let report = rebuild_store_index(input.config, &resolved_store.name)?;

    let ctx = PromoteCtx {
        config: input.config,
        store_root: &store_config.root,
        manifest: &manifest,
        entries: &report.entries,
        backend: input.backend,
        agent_id: input.agent_id,
        writer_agent_id: input.writer_agent_id,
        limit: input.args.limit,
        timeout: input.timeout,
    };

    let mut outcomes: Vec<(String, &'static str, Option<String>)> = Vec::new();
    for fact in input.facts {
        // Defense-in-depth: capture's extraction already drops secret-looking
        // facts, but re-check here so a credential can never reach durable
        // memory through promotion. The body is redacted in the report so it is
        // not echoed to stdout (which a pipeline might log).
        if !secret::detect(fact).is_empty() {
            outcomes.push(("<redacted secret>".to_owned(), "skipped-secret", None));
            continue;
        }
        let operation = decide_candidate(&ctx, fact)?;
        let action = operation_action(&operation);
        let id = apply_decision(&ctx, fact, &operation, input.args.source_ref.as_deref())?;
        outcomes.push((fact.clone(), action, id));
    }

    let skipped = outcomes
        .iter()
        .filter(|(_, action, _)| *action == "skipped-secret")
        .count();
    let promoted = outcomes.len() - skipped;

    if input.args.json {
        let rows: Vec<_> = outcomes
            .iter()
            .map(|(fact, action, id)| {
                serde_json::json!({ "fact": fact, "operation": action, "id": id })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        if skipped > 0 {
            println!(
                "promoted {promoted} captured fact(s) into store {} ({skipped} skipped as secret):",
                resolved_store.name
            );
        } else {
            println!(
                "promoted {promoted} captured fact(s) into store {}:",
                resolved_store.name
            );
        }
        for (fact, action, id) in &outcomes {
            match id {
                Some(id) => println!("- {action} {id}: {fact}"),
                None => println!("- {action}: {fact}"),
            }
        }
    }
    Ok(())
}

/// Shared inputs for reconciling candidate facts against a store's durable
/// memory, resolved once so a batch (`hm capture --promote`) reuses one index
/// snapshot and backend across all candidates.
struct PromoteCtx<'a> {
    config: &'a Config,
    store_root: &'a Path,
    manifest: &'a store::StoreManifest,
    entries: &'a [index::IndexEntry],
    backend: &'a llm::Backend,
    agent_id: Option<&'a str>,
    writer_agent_id: &'a str,
    limit: usize,
    timeout: std::time::Duration,
}

/// Decide the mem0-style operation for one candidate against the store's nearest
/// durable memories.
fn decide_candidate(ctx: &PromoteCtx<'_>, candidate: &str) -> Result<reconcile::Operation> {
    let hits = search::search(search::SearchInput {
        store_root: ctx.store_root,
        entries: ctx.entries,
        query: candidate,
        scopes: &ctx.config.defaults.search_scopes,
        sources: &["remembered".to_owned()],
        include_inbox: false,
        agent_id: ctx.agent_id,
        project_id: None,
        limit: ctx.limit,
    })?;
    let existing: Vec<reconcile::ExistingMemory> = hits
        .iter()
        .map(|hit| reconcile::ExistingMemory {
            id: hit.entry.id.clone(),
            text: hit.entry.body.clone(),
        })
        .collect();
    reconcile::reconcile(ctx.backend, candidate, &existing, ctx.timeout)
        .map_err(|err| anyhow::anyhow!("reconcile decision failed: {err}"))
}

/// The stable action label for a reconcile operation.
fn operation_action(operation: &reconcile::Operation) -> &'static str {
    match operation {
        reconcile::Operation::Add => "add",
        reconcile::Operation::Update { .. } => "update",
        reconcile::Operation::Delete { .. } => "delete",
        reconcile::Operation::Noop => "noop",
    }
}

/// The existing record ids a decision supersedes. UPDATE/DELETE supersede their
/// target; ADD/NOOP supersede nothing. Single-sourced so the dry-run display and
/// the write path in [`apply_decision`] never drift.
fn supersede_targets(operation: &reconcile::Operation) -> Vec<String> {
    match operation {
        reconcile::Operation::Update { target } | reconcile::Operation::Delete { target } => {
            vec![target.clone()]
        }
        reconcile::Operation::Add | reconcile::Operation::Noop => Vec::new(),
    }
}

/// Apply a reconcile decision by writing the candidate as durable memory.
/// UPDATE/DELETE additionally supersede the target (retained for audit, never
/// hard-deleted). NOOP writes nothing. Returns the written record id, if any.
fn apply_decision(
    ctx: &PromoteCtx<'_>,
    candidate: &str,
    operation: &reconcile::Operation,
    source_ref: Option<&str>,
) -> Result<Option<String>> {
    if matches!(operation, reconcile::Operation::Noop) {
        return Ok(None);
    }
    let supersedes = supersede_targets(operation);
    let options = write::AtomicWriteOptions {
        fsync: ctx.config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    };
    let result = memory::write_record(memory::WriteRecordInput {
        root: ctx.store_root,
        manifest: ctx.manifest,
        entry_kind: note::EntryKind::Remember,
        created_at: OffsetDateTime::now_utc(),
        agent_id: ctx.writer_agent_id.to_owned(),
        host_id: resolve_host_id(ctx.config),
        user_id: ctx.config.user_id.clone(),
        session_id: std::env::var("HIVE_MEMORY_SESSION_ID").ok(),
        scope: ctx.config.defaults.write_scope.clone(),
        confidence: note::Confidence::Medium,
        body: candidate.to_owned(),
        project_id: None,
        subject: None,
        kind: None,
        valid_from: None,
        valid_to: None,
        supersedes,
        tags: vec!["reconciled".to_owned()],
        audience: Vec::new(),
        source_kind: Some("reconcile".to_owned()),
        source_ref: source_ref.map(str::to_owned),
        write_event: true,
        options,
    })?;
    Ok(Some(result.id))
}

fn run_reconcile(args: ReconcileArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let writer_agent_id = agent_id.clone().unwrap_or_else(|| "human".to_owned());

    let backend = llm::detect(
        config.classifier.backend.as_deref(),
        &config.classifier.command,
        config.classifier.model.as_deref(),
        None,
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "no usable model backend; configure [classifier] or install codex/claude/gemini"
        )
    })?;

    let candidate = match args.text {
        Some(text) => text,
        None => {
            let mut buffer = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)?;
            buffer
        }
    };
    let candidate = candidate.trim().to_owned();
    if candidate.is_empty() {
        anyhow::bail!("no candidate fact provided");
    }
    // Never let a credential reach durable memory through reconciliation.
    if !secret::detect(&candidate).is_empty() {
        anyhow::bail!("refusing to reconcile a candidate that looks like a secret");
    }

    let resolved_store = resolve_store(
        &config,
        context.store.as_deref(),
        None,
        agent_id.as_deref(),
        StoreAccess::Write,
    )?;
    let store_config = &config.stores[resolved_store.name.as_str()];
    let manifest = read_store_manifest(&config, &resolved_store.name, store_config)?;
    let report = rebuild_store_index(&config, &resolved_store.name)?;

    let timeout = std::time::Duration::from_secs(config.classifier.timeout_seconds);
    let ctx = PromoteCtx {
        config: &config,
        store_root: &store_config.root,
        manifest: &manifest,
        entries: &report.entries,
        backend: &backend,
        agent_id: agent_id.as_deref(),
        writer_agent_id: &writer_agent_id,
        limit: args.limit,
        timeout,
    };

    let operation = decide_candidate(&ctx, &candidate)?;
    let action = operation_action(&operation);
    let supersedes = supersede_targets(&operation);

    if args.dry_run || matches!(operation, reconcile::Operation::Noop) {
        if args.json {
            println!(
                "{}",
                serde_json::json!({ "operation": action, "supersedes": supersedes, "applied": false })
            );
        } else if matches!(operation, reconcile::Operation::Noop) {
            println!("noop: candidate already represented; nothing written");
        } else {
            println!("{action} (dry run): would write candidate, supersedes={supersedes:?}");
        }
        return Ok(());
    }

    let id = apply_decision(&ctx, &candidate, &operation, args.source_ref.as_deref())?
        .expect("non-noop decision always writes a record");

    if args.json {
        println!(
            "{}",
            serde_json::json!({ "operation": action, "id": id, "applied": true })
        );
    } else {
        println!("{action}: wrote {id} in store {}", resolved_store.name);
    }
    Ok(())
}

fn run_classify(args: ClassifyArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let store_access = if args.pending {
        StoreAccess::Read
    } else {
        StoreAccess::Write
    };
    let resolved_store = resolve_store(
        &config,
        context.store.as_deref(),
        None,
        agent_id.as_deref(),
        store_access,
    )?;
    let store_config = &config.stores[resolved_store.name.as_str()];
    let report = rebuild_store_index(&config, &resolved_store.name)?;

    if args.pending {
        emit_classify_pending(&args, &resolved_store.name, &report.entries)?;
        return Ok(());
    }

    let backends = classify::configured_backends(&config);
    let options = write::AtomicWriteOptions {
        fsync: config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    };
    let run_report = classify::run(classify::RunInput {
        config: &config,
        store_name: &resolved_store.name,
        store_root: &store_config.root,
        store_sensitivity: store_config.sensitivity,
        entries: &report.entries,
        backends,
        force: !args.auto,
        dry_run: args.dry_run,
        limit: args.limit,
        options,
    });

    if args.json {
        println!("{}", serde_json::to_string_pretty(&run_report)?);
        return Ok(());
    }

    match run_report.outcome {
        classify::Outcome::Ran => println!(
            "classified: pending={} judged={} applied={} marked_only={} errors={}",
            run_report.pending,
            run_report.judged,
            run_report.applied,
            run_report.marked_only,
            run_report.errors
        ),
        classify::Outcome::Aborted => {
            println!(
                "classifier aborted after {} backend errors; records remain pending",
                run_report.errors
            );
            if let Some(last_error) = &run_report.last_error {
                println!("last error: {last_error}");
            }
        }
        classify::Outcome::SkippedDisabled => println!("classifier: disabled"),
        classify::Outcome::SkippedNoBackend => println!("classifier: no backend detected"),
        classify::Outcome::SkippedLocked => println!("classifier: already running"),
        classify::Outcome::SkippedFresh => println!("classifier: interval not elapsed"),
    }
    Ok(())
}

fn emit_classify_pending(
    args: &ClassifyArgs,
    store_name: &str,
    entries: &[index::IndexEntry],
) -> Result<()> {
    let pending = classify::pending_entries(entries, hive_memory::llm::VERDICT_VERSION);
    let limit = args.limit.map(|value| value as usize).unwrap_or(usize::MAX);
    let records: Vec<ClassifyPendingRecord> = pending
        .iter()
        .take(limit)
        .map(|entry| ClassifyPendingRecord {
            id: entry.id.clone(),
            note_path: entry.note_path.clone(),
            kind: entry.kind.map(memory_kind_label),
            scope: entry.scope.clone(),
            project_id: entry.project_id.clone(),
        })
        .collect();

    if args.json {
        let output = ClassifyPendingJsonOutput {
            store: store_name.to_owned(),
            backend_invoked: false,
            pending: pending.len(),
            limit: args.limit,
            records,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("classifier pending: {}", pending.len());
        for record in records {
            println!(
                "{}\t{}\t{}\t{}",
                record.id,
                record.kind.unwrap_or("none"),
                record.scope,
                record.note_path
            );
        }
    }
    Ok(())
}

fn run_sync_status(args: SyncStatusArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let resolved_store = resolve_store(
        &config,
        context.store.as_deref(),
        None,
        agent_id.as_deref(),
        StoreAccess::Read,
    )?;
    let store_config = &config.stores[resolved_store.name.as_str()];
    let manifest = store::read_manifest(&store_config.root);
    let (reachable, store_id, manifest_schema_version, manifest_error) = match manifest {
        Ok(manifest) => (
            true,
            Some(manifest.store.id),
            Some(manifest.schema_version),
            None,
        ),
        Err(err) => (false, None, None, Some(err.to_string())),
    };

    let notes_root = store_config.root.join("inbox/notes");
    let events_root = store_config.root.join("inbox/events");
    let newest_note = newest_file_mtime(&notes_root)?;
    let newest_event = newest_file_mtime(&events_root)?;
    let newest_canonical = [newest_note, newest_event].into_iter().flatten().max();
    let index_path =
        index::scoped_index_path(&config.cache_dir, &resolved_store.name, &store_config.root);
    let index_modified = file_mtime(&index_path)?;
    let index_exists = index_modified.is_some();
    let index_stale = match (newest_canonical, index_modified) {
        (Some(_), None) => true,
        (Some(canonical), Some(index_modified)) => canonical > index_modified,
        _ => false,
    };
    let cloud_conflict_files = count_conflict_files(&store_config.root)?;
    let hosts = host_sync_status(&index_path);

    let output = SyncStatusJsonOutput {
        store: resolved_store.name,
        store_source: resolved_store.source.to_string(),
        store_id,
        manifest_schema_version,
        root: store_config.root.clone(),
        reachable,
        manifest_error,
        index_path,
        index_exists,
        index_modified_at: system_time_rfc3339(index_modified),
        newest_note_at: system_time_rfc3339(newest_note),
        newest_event_at: system_time_rfc3339(newest_event),
        newest_canonical_at: system_time_rfc3339(newest_canonical),
        index_stale,
        cloud_conflict_files,
        hosts,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!("store: {} ({})", output.store, output.store_source);
    println!("root: {}", output.root.display());
    println!("reachable: {}", if output.reachable { "yes" } else { "no" });
    if let Some(error) = output.manifest_error.as_deref() {
        println!("manifest_error: {error}");
    }
    println!(
        "index: {} ({})",
        output.index_path.display(),
        if output.index_exists {
            "exists"
        } else {
            "missing"
        }
    );
    println!(
        "index_stale: {}",
        if output.index_stale { "yes" } else { "no" }
    );
    println!("cloud_conflict_files: {}", output.cloud_conflict_files);
    for host in &output.hosts {
        println!(
            "host {}: last_seen={} records={}",
            host.host_id,
            host.last_seen_at.as_deref().unwrap_or("unknown"),
            host.records
        );
    }
    Ok(())
}

fn file_mtime(path: &Path) -> Result<Option<SystemTime>> {
    match path.metadata() {
        Ok(metadata) => Ok(Some(metadata.modified()?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn newest_file_mtime(root: &Path) -> Result<Option<SystemTime>> {
    let mut newest = None;
    visit_files(root, &mut |path| {
        if let Some(modified) = file_mtime(path)? {
            newest = Some(newest.map_or(modified, |current: SystemTime| current.max(modified)));
        }
        Ok(())
    })?;
    Ok(newest)
}

fn count_conflict_files(root: &Path) -> Result<usize> {
    let mut count = 0;
    visit_files(root, &mut |path| {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.to_ascii_lowercase().contains("conflict"))
        {
            count += 1;
        }
        Ok(())
    })?;
    Ok(count)
}

fn visit_files<F>(root: &Path, visit: &mut F) -> Result<()>
where
    F: FnMut(&Path) -> Result<()>,
{
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            visit_files(&path, visit)?;
        } else if file_type.is_file() {
            visit(&path)?;
        }
    }
    Ok(())
}

fn system_time_rfc3339(value: Option<SystemTime>) -> Option<String> {
    value.map(|time| {
        OffsetDateTime::from(time)
            .format(&time::format_description::well_known::Rfc3339)
            .expect("RFC3339 formatting should not fail")
    })
}

fn rebuild_store_index(config: &Config, store_name: &str) -> Result<index::LoadIndexReport> {
    let store_config = &config.stores[store_name];
    let options = write::AtomicWriteOptions {
        fsync: config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    };
    // Read commands share one hot-path loader. It validates a cheap canonical
    // file fingerprint before reusing JSONL so hooks do not parse thousands of
    // notes on every session boundary, while file create/delete/rename changes
    // still invalidate the cache on the next read.
    let report = index::load_or_rebuild_index(index::LoadIndexInput {
        store_name,
        store_root: &store_config.root,
        cache_dir: &config.cache_dir,
        options,
        path_case: memory_path::resolve_case(&config.storage.case_sensitive, &store_config.root),
    })?;
    for warning in &report.warnings {
        eprintln!("warning: {}: {}", warning.path.display(), warning.message);
    }
    Ok(report)
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

fn run_refresh(args: RefreshArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let receipt_cursor = refresh_receipt_cursor(&config, &context)?;
    if let Some(cursor) = receipt_cursor.as_ref()
        && cursor.unrefreshed == 0
        && !args.force
    {
        let report = skipped_refresh_report(args.force);
        emit_refresh_report(&report, &args)?;
        return Ok(());
    }

    let _refresh_lock = if let Some(cursor) = receipt_cursor.as_ref() {
        match memory_hook::try_refresh_lock(
            &config.state_dir,
            &cursor.agent_id,
            &cursor.session_id,
        )? {
            Some(lock) => Some(lock),
            None => {
                let report = coalesced_refresh_report(args.force, cursor.unrefreshed);
                emit_refresh_report(&report, &args)?;
                return Ok(());
            }
        }
    } else {
        None
    };

    let mut report = perform_refresh(&config, args.force)?;
    if let Some(cursor) = receipt_cursor {
        report.write_receipts = cursor.unrefreshed;
        // `hm refresh` owns only maintenance idempotency. Memory-pending debt is
        // cleared by `hm hook tool-complete`, where the hook knows the tool
        // actually succeeded and a receipt should satisfy the prompt reminder.
        memory_hook::mark_receipts_refreshed(
            &config.state_dir,
            &cursor.session_id,
            cursor.receipt_count,
            false,
            &hook_options(&config),
        )?;
    }

    emit_refresh_report(&report, &args)
}

fn emit_refresh_report(report: &HookRefreshReport, args: &RefreshArgs) -> Result<()> {
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if !args.quiet {
        println!(
            "refresh: indexes={} flushed={} skipped={} failed={} unbound={} pending={} forced={} write_receipts={} refreshed={} coalesced={}",
            report.indexes,
            report.flushed,
            report.skipped,
            report.failed,
            report.unbound,
            report.pending,
            report.forced,
            report.write_receipts,
            report.refreshed,
            report.coalesced
        );
    }

    Ok(())
}

struct RefreshReceiptCursor {
    agent_id: String,
    session_id: String,
    receipt_count: usize,
    unrefreshed: usize,
}

/// Return hook-session receipt progress when refresh is running in hook mode.
///
/// Plain human `hm refresh` remains eager and deterministic. Only hook-active
/// refreshes use write receipts as a cheap idempotency cursor, because hooks may
/// call refresh after many tool boundaries where no memory write happened.
fn refresh_receipt_cursor(
    config: &Config,
    context: &CliContext,
) -> Result<Option<RefreshReceiptCursor>> {
    if !hook_active(context) {
        return Ok(None);
    }
    let Some(session_id) = context_session_id() else {
        return Ok(None);
    };
    let agent_id =
        resolve_agent_id(context.as_agent.clone()).unwrap_or_else(|| "unknown".to_owned());

    let receipts = memory_hook::load_write_receipts(&config.state_dir, &session_id)?;
    let state = memory_hook::load_state(&config.state_dir, &session_id)?;
    let unrefreshed = receipts.len().saturating_sub(state.refreshed_receipts);
    Ok(Some(RefreshReceiptCursor {
        agent_id,
        session_id,
        receipt_count: receipts.len(),
        unrefreshed,
    }))
}

fn run_flush(args: FlushArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    if let Some(item_id) = args.bind.as_deref() {
        let Some(store) = context.store.as_deref() else {
            anyhow::bail!("hm flush --bind requires --store <name>");
        };
        outbox::bind_item(outbox::BindInput {
            data_dir: &config.data_dir,
            stores: &config.stores,
            item_id,
            store,
            options: hook_options(&config),
        })?;
    }
    let report = outbox::flush(outbox::FlushInput {
        data_dir: &config.data_dir,
        stores: &config.stores,
        host_id: &resolve_host_id(&config),
        options: hook_options(&config),
    })?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if !args.quiet {
        println!(
            "flush: flushed={} skipped={} failed={} unbound={} pending={}",
            report.flushed, report.skipped, report.failed, report.unbound, report.pending
        );
        for item in &report.items {
            if item.result == "flushed" || item.result == "skipped" {
                continue;
            }
            println!(
                "{}\t{}\t{}\t{}",
                item.result, item.store, item.id, item.message
            );
        }
    }

    if report.failed > 0 {
        anyhow::bail!("flush failed for {} item(s)", report.failed);
    }
    Ok(())
}

fn perform_refresh(config: &Config, forced: bool) -> Result<HookRefreshReport> {
    // Refresh is the one maintenance command hooks need to call after writes.
    // Flushing first makes any locally queued memory visible to the index in
    // the same cycle without teaching hook scripts outbox policy.
    let flush = outbox::flush(outbox::FlushInput {
        data_dir: &config.data_dir,
        stores: &config.stores,
        host_id: &resolve_host_id(config),
        options: hook_options(config),
    })?;
    if flush.failed > 0 {
        anyhow::bail!("flush failed for {} item(s)", flush.failed);
    }

    let mut indexes = 0usize;
    for (store_name, store_config) in &config.stores {
        // Serialize the whole rebuild+publish (JSONL + Tantivy) for this store's
        // shared cache artifact under one host-local, cache-key-scoped lock so
        // concurrent `hm refresh` runs or lazy read rebuilds cannot redundantly
        // scan the store or fight over the Tantivy writer. If another rebuild
        // already holds the lock, skip this store: that other run is producing
        // the same artifact, so this is a safe coalesce, not a dropped update.
        let _rebuild_lock =
            match index::try_rebuild_lock(&config.cache_dir, store_name, &store_config.root)? {
                Some(lock) => lock,
                None => continue,
            };
        let report = rebuild_store_index(config, store_name)?;
        // Keep the full-text index fresh off the hot path so the prompt-submit
        // hook can query BM25 cheaply (it never rebuilds). No-op unless the
        // tantivy backend is enabled.
        // TODO(perf): this re-reads canonical notes that rebuild_store_index just
        // read to extract search documents; a later phase should share one
        // document-extraction pass between the JSONL and Tantivy indexes.
        refresh_tantivy_index(config, store_name, &store_config.root, &report.entries);
        indexes += 1;
    }

    Ok(HookRefreshReport {
        indexes,
        flushed: flush.flushed,
        skipped: flush.skipped,
        failed: flush.failed,
        unbound: flush.unbound,
        pending: flush.pending,
        forced,
        write_receipts: 0,
        refreshed: true,
        coalesced: false,
    })
}

/// Build the successful no-op report for receipt-aware hook refresh.
///
/// A skipped refresh means no writes happened since the last consumed receipt,
/// so there is no maintenance work to do and no receipt cursor to advance.
fn skipped_refresh_report(forced: bool) -> HookRefreshReport {
    HookRefreshReport {
        indexes: 0,
        flushed: 0,
        skipped: 0,
        failed: 0,
        unbound: 0,
        pending: 0,
        forced,
        write_receipts: 0,
        refreshed: false,
        coalesced: false,
    }
}

/// Build the successful coalesced report for overlapping hook refreshes.
///
/// Coalescing must leave receipts unconsumed. The refresh holding the lock is
/// responsible for advancing the cursor after it completes successfully.
fn coalesced_refresh_report(forced: bool, write_receipts: usize) -> HookRefreshReport {
    HookRefreshReport {
        indexes: 0,
        flushed: 0,
        skipped: 0,
        failed: 0,
        unbound: 0,
        pending: 0,
        forced,
        write_receipts,
        refreshed: false,
        coalesced: true,
    }
}

fn run_hook(command: HookCommand, context: CliContext) -> Result<()> {
    match command {
        HookCommand::SessionStart(args) => run_hook_session_start(args, context),
        HookCommand::PromptSubmit(args) => run_hook_prompt_submit(args, context),
        HookCommand::ToolComplete(args) => run_hook_tool_complete(args, context),
        HookCommand::Stop(args) => run_hook_stop(args, context),
    }
}

/// Emit startup memory context for agent hooks.
///
/// The hook interface is deliberately policy-light for callers: dotfiles hooks
/// pass the project hint they already know, and `hm` resolves agent identity,
/// store affinity, source defaults, and context budgeting from config/env.
fn run_hook_session_start(args: HookContextArgs, mut context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    if context.as_agent.is_none() {
        context.as_agent = std::env::var("HIVE_MEMORY_AGENT_ID").ok();
    }
    let mut warnings = Vec::new();
    let path_hint = args
        .project
        .or_else(|| std::env::var("HIVE_MEMORY_PROJECT").ok());
    let assembly = assemble_cli_context(
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
fn run_hook_prompt_submit(args: HookPromptSubmitArgs, context: CliContext) -> Result<()> {
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
fn run_hook_tool_complete(args: HookToolCompleteArgs, context: CliContext) -> Result<()> {
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

            let mut report = perform_refresh(&config, false)?;
            report.write_receipts = unrefreshed_receipts;
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
fn run_hook_stop(args: HookStopArgs, context: CliContext) -> Result<()> {
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
    refresh: Option<HookRefreshReport>,
    /// Prompt-specific recall diagnostics.
    recall: Option<HookRecallReport>,
}

#[derive(Debug, Serialize)]
struct HookRefreshReport {
    /// Number of configured store indexes rebuilt.
    indexes: usize,
    /// Outbox items newly published before indexing.
    flushed: usize,
    /// Outbox items removed because identical payloads were already present.
    skipped: usize,
    /// Outbox items that hit an unsafe consistency or policy problem.
    failed: usize,
    /// Outbox items left for explicit human/store binding.
    unbound: usize,
    /// Outbox items left for retry because their store root is unavailable.
    pending: usize,
    /// Whether the caller requested a force refresh.
    forced: bool,
    /// New session write receipts consumed by this refresh.
    write_receipts: usize,
    /// Stable boolean for hook adapters that only need success/failure state.
    refreshed: bool,
    /// Whether another hook refresh was already running for this session.
    coalesced: bool,
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

fn context_session_id() -> Option<String> {
    std::env::var("HIVE_MEMORY_SESSION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
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

    let assembly = assemble_cli_context(
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
    let report = match load_cached_store_index(config, &store_name) {
        Ok(Some(report)) => report,
        Ok(None) => {
            let mut recall = HookRecallReport::skipped("index-not-fresh");
            recall.retrieval_ms = started.elapsed().as_millis();
            return Ok((None, recall));
        }
        Err(err) => {
            let mut recall = HookRecallReport::skipped("index-unavailable");
            recall.retrieval_ms = started.elapsed().as_millis();
            eprintln!("warning: prompt recall skipped: {err}");
            return Ok((None, recall));
        }
    };
    let search_input = search::SearchInput {
        store_root: &store_config.root,
        entries: &report.entries,
        query: &query,
        scopes: &config.defaults.search_scopes,
        sources: &["remembered".to_owned()],
        include_inbox: false,
        agent_id: agent_id.as_deref(),
        project_id: project_id.as_deref(),
        limit: 10,
    };
    // Prefer BM25 recall when the persistent index is already fresh; this is
    // where the prompt hook gains paraphrase/multi-session recall. Fall back to
    // the lexical scan when the index is stale/absent so the hook never pays for
    // a rebuild on its latency budget (refresh/tool-complete keeps it fresh).
    let hits = match tantivy_search_if_fresh(config, &store_name, search_input.clone()) {
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
    let output = memory_context::assemble_context(memory_context::ContextInput {
        store_name: store_name.as_str(),
        store_root: &store_config.root,
        entries: &selected_entries,
        scopes: &config.defaults.search_scopes,
        sources: &["remembered".to_owned()],
        include_inbox: false,
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
fn context_selection_key(
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

struct ContextKeyPolicy<'a> {
    include_inbox: bool,
    include_search_only: bool,
    strategy: &'a str,
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

    Ok(context_selection_key(
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

#[cfg(test)]
mod tests {
    use super::{context_selection_key, normalize_supersedes, validate_validity_window};

    #[test]
    fn context_key_tracks_inbox_and_search_only_policy() {
        let stores = ["personal".to_owned()];
        let scopes = ["global".to_owned()];
        let sources = ["remembered".to_owned()];
        let strict = context_selection_key(
            "codex",
            &stores,
            Some("project-a"),
            Some("/repo"),
            &scopes,
            &sources,
            super::ContextKeyPolicy {
                include_inbox: false,
                include_search_only: false,
                strategy: "relevance",
            },
        );
        let inbox = context_selection_key(
            "codex",
            &stores,
            Some("project-a"),
            Some("/repo"),
            &scopes,
            &sources,
            super::ContextKeyPolicy {
                include_inbox: true,
                include_search_only: true,
                strategy: "relevance",
            },
        );

        assert_ne!(strict, inbox);
        assert!(strict.contains("include_inbox=false"));
        assert!(inbox.contains("include_search_only=true"));
    }

    #[test]
    fn validity_window_rejects_inverted_range() {
        let err =
            validate_validity_window(Some("2030-01-01T00:00:00Z"), Some("2020-01-01T00:00:00Z"))
                .expect_err("inverted window rejected");

        assert!(err.to_string().contains("--valid-from must be earlier"));
    }

    #[test]
    fn supersedes_ids_are_trimmed_deduplicated_and_non_empty() {
        assert_eq!(
            normalize_supersedes(vec![
                " old-id ".to_owned(),
                "old-id".to_owned(),
                "new-id".to_owned(),
            ])
            .expect("supersedes"),
            vec!["old-id".to_owned(), "new-id".to_owned()]
        );
        assert!(normalize_supersedes(vec![" ".to_owned()]).is_err());
    }
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

fn hook_options(config: &Config) -> write::AtomicWriteOptions {
    write::AtomicWriteOptions {
        fsync: config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    }
}

fn append_session_write_receipt(
    config: &Config,
    store: &str,
    scope: &str,
    project_id: Option<String>,
    note_id: &str,
) {
    let Ok(session_id) = std::env::var("HIVE_MEMORY_SESSION_ID") else {
        return;
    };
    if session_id.trim().is_empty() {
        return;
    }

    let result = memory_hook::append_write_receipt(
        &config.state_dir,
        &session_id,
        &memory_hook::WriteReceipt {
            created_at: OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .expect("RFC3339 formatting should not fail"),
            store: store.to_owned(),
            scope: scope.to_owned(),
            project_id,
            note_id: note_id.to_owned(),
            created: true,
        },
    );
    if let Err(err) = result {
        // Receipts are ephemeral hook coordination state. The canonical memory
        // write has already succeeded, so receipt loss should warn but never
        // make a successful `hm remember`/`hm note` look failed.
        eprintln!("warning: failed to write session receipt: {err}");
    }
}

fn memory_intent_reminder() -> &'static str {
    "Hive Memory: this prompt sounds like durable memory intent. If it remains useful, write it with `hm remember --scope project --text \"...\"` or `hm remember --text \"...\"`."
}

fn stop_memory_reminder() -> &'static str {
    "Hive Memory: durable memory intent is still pending. Before ending, write any lasting preference, decision, or project fact with `hm remember`."
}

/// Print configured store inventory.
///
/// The JSON form is intentionally policy-aware when an agent id is active:
/// hooks and host integrations can ask `hm` whether a store is readable or
/// writable instead of duplicating the effective-agent-policy fallback rules.
fn list_stores(config: &Config, agent_id: Option<String>, json: bool) -> Result<()> {
    let policy = agent_id
        .as_deref()
        .map(|agent_id| config.effective_agent_policy(agent_id));
    if json {
        let output = config
            .stores
            .iter()
            .map(|(name, store)| store_list_json(config, name, store, policy.as_ref()))
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    for (name, store) in &config.stores {
        let available = if store.root.join("manifest.toml").is_file() {
            "available"
        } else {
            "missing"
        };
        println!("{name}\t{}\t{available}", store.root.display());
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct StoreListJson {
    /// Local store alias from config.
    name: String,
    /// Stable manifest identity when a parseable manifest is present.
    store_id: Option<String>,
    /// Configured store root.
    root: String,
    /// Whether the root currently advertises a manifest file.
    ///
    /// This mirrors the human `stores list` availability check. Manifest
    /// validity remains a doctor/show concern so list output stays cheap and
    /// tolerant of temporarily broken stores.
    available: bool,
    /// Whether this is config.default_store.
    default: bool,
    /// Configured sensitivity, available even when the store root is offline.
    sensitivity: Sensitivity,
    /// Whether the active agent can read this store; null without agent policy.
    readable: Option<bool>,
    /// Whether the active agent can write this store; null without agent policy.
    writable: Option<bool>,
    /// Whether this is the active agent's resolved default store.
    default_for_agent: Option<bool>,
}

fn store_list_json(
    config: &Config,
    name: &str,
    store: &StoreConfig,
    policy: Option<&config::EffectiveAgentPolicy>,
) -> StoreListJson {
    let manifest = store::read_manifest(&store.root).ok();
    let readable = policy.map(|policy| {
        policy.allow_all_stores || policy.read_stores.iter().any(|allowed| allowed == name)
    });
    let writable = policy.map(|policy| {
        policy.allow_all_stores || policy.write_stores.iter().any(|allowed| allowed == name)
    });
    StoreListJson {
        name: name.to_owned(),
        store_id: manifest.map(|manifest| manifest.store.id),
        root: store.root.display().to_string(),
        available: store.root.join("manifest.toml").is_file(),
        default: config.default_store == name,
        sensitivity: store.sensitivity,
        readable,
        writable,
        default_for_agent: policy.map(|policy| policy.default_store == name),
    }
}

/// Print one store's configured values plus current manifest state.
///
/// Human output preserves the compact diagnostic shape used by early store
/// commands. JSON output exposes both config and manifest because automation
/// often needs to distinguish "configured here" from "identity present there."
fn show_store(
    config: &Config,
    name: Option<&str>,
    agent_id: Option<String>,
    json: bool,
) -> Result<()> {
    let name = name.unwrap_or(&config.default_store);
    let Some(store) = config.stores.get(name) else {
        anyhow::bail!("unknown store: {name}");
    };

    if json {
        let manifest = store::read_manifest(&store.root).ok();
        let output = StoreShowJson {
            name: name.to_owned(),
            config: store_config_json(store),
            manifest,
            available: store.root.join("manifest.toml").is_file(),
            effective_agent_policy: agent_id.as_deref().map(|agent_id| {
                effective_agent_policy_json(config.effective_agent_policy(agent_id))
            }),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!("name: {name}");
    println!("root: {}", store.root.display());
    println!("sensitivity: {}", store.sensitivity);

    let manifest_path = store.root.join("manifest.toml");
    if manifest_path.is_file() {
        let manifest = store::read_manifest(&store.root)?;
        println!("available: true");
        println!("manifest_id: {}", manifest.store.id);
        println!("manifest_name: {}", manifest.store.name);
    } else {
        println!("available: false");
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct StoreShowJson {
    /// Local store alias being inspected.
    name: String,
    /// Configured values for this alias.
    config: StoreConfigJson,
    /// Parsed manifest when the root has a valid manifest.
    manifest: Option<store::StoreManifest>,
    /// Whether the root currently advertises a manifest file.
    available: bool,
    /// Resolved agent policy when an agent id is active.
    effective_agent_policy: Option<EffectiveAgentPolicyJson>,
}

#[derive(Debug, Serialize)]
struct StoreConfigJson {
    /// Configured store root.
    root: String,
    /// Optional manifest id this alias expects.
    expected_id: Option<String>,
    /// Optional human-facing description.
    description: Option<String>,
    /// Configured sensitivity fallback.
    sensitivity: Sensitivity,
}

#[derive(Debug, Serialize)]
struct EffectiveAgentPolicyJson {
    /// Store selected by default for the active agent.
    default_store: String,
    /// Stores readable by the active agent.
    read_stores: Vec<String>,
    /// Stores writable by the active agent.
    write_stores: Vec<String>,
    /// Whether explicit all-store operations are permitted.
    allow_all_stores: bool,
}

fn store_config_json(store: &StoreConfig) -> StoreConfigJson {
    StoreConfigJson {
        root: store.root.display().to_string(),
        expected_id: store.expected_id.clone(),
        description: store.description.clone(),
        sensitivity: store.sensitivity,
    }
}

fn effective_agent_policy_json(policy: config::EffectiveAgentPolicy) -> EffectiveAgentPolicyJson {
    EffectiveAgentPolicyJson {
        default_store: policy.default_store,
        read_stores: policy.read_stores,
        write_stores: policy.write_stores,
        allow_all_stores: policy.allow_all_stores,
    }
}

fn run_store_doctor(config: &Config, name: Option<&str>, json: bool) -> Result<()> {
    let reports = doctor_reports(config, name)?;
    let mut has_error = false;

    if json {
        has_error = reports.iter().any(|report| {
            report
                .issues
                .iter()
                .any(|issue| issue.level == store::StoreDoctorLevel::Error)
        });
        println!("{}", serde_json::to_string_pretty(&reports)?);
        if has_error {
            anyhow::bail!("store doctor found errors");
        }
        return Ok(());
    }

    for report in &reports {
        println!("store: {}", report.name);
        println!("root: {}", report.root.display());
        println!(
            "manifest: {}",
            if report.manifest_available {
                "present"
            } else {
                "missing"
            }
        );
        if report.issues.is_empty() {
            println!("status: ok");
        } else {
            for issue in &report.issues {
                let level = match issue.level {
                    store::StoreDoctorLevel::Warning => "warning",
                    store::StoreDoctorLevel::Error => {
                        has_error = true;
                        "error"
                    }
                };
                println!("{level}: {}", issue.message);
            }
        }
    }

    if has_error {
        anyhow::bail!("store doctor found errors");
    }
    Ok(())
}

fn run_store_migrate(config: &Config, name: Option<&str>, dry_run: bool) -> Result<()> {
    let inputs = store_inputs(config, name)?;
    let report = store::migrate_stores(inputs, dry_run)?;
    println!("stores_checked: {}", report.stores_checked);
    println!("migrations_run: {}", report.migrations_run);
    println!("dry_run: {}", report.dry_run);
    if report.migrations_run == 0 {
        println!("status: no migrations for schema v1");
    }
    Ok(())
}

fn doctor_reports(config: &Config, name: Option<&str>) -> Result<Vec<store::StoreDoctorReport>> {
    Ok(store_inputs(config, name)?
        .into_iter()
        .map(store::doctor_store)
        .collect())
}

fn store_inputs<'a>(
    config: &'a Config,
    name: Option<&'a str>,
) -> Result<Vec<store::StoreDoctorInput<'a>>> {
    if let Some(name) = name {
        let Some(store) = config.stores.get(name) else {
            anyhow::bail!("unknown store: {name}");
        };
        return Ok(vec![store::StoreDoctorInput {
            name,
            config: store,
        }]);
    }

    Ok(config
        .stores
        .iter()
        .map(|(name, store)| store::StoreDoctorInput {
            name: name.as_str(),
            config: store,
        })
        .collect())
}

struct ResolvedStore {
    name: String,
    source: StoreSource,
}

#[derive(Debug, Clone, Copy)]
enum StoreAccess {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy)]
enum StoreSource {
    Cli,
    Env,
    ProjectBinding,
    AgentDefault,
    GlobalDefault,
}

impl std::fmt::Display for StoreSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Cli => "cli",
            Self::Env => "env",
            Self::ProjectBinding => "project-binding",
            Self::AgentDefault => "agent-default",
            Self::GlobalDefault => "global-default",
        };
        f.write_str(value)
    }
}

/// How store policy behaves when no agent identity is asserted.
///
/// Agent identity is self-asserted (`--as-agent`/`HIVE_MEMORY_AGENT_ID`), so
/// these are defense-in-depth choices, not cryptographic boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoIdentityPolicy {
    /// Apply the global default store's conservative policy when no identity is
    /// present: the default store stays usable, but a NON-default store is
    /// refused. This closes the bypass where a restricted agent drops
    /// `--as-agent` to reach a store outside its allowlist. Used by the memory
    /// read/write commands (`remember`, `note`, `search`, `context`, `promote`,
    /// `inbox`, `classify`, `reconcile`, `alias`, hooks).
    DefaultStoreOnly,
    /// Skip per-store policy entirely when no identity is present, allowing a
    /// human to select any configured store. Used only by local-affinity
    /// commands (`projects bind`/`resolve`) where the spec grants humans
    /// any-store access and agent affinity is still re-checked at memory use
    /// time. Agent identities, when present, are always enforced.
    AllowAnyStore,
}

/// Resolve the single store a CLI command should use and enforce agent policy.
///
/// All one-store commands share the same precedence: explicit `--store`, then
/// `HIVE_MEMORY_STORE`, then local project binding, then the active agent's
/// configured default store, then the global default. Centralizing that order
/// keeps read, write, context, and hook commands from drifting as the command
/// surface grows. Callers that do not have project context pass `None` for the
/// binding slot rather than trying to derive path policy locally.
///
/// This is the secure default entry point: when no agent identity is present it
/// applies `NoIdentityPolicy::DefaultStoreOnly`. Local-affinity commands that
/// must preserve human any-store access call `resolve_store_with_policy`.
fn resolve_store(
    config: &Config,
    explicit_store: Option<&str>,
    project_binding: Option<&str>,
    agent_id: Option<&str>,
    access: StoreAccess,
) -> Result<ResolvedStore> {
    resolve_store_with_policy(
        config,
        explicit_store,
        project_binding,
        agent_id,
        access,
        NoIdentityPolicy::DefaultStoreOnly,
    )
}

/// Resolve a store with an explicit no-identity policy. See `resolve_store`.
fn resolve_store_with_policy(
    config: &Config,
    explicit_store: Option<&str>,
    project_binding: Option<&str>,
    agent_id: Option<&str>,
    access: StoreAccess,
    no_identity_policy: NoIdentityPolicy,
) -> Result<ResolvedStore> {
    let (name, source) = if let Some(store) = explicit_store {
        (store.to_owned(), StoreSource::Cli)
    } else if let Ok(store) = std::env::var("HIVE_MEMORY_STORE") {
        (store, StoreSource::Env)
    } else if let Some(store) = project_binding {
        (store.to_owned(), StoreSource::ProjectBinding)
    } else if let Some(agent_id) = agent_id {
        (
            config.effective_agent_policy(agent_id).default_store,
            StoreSource::AgentDefault,
        )
    } else {
        (config.default_store.clone(), StoreSource::GlobalDefault)
    };

    let Some(_store) = config.stores.get(&name) else {
        anyhow::bail!("unknown store: {name}");
    };

    // Fail closed for missing identity WITHOUT breaking the human path.
    //
    // Agent identity is self-asserted (just `--as-agent`/`HIVE_MEMORY_AGENT_ID`),
    // so this is defense in depth, not a cryptographic boundary: a determined
    // process can claim any agent id. The bypass we close (for memory commands,
    // via `NoIdentityPolicy::DefaultStoreOnly`) is the cheaper one: previously,
    // when no identity was set, per-agent `read_stores`/`write_stores`
    // enforcement was skipped entirely while `--store`/`HIVE_MEMORY_STORE` could
    // still target ANY store. A restricted agent could therefore sidestep its
    // allowlist simply by NOT passing `--as-agent`.
    //
    // Rather than skip policy when no identity is present, we apply the global
    // default store's conservative policy: the default store stays usable (so a
    // plain human shell running `hm remember`/`hm search` with no `--as-agent`
    // keeps working), but reaching a NON-default restricted store with no
    // identity is refused. This mirrors the conservative missing-agent policy:
    // {read,write}_stores = [default_store], allow_all_stores = false.
    //
    // `NoIdentityPolicy::AllowAnyStore` opts the local-affinity commands out of
    // this no-identity tightening so a human keeps any-store access there, per
    // the spec; an asserted agent identity is enforced under either policy.
    let policy = match agent_id {
        Some(agent_id) => config.effective_agent_policy(agent_id),
        None => match no_identity_policy {
            NoIdentityPolicy::AllowAnyStore => return Ok(ResolvedStore { name, source }),
            NoIdentityPolicy::DefaultStoreOnly => config::EffectiveAgentPolicy {
                default_store: config.default_store.clone(),
                read_stores: vec![config.default_store.clone()],
                write_stores: vec![config.default_store.clone()],
                allow_all_stores: false,
            },
        },
    };
    let subject = agent_id.unwrap_or("no-identity caller");
    let (allowed_stores, access_name) = match access {
        StoreAccess::Read => (&policy.read_stores, "read"),
        StoreAccess::Write => (&policy.write_stores, "write"),
    };
    if !policy.allow_all_stores && !allowed_stores.iter().any(|store| store == &name) {
        return Err(PrivacyRefusal {
            message: format!(
                "agent {subject} may not {access_name} store {name}; configured {access_name} stores: {}",
                allowed_stores.join(",")
            ),
        }
        .into());
    }

    Ok(ResolvedStore { name, source })
}

fn resolve_agent_id(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| std::env::var("HIVE_MEMORY_AGENT_ID").ok())
}

/// Query the OS for the current machine's hostname via syscall.
///
/// On Unix this calls `gethostname(2)` directly. On other platforms it falls
/// back to the `COMPUTERNAME` environment variable, which Windows sets as a
/// genuine system variable (not shell-specific).
#[cfg(unix)]
fn system_hostname() -> Option<String> {
    // POSIX requires HOST_NAME_MAX >= 255; 256 bytes covers the null terminator.
    let mut buf = vec![0u8; 256];
    // SAFETY: buf is valid for buf.len() bytes and outlives this call.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec())
        .ok()
        .filter(|s| !s.is_empty())
}

#[cfg(not(unix))]
fn system_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

/// Resolve the host label written into memory metadata.
///
/// When `host_id` is `"auto"` the OS hostname is queried directly via
/// `gethostname(2)` on Unix, avoiding reliance on shell-exported variables
/// like `$HOSTNAME` that are not guaranteed to be present in subprocess
/// environments.
fn resolve_host_id(config: &Config) -> String {
    if config.host_id != "auto" {
        return config.host_id.clone();
    }
    system_hostname().unwrap_or_else(|| "unknown-host".to_owned())
}

fn validate_secret_write(
    config: &Config,
    context: &CliContext,
    store: &StoreConfig,
    allow_secret_write: bool,
    text: &str,
) -> Result<()> {
    let findings = secret::detect(text);
    if findings.is_empty() {
        return Ok(());
    }

    // Detector ids are safe to print; matched values are intentionally not part
    // of `SecretFinding` so command errors, hook JSON, and transcripts do not
    // re-leak the material the guard is trying to protect.
    let detector_ids = findings
        .iter()
        .map(|finding| finding.detector_id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    if !allow_secret_write {
        return Err(PrivacyRefusal {
            message: format!(
                "Hive Memory does not store likely secrets by default; detectors: {detector_ids}; rerun with --allow-secret-write only for intentional secret-store writes"
            ),
        }
        .into());
    }
    if store.sensitivity != Sensitivity::Secret {
        return Err(PrivacyRefusal {
            message: format!(
                "--allow-secret-write requires a resolved secret store; detectors: {detector_ids}"
            ),
        }
        .into());
    }
    if !config.privacy.allow_secret_writes {
        return Err(PrivacyRefusal {
            message: format!(
                "--allow-secret-write requires privacy.allow_secret_writes = true; detectors: {detector_ids}"
            ),
        }
        .into());
    }
    if hook_active(context) && !config.privacy.allow_hook_secret_writes {
        return Err(PrivacyRefusal {
            message: format!(
                "hook secret writes require privacy.allow_hook_secret_writes = true; detectors: {detector_ids}"
            ),
        }
        .into());
    }

    Ok(())
}

fn validate_optional_rfc3339(label: &str, value: Option<&str>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map(|_| ())
        .map_err(|err| anyhow::anyhow!("{label} must be RFC3339: {err}"))
}

fn parse_optional_rfc3339(label: &str, value: Option<&str>) -> Result<Option<OffsetDateTime>> {
    value
        .map(|value| {
            OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                .map_err(|err| anyhow::anyhow!("{label} must be RFC3339: {err}"))
        })
        .transpose()
}

fn validate_validity_window(valid_from: Option<&str>, valid_to: Option<&str>) -> Result<()> {
    let valid_from_time = parse_optional_rfc3339("--valid-from", valid_from)?;
    let valid_to_time = parse_optional_rfc3339("--valid-to", valid_to)?;
    if let (Some(valid_from_time), Some(valid_to_time)) = (valid_from_time, valid_to_time)
        && valid_from_time >= valid_to_time
    {
        return Err(anyhow::anyhow!(
            "--valid-from must be earlier than --valid-to"
        ));
    }
    Ok(())
}

fn normalize_supersedes(values: Vec<String>) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            return Err(anyhow::anyhow!("--supersedes must not be empty"));
        }
        if seen.insert(value.to_owned()) {
            normalized.push(value.to_owned());
        }
    }
    Ok(normalized)
}

fn resolve_audience(
    args: &WriteMemoryArgs,
    scope: &str,
    writer_agent_id: &str,
) -> Result<Vec<String>> {
    // Non-private scopes do not carry an audience, even if the caller supplied
    // one. Visibility is easier to audit when audience has one meaning:
    // narrowing `agent-private` records.
    if scope != "agent-private" {
        return Ok(Vec::new());
    }

    if args.audience_writer_only {
        return Ok(vec![writer_agent_id.to_owned()]);
    }

    if args.audience.is_empty() {
        return Err(PrivacyRefusal {
            message: "agent-private writes require --audience or --audience-writer-only".to_owned(),
        }
        .into());
    }

    Ok(args.audience.clone())
}

fn validate_memory_kind_context(
    kind: Option<note::MemoryKind>,
    scope: &str,
    project_id: Option<&str>,
) -> Result<()> {
    memory::validate_kind_context(kind, scope, project_id).map_err(|err| {
        PrivacyRefusal {
            message: err.to_string(),
        }
        .into()
    })
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting is infallible for UTC timestamps")
}
