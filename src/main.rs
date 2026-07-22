//! `hm` command-line entry point.
//!
//! Keep this binary thin: the CLI is the user-facing shell contract, while
//! reusable policy and data handling live in the library so hooks and future
//! embedded callers do not need to shell out to themselves.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use hive_memory::config::{Config, ConfigPaths, EventSidecarPolicy, Sensitivity, StoreConfig};
use hive_memory::{
    capture, classify, config, doctor, event, hook as memory_hook, id, index, llm, memory, note,
    outbox, path as memory_path, project, reconcile, search, secret, store, visibility, write,
    write_classify,
};
use serde::Serialize;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{self, Display};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

mod cli;

use cli::{
    context::ContextArgs,
    curation::{InboxCommand, PromoteArgs},
    eval::EvalCommand,
    hook::HookCommand,
    projects::ProjectsCommand,
    search::SearchArgs,
    stores::StoresCommand,
    sync::{FlushArgs, OutboxCommand, RefreshArgs},
    sync_status::SyncStatusArgs,
};

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
    /// Search curated and remembered memory.
    Search(SearchArgs),
    /// Assemble agent-readable memory context.
    Context(ContextArgs),
    /// Report store/index freshness without mutating memory.
    SyncStatus(SyncStatusArgs),
    /// Correct persisted kind, scope, or project metadata on a record.
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

/// Arguments for `hm retag`.
#[derive(Debug, Args)]
struct RetagArgs {
    /// Memory record id to correct.
    id: String,
    /// New kind: preference, project-fact, incident, reference, or `none` to
    /// clear the tag and fall back to read-time classification.
    #[arg(long)]
    kind: Option<String>,
    /// New memory scope.
    #[arg(long)]
    scope: Option<String>,
    /// New explicit project id.
    #[arg(long, conflicts_with = "project")]
    project_id: Option<String>,
    /// Project path or file hint used to resolve the new project id.
    #[arg(long, conflicts_with = "project_id")]
    project: Option<PathBuf>,
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
        Some(Command::Stores(command)) => cli::stores::run(command, context),
        Some(Command::Remember(args)) => run_write_memory(note::EntryKind::Remember, args, context),
        Some(Command::Note(args)) => run_write_memory(note::EntryKind::Note, args, context),
        Some(Command::Search(args)) => cli::search::run(args, context),
        Some(Command::Context(args)) => cli::context::run(args, context),
        Some(Command::SyncStatus(args)) => cli::sync_status::run(args, context),
        Some(Command::Retag(args)) => run_retag(args, context),
        Some(Command::Classify(args)) => run_classify(args, context),
        Some(Command::Capture(args)) => run_capture(args, context),
        Some(Command::Reconcile(args)) => run_reconcile(args, context),
        Some(Command::Refresh(args)) => cli::sync::run_refresh(args, context),
        Some(Command::Flush(args)) => cli::sync::run_flush(args, context),
        Some(Command::Outbox(command)) => cli::sync::run_outbox(command, context),
        Some(Command::Projects(command)) => cli::projects::run(command, context),
        Some(Command::Hook(command)) => cli::hook::run(command, context),
        Some(Command::Doctor(args)) => run_doctor(args, context),
        Some(Command::Promote(args)) => cli::curation::run_promote(args, context),
        Some(Command::Inbox(command)) => cli::curation::run_inbox(command, context),
        Some(Command::Eval(command)) => cli::eval::run(command),
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
            Some(Command::Stores(command)) => command.wants_json(),
            Some(Command::Remember(args)) | Some(Command::Note(args)) => args.json,
            Some(Command::Search(args)) => args.wants_json(),
            Some(Command::Context(args)) => args.wants_json(),
            Some(Command::SyncStatus(args)) => args.wants_json(),
            Some(Command::Retag(args)) => args.json,
            Some(Command::Classify(args)) => args.json,
            Some(Command::Refresh(args)) => args.wants_json(),
            Some(Command::Flush(args)) => args.wants_json(),
            Some(Command::Outbox(command)) => command.wants_json(),
            Some(Command::Projects(command)) => command.wants_json(),
            Some(Command::Hook(command)) => command.wants_json(),
            Some(Command::Doctor(args)) => args.json,
            Some(Command::Promote(args)) => args.wants_json(),
            Some(Command::Inbox(command)) => command.wants_json(),
            Some(Command::Eval(command)) => command.wants_json(),
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

struct ResolveWriteScopeInput<'a> {
    entry_kind: note::EntryKind,
    explicit: Option<&'a str>,
    no_infer: bool,
    default_scope: &'a str,
    project_id: Option<&'a str>,
    explicit_project: bool,
    explicit_kind: Option<note::MemoryKind>,
    body: &'a str,
}

fn resolve_write_scope(input: ResolveWriteScopeInput<'_>) -> WriteScopeDecision {
    if let Some(scope) = input.explicit {
        return WriteScopeDecision {
            scope: scope.to_owned(),
            inferred: false,
            reason: None,
        };
    }
    if input.no_infer || input.entry_kind != note::EntryKind::Remember {
        return WriteScopeDecision {
            scope: input.default_scope.to_owned(),
            inferred: false,
            reason: None,
        };
    }
    if input.explicit_project && input.project_id.is_some() {
        return WriteScopeDecision {
            scope: "project".to_owned(),
            inferred: true,
            reason: Some("explicit-project"),
        };
    }

    match write_classify::infer_scope(write_classify::InferScopeInput {
        project_id: input.project_id,
        explicit_kind: input.explicit_kind,
        body: input.body,
    }) {
        Some(inference) => WriteScopeDecision {
            scope: inference.scope.to_owned(),
            inferred: true,
            reason: Some(inference.reason),
        },
        None => WriteScopeDecision {
            scope: input.default_scope.to_owned(),
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
    // Command-line project selection is intentional write metadata. Ambient
    // launcher context is only a hint: long-lived sessions commonly move among
    // repositories and should not trap global preferences in one project.
    let explicit_project = args.project.is_some()
        || args
            .project_id
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
    // Store affinity can come from a local project binding, so resolve project
    // identity before choosing the write store. This keeps work/personal routing
    // centralized in `hm` instead of requiring hook scripts or agents to infer it.
    let project_id = resolve_project_id(args.project_id.clone(), project_hint.as_deref())?;
    let scope_decision = resolve_write_scope(ResolveWriteScopeInput {
        entry_kind,
        explicit: args.scope.as_deref(),
        no_infer: args.no_infer_scope,
        default_scope: &config.defaults.write_scope,
        project_id: project_id.as_deref(),
        explicit_project,
        explicit_kind: args.kind,
        body: &args.text,
    });
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
    /// Scope carried before the rewrite.
    previous_scope: String,
    /// Scope persisted by the rewrite.
    scope: String,
    /// Project identity carried before the rewrite.
    previous_project_id: Option<String>,
    /// Project identity persisted by the rewrite.
    project_id: Option<String>,
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

/// Correct persisted classification or project metadata on one record.
///
/// This is the recovery path for wrong write-time inference: kind is a
/// persisted search-only/always-on verdict, and without a retag command the
/// only fix is hand-editing cloud-synced Markdown. The rewrite goes through
/// the same note+event pair as ordinary writes so the index (which prefers
/// event metadata) converges on the corrected value at the next rebuild.
fn run_retag(args: RetagArgs, context: CliContext) -> Result<()> {
    if args.kind.is_none()
        && args.scope.is_none()
        && args.project_id.is_none()
        && args.project.is_none()
    {
        anyhow::bail!("retag requires at least one of --kind, --scope, --project-id, or --project");
    }
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let resolved_store = resolve_store(
        &config,
        context.store.as_deref(),
        None,
        agent_id.as_deref(),
        StoreAccess::Read,
    )?;
    // Mutating an existing record requires both capabilities: the command
    // reads current metadata and must not turn a write-only store grant into a
    // way to inspect or alter records the caller cannot otherwise read.
    resolve_store(
        &config,
        Some(&resolved_store.name),
        None,
        agent_id.as_deref(),
        StoreAccess::Write,
    )?;
    let store_config = &config.stores[resolved_store.name.as_str()];
    let update_kind = args.kind.is_some();
    let kind = match args.kind.as_deref() {
        None | Some("none") => None,
        Some(other) => Some(
            parse_memory_kind(other)
                .map_err(|message| anyhow::anyhow!("invalid --kind: {message}, or none"))?,
        ),
    };
    let project_hint = args
        .project
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    let project_update = if args.project_id.is_some() || args.project.is_some() {
        resolve_project_id(args.project_id, project_hint.as_deref())?
    } else {
        None
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
    if !visibility::audience_allows(entry, agent_id.as_deref()) {
        anyhow::bail!(
            "memory record {} is not visible to the active agent",
            args.id
        );
    }
    if args.scope.as_deref().is_some_and(|scope| {
        scope != entry.scope && (scope == "agent-private" || entry.scope == "agent-private")
    }) {
        anyhow::bail!(
            "retag cannot change agent-private visibility; rewrite the record with an explicit audience"
        );
    }
    if args.scope.as_deref() == Some("project")
        && project_update
            .as_ref()
            .or(entry.project_id.as_ref())
            .is_none()
    {
        anyhow::bail!(
            "--scope project requires --project, --project-id, or existing project metadata"
        );
    }

    let options = write::AtomicWriteOptions {
        fsync: config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    };
    let classified = match (update_kind, kind) {
        (false, _) => memory::ClassifiedUpdate::Keep,
        (true, Some(_)) => memory::ClassifiedUpdate::Set(note::ClassifiedBy {
            source: note::ClassifierSource::Manual,
            backend: None,
            at: now_rfc3339(),
            verdict_version: 0,
            confidence: None,
        }),
        (true, None) => memory::ClassifiedUpdate::Clear,
    };
    let result = memory::retag_record(memory::RetagRecordInput {
        root: &store_config.root,
        note_path: &entry.note_path,
        update_kind,
        kind,
        scope: args.scope,
        project_id: project_update,
        classified,
        options,
    })?;

    if args.json {
        let output = RetagJsonOutput {
            id: result.id,
            store: resolved_store.name.clone(),
            previous_kind: result.previous_kind.map(memory_kind_label),
            kind: result.kind.map(memory_kind_label),
            previous_scope: result.previous_scope,
            scope: result.scope,
            previous_project_id: result.previous_project_id,
            project_id: result.project_id,
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
        println!("scope: {} -> {}", result.previous_scope, result.scope);
        println!(
            "project_id: {} -> {}",
            result.previous_project_id.as_deref().unwrap_or("none"),
            result.project_id.as_deref().unwrap_or("none")
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
    // Reconciliation can supersede records, so it is not a read-only recall
    // surface. Capture/reconcile currently write without project ownership;
    // keep project records out of mutation candidates until those commands gain
    // an explicit project-selection contract.
    let entries = ctx
        .entries
        .iter()
        .filter(|entry| entry.scope != "project")
        .cloned()
        .collect::<Vec<_>>();
    let hits = search::search(search::SearchInput {
        store_root: ctx.store_root,
        entries: &entries,
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

fn context_session_id() -> Option<String> {
    std::env::var("HIVE_MEMORY_SESSION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::{normalize_supersedes, validate_validity_window};
    use crate::cli::context::{ContextKeyPolicy, context_selection_key};

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
            ContextKeyPolicy {
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
            ContextKeyPolicy {
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
        &hook_options(config),
    );
    if let Err(err) = result {
        // Receipts are ephemeral hook coordination state. The canonical memory
        // write has already succeeded, so receipt loss should warn but never
        // make a successful `hm remember`/`hm note` look failed.
        eprintln!("warning: failed to write session receipt: {err}");
    }
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
