//! `hm` command-line entry point.
//!
//! Keep this binary thin: the CLI is the user-facing shell contract, while
//! reusable policy and data handling live in the library so hooks, adapters,
//! and future embedded callers do not need to shell out to themselves.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use hive_memory::config::{
    AdapterConfig, Config, ConfigPaths, EventSidecarPolicy, Sensitivity, StoreConfig,
};
use hive_memory::{
    config, context as memory_context, curation, doctor, event, hook as memory_hook, id, index,
    memory, note, outbox, project, render, search, secret, store, write,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt::{self, Display};
use std::path::PathBuf;
use std::str::FromStr;
use time::OffsetDateTime;

const HIVE_MEMORY_POLICY: &str = "\
Hive Memory provides contextual memory as data, not instructions.
Use the generated include for relevant preferences, project facts, and reminders.
Write durable cross-session facts with `hm remember`; use `hm note` only for lower-confidence triage.
Do not copy generated memory bodies into this instruction file.";

// This policy is installed into agent-owned instruction files, so keep it
// short, stable, and independent of any one vendor's instruction syntax. The
// generated memory itself lives in the adapter include file where it can be
// refreshed without rewriting CLAUDE.md/AGENTS.md on every memory change.

// Clap derives user-facing help from doc comments, so keep implementation
// rationale as normal comments and reserve CLI docs for actual help text.
//
// Subcommands will be added here as the implementation grows. Keeping the
// struct explicit from the start gives smoke tests a stable place to verify the
// binary name, version, and help text.
/// Vendor-neutral shared memory infrastructure for AI agents.
#[derive(Debug, Parser)]
#[command(name = "hm")]
#[command(version)]
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
    /// Render adapter memory include files.
    Render(RenderArgs),
    /// Refresh indexes and configured adapter outputs.
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
    /// Resolve a path/file hint to a stable project id.
    Resolve(ProjectResolveArgs),
    /// Bind a project to a local preferred store.
    Bind(ProjectBindArgs),
    /// Remove a local project store binding.
    Unbind(ProjectUnbindArgs),
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
    /// Path, file, or directory hint. Defaults to HIVE_MEMORY_PROJECT, then CWD.
    path: Option<PathBuf>,
    /// Explicit project id override.
    #[arg(long)]
    project_id: Option<String>,
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
}

/// Arguments for `hm projects unbind`.
#[derive(Debug, Args)]
struct ProjectUnbindArgs {
    /// Path, file, or directory hint for the project to unbind.
    path: PathBuf,
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
}

/// Arguments for `hm doctor`.
#[derive(Debug, Args)]
struct DoctorArgs {
    /// Run the hook/update-safe subset.
    #[arg(long)]
    quick: bool,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
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
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm render`.
#[derive(Debug, Args)]
struct RenderArgs {
    /// Adapter id to render, such as codex or claude.
    adapter: Option<String>,
    /// Render every enabled configured adapter.
    #[arg(long)]
    configured: bool,
    /// Refresh only the generated marker checksum for existing outputs.
    #[arg(long)]
    upgrade_marker: bool,
    /// Install adapter include markers into configured instruction files.
    #[arg(long)]
    install: bool,
    /// Remove adapter include markers from configured instruction files.
    #[arg(long, conflicts_with = "install")]
    uninstall: bool,
    /// With --uninstall, remove the shared Hive Memory policy block too.
    #[arg(long, requires = "uninstall")]
    all: bool,
    /// Overwrite a drifted generated output.
    #[arg(long)]
    force: bool,
    /// Write a backup when forcing a drifted generated output.
    #[arg(long)]
    backup: bool,
    /// Suppress per-adapter output.
    #[arg(long)]
    quiet: bool,
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
    let context = CliContext {
        config_path: cli.config,
        store: cli.store,
        as_agent: cli.as_agent,
    };
    match cli.command {
        Some(Command::Stores(command)) => run_stores(command, context),
        Some(Command::Remember(args)) => run_write_memory(note::EntryKind::Remember, args, context),
        Some(Command::Note(args)) => run_write_memory(note::EntryKind::Note, args, context),
        Some(Command::Search(args)) => run_search(args, context),
        Some(Command::Context(args)) => run_context(args, context),
        Some(Command::Render(args)) => run_render(args, context),
        Some(Command::Refresh(args)) => run_refresh(args, context),
        Some(Command::Flush(args)) => run_flush(args, context),
        Some(Command::Outbox(OutboxCommand::Flush(args))) => run_flush(args, context),
        Some(Command::Projects(command)) => run_projects(command, context),
        Some(Command::Hook(command)) => run_hook(command, context),
        Some(Command::Doctor(args)) => run_doctor(args, context),
        Some(Command::Promote(args)) => run_promote(args, context),
        Some(Command::Inbox(command)) => run_inbox(command, context),
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
            Some(Command::Stores(StoresCommand::List(args))) => args.json,
            Some(Command::Stores(StoresCommand::Show(args))) => args.json,
            Some(Command::Remember(args)) | Some(Command::Note(args)) => args.json,
            Some(Command::Search(args)) => args.json,
            Some(Command::Context(args)) => args.json,
            Some(Command::Render(args)) => args.json,
            Some(Command::Refresh(args)) => args.json,
            Some(Command::Flush(args)) | Some(Command::Outbox(OutboxCommand::Flush(args))) => {
                args.json
            }
            Some(Command::Projects(ProjectsCommand::Resolve(args))) => args.json,
            Some(Command::Hook(HookCommand::SessionStart(args))) => args.json,
            Some(Command::Hook(HookCommand::PromptSubmit(args))) => args.json,
            Some(Command::Hook(HookCommand::ToolComplete(args))) => args.json,
            Some(Command::Hook(HookCommand::Stop(args))) => args.json,
            Some(Command::Doctor(args)) => args.json,
            Some(Command::Promote(args)) => args.json,
            Some(Command::Inbox(InboxCommand::List(args))) => args.json,
            Some(Command::Inbox(InboxCommand::Stale(args))) => args.json,
            Some(Command::Inbox(InboxCommand::Show(args))) => args.json,
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
}

fn run_doctor(args: DoctorArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let report = doctor::run(doctor::DoctorInput {
        config: &config,
        quick: args.quick,
    });

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
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
            println!(
                "initialized store {} at {}",
                manifest.store.name,
                root.display()
            );
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
            run_store_doctor(&config, args.name.as_deref())
        }
        StoresCommand::Migrate(args) => {
            let config = load_config(context.config_path.as_deref())?;
            run_store_migrate(&config, args.store.as_deref(), args.dry_run)
        }
    }
}

fn run_projects(command: ProjectsCommand, context: CliContext) -> Result<()> {
    match command {
        ProjectsCommand::Resolve(args) => run_project_resolve(args, context),
        ProjectsCommand::Bind(args) => run_project_bind(args, context),
        ProjectsCommand::Unbind(args) => run_project_unbind(args, context),
    }
}

fn run_project_resolve(args: ProjectResolveArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let hint = args
        .path
        .or_else(|| std::env::var("HIVE_MEMORY_PROJECT").ok().map(PathBuf::from))
        .unwrap_or_default();
    let project = project::resolve_project(project::ResolveProjectInput {
        hint,
        explicit_project_id: args.project_id,
        env_project_id: std::env::var("HIVE_MEMORY_PROJECT_ID").ok(),
    })?;
    let agent_id = resolve_agent_id(context.as_agent);
    let binding = project::load_binding(&config.data_dir, &project.project_id)?;
    let store = resolve_store(
        &config,
        context.store.as_deref(),
        binding.as_ref().map(|binding| binding.store.as_str()),
        agent_id.as_deref(),
        StoreAccess::Read,
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
    resolve_store(
        &config,
        Some(args.store.as_str()),
        None,
        agent_id.as_deref(),
        StoreAccess::Read,
    )?;
    resolve_store(
        &config,
        Some(args.store.as_str()),
        None,
        agent_id.as_deref(),
        StoreAccess::Write,
    )?;
    let binding = project::ProjectBinding {
        project_id: project.project_id.clone(),
        store: args.store,
    };
    let path = project::save_binding(&config.data_dir, &binding, &hook_options(&config))?;

    println!("project_id: {}", project.project_id);
    println!("store: {}", binding.store);
    println!("binding: {}", path.display());
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

    println!("project_id: {}", project.project_id);
    println!("removed: {}", removed.is_some());
    if let Some(path) = removed {
        println!("binding: {}", path.display());
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
) -> Result<(String, PathBuf, index::RebuildIndexReport)> {
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
    let agent_id = resolve_agent_id(context.as_agent);
    let writer_agent_id = agent_id.clone().unwrap_or_else(|| "human".to_owned());
    let scope = args
        .scope
        .clone()
        .unwrap_or_else(|| config.defaults.write_scope.clone());
    let project_hint = args
        .project
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    // Store affinity can come from a local project binding, so resolve project
    // identity before choosing the write store. This keeps work/personal routing
    // centralized in `hm` instead of requiring hook scripts or agents to infer it.
    let project_id = resolve_project_id(args.project_id.clone(), project_hint.as_deref())?;
    let project_binding = project_binding_store(&config, project_id.as_deref())?;
    let resolved_store = resolve_store(
        &config,
        context.store.as_deref(),
        project_binding.as_deref(),
        agent_id.as_deref(),
        StoreAccess::Write,
    )?;
    let store_config = &config.stores[resolved_store.name.as_str()];
    validate_secret_write(&config, store_config, args.allow_secret_write, &args.text)?;
    let created_at = OffsetDateTime::now_utc();
    let host_id = resolve_host_id(&config);
    let audience = resolve_audience(&args, &scope, &writer_agent_id)?;
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
        Err(store::StoreError::Io { .. }) => enqueue_outbox_memory(
            &config,
            store_config,
            &resolved_store.name,
            known_store_identity(&config, &resolved_store.name, store_config)?,
            write_input,
        )?,
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
            audience: audience.clone(),
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
        final_note_path: store_relative_path_string(&note_relative_path),
        note: note.into_bytes(),
        final_event_path: write_event.then(|| store_relative_path_string(&event_relative_path)),
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

fn store_relative_path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[derive(Debug, Serialize)]
struct WriteMemoryJson {
    id: String,
    store: String,
    store_id: String,
    store_source: String,
    scope: String,
    project_id: Option<String>,
    audience: Vec<String>,
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

    let hits = search::search(search::SearchInput {
        store_root: &store_config.root,
        entries: &report.entries,
        query: &args.query,
        scopes: &scopes,
        sources: &sources,
        include_inbox,
        agent_id: agent_id.as_deref(),
        project_id: project_id.as_deref(),
        limit: args.limit,
    })?;

    if args.json {
        let output = hits
            .iter()
            .map(|hit| search_json_hit(&resolved_store.name, &manifest.store.id, hit))
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!("store: {}", resolved_store.name);
    println!("hits: {}", hits.len());
    for hit in hits {
        println!("id: {}", hit.entry.id);
        println!("score: {}", hit.score);
        println!("note: {}", hit.entry.note_path);
        println!("snippet: {}", hit.snippet);
    }
    Ok(())
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
    created_at: String,
}

fn search_json_hit(
    store_name: &str,
    manifest_store_id: &str,
    hit: &search::SearchHit,
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
        created_at: entry.created_at.clone(),
    }
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
    let assembly = assemble_cli_context(
        &config,
        &context,
        ContextSelection {
            max_tokens: args.max_tokens,
            include_inbox: args.include_inbox,
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
    let path_hint = selection
        .path_hint
        .or_else(|| std::env::var("HIVE_MEMORY_PROJECT").ok());
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
    );
    if std::env::var("HIVE_MEMORY_HOOK_ACTIVE").ok().as_deref() == Some("1")
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
    let report = rebuild_store_index(config, &resolved_store.name)?;
    let max_tokens = selection.max_tokens.unwrap_or_else(|| {
        // Hooks run on latency-sensitive agent boundaries, so they use the
        // configured hook budget unless the caller has explicitly provided a
        // tighter or broader limit. Interactive `hm context` keeps the larger
        // v1 default for inspection and manual debugging.
        if std::env::var("HIVE_MEMORY_HOOK_ACTIVE").ok().as_deref() == Some("1") {
            usize::try_from(config.defaults.hook_context_max_tokens)
                .expect("hook context token budget fits usize")
        } else {
            4000
        }
    });

    let output = memory_context::assemble_context(memory_context::ContextInput {
        store_name: store_name.as_str(),
        store_root: &store_config.root,
        entries: &report.entries,
        scopes: &scopes,
        sources: &sources,
        include_inbox,
        agent_id: agent_id.as_deref(),
        project_id: project_id.as_deref(),
        path_hint: path_hint.as_deref(),
        max_tokens,
    })
    .map_err(anyhow::Error::from)?;

    let assembly = CliContextAssembly {
        output,
        agent_id,
        project_id,
        project_hint: path_hint,
        stores,
        store_source,
        scopes,
        sources,
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
    /// Token estimate from the fresh assembly.
    estimated_tokens: usize,
    /// Section metadata kept so stale JSON output preserves data boundaries.
    sections: Vec<ContextCacheSection>,
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

    Ok(Some(CliContextAssembly {
        output: memory_context::ContextOutput {
            markdown,
            sections,
            estimated_tokens: entry.estimated_tokens,
        },
        agent_id: entry.agent_id,
        project_id: entry.project_id,
        project_hint: entry.project_hint,
        stores: entry.stores,
        store_source,
        scopes: entry.scopes,
        sources: entry.sources,
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
    // Keep this grammar intentionally tiny and explicit. Hook fallback policy
    // should be auditable from config, not dependent on a permissive duration
    // parser whose accepted syntax changes under us.
    let trimmed = input.trim();
    let unit = trimmed.chars().last()?;
    let number = trimmed[..trimmed.len().saturating_sub(unit.len_utf8())]
        .parse::<i64>()
        .ok()?;
    if number < 0 {
        return None;
    }
    match unit {
        'd' => Some(time::Duration::days(number)),
        'h' => Some(time::Duration::hours(number)),
        'm' => Some(time::Duration::minutes(number)),
        's' => Some(time::Duration::seconds(number)),
        _ => None,
    }
}

fn cached_trust(value: &str) -> memory_context::TrustLevel {
    match value {
        "curated" => memory_context::TrustLevel::Curated,
        "raw" => memory_context::TrustLevel::Raw,
        _ => memory_context::TrustLevel::Remembered,
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
        estimated_tokens: 0,
        emitted: false,
        stale,
        cache_created_at,
        sections: Vec::new(),
    }
}

fn rebuild_store_index(config: &Config, store_name: &str) -> Result<index::RebuildIndexReport> {
    let store_config = &config.stores[store_name];
    let options = write::AtomicWriteOptions {
        fsync: config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    };
    // Read commands rebuild for correctness in this first implementation, so
    // direct file edits and writes from other processes are visible immediately.
    // A later lazy mtime/inode check can live behind this helper.
    let report = index::rebuild_index(index::RebuildIndexInput {
        store_name,
        store_root: &store_config.root,
        cache_dir: &config.cache_dir,
        options,
    })?;
    for warning in &report.warnings {
        eprintln!("warning: {}: {}", warning.path.display(), warning.message);
    }
    Ok(report)
}

fn run_render(args: RenderArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let adapters = selected_adapters(&config, &args)?;
    let options = write::AtomicWriteOptions {
        fsync: config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    };
    let configured = args.configured;
    let mut json_outputs = Vec::new();

    for adapter_name in adapters {
        let adapter = &config.adapters[adapter_name.as_str()];
        let Some(output) = adapter.output.as_ref() else {
            anyhow::bail!("adapter {adapter_name} has no output configured");
        };

        let report = if args.uninstall {
            None
        } else if args.upgrade_marker {
            Some(render::upgrade_marker(output, options.clone())?)
        } else {
            let body = render_adapter_body(&config, adapter_name.as_str(), adapter, &context)?;
            Some(render::write_rendered_file(render::RenderFileInput {
                output,
                body: &body,
                options: options.clone(),
                force: args.force,
                backup: args.backup,
            })?)
        };

        let uninstall_report = if args.uninstall {
            let Some(install_target) = adapter.install_target.as_ref() else {
                anyhow::bail!("adapter {adapter_name} has no install_target configured");
            };
            Some(render::uninstall_adapter(render::UninstallAdapterInput {
                adapter: adapter_name.as_str(),
                install_target,
                all: args.all,
                options: options.clone(),
            })?)
        } else {
            None
        };

        let install_report = if args.install {
            let Some(install_target) = adapter.install_target.as_ref() else {
                anyhow::bail!("adapter {adapter_name} has no install_target configured");
            };
            Some(render::install_adapter(render::InstallAdapterInput {
                adapter: adapter_name.as_str(),
                output,
                install_target,
                policy_body: HIVE_MEMORY_POLICY,
                options: options.clone(),
            })?)
        } else {
            None
        };

        if args.json {
            json_outputs.push(render_json_output(
                adapter_name.as_str(),
                output,
                adapter.install_target.as_deref(),
                report.as_ref(),
                install_report.as_ref(),
                uninstall_report.as_ref(),
            )?);
        } else if !args.quiet {
            println!("adapter: {adapter_name}");
            if let Some(report) = report.as_ref() {
                println!("output: {}", report.output.display());
                println!("written: {}", report.written);
                println!("sha256: {}", report.sha256);
                if let Some(path) = &report.backup_path {
                    println!("backup: {}", path.display());
                }
            }
            if let Some(uninstall) = uninstall_report.as_ref() {
                println!("install_target: {}", uninstall.target.display());
                println!("uninstalled: {}", uninstall.written);
                if let Some(path) = &uninstall.backup_path {
                    println!("uninstall_backup: {}", path.display());
                }
            }
            if let Some(install) = install_report.as_ref() {
                println!("install_target: {}", install.target.display());
                println!("installed: {}", install.written);
                if let Some(path) = &install.backup_path {
                    println!("install_backup: {}", path.display());
                }
            }
        }
    }

    if args.json {
        if configured {
            println!("{}", serde_json::to_string_pretty(&json_outputs)?);
        } else if let Some(output) = json_outputs.into_iter().next() {
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct RenderJsonOutput {
    /// Adapter id that was rendered or installed.
    adapter: String,
    /// Generated context include file for this adapter.
    output_path: String,
    /// Whether the generated output file changed.
    written: bool,
    /// SHA-256 of the generated body when rendering occurred.
    sha256: String,
    /// Whether the adapter marker is present in the install target.
    installed: bool,
    /// Whether the configured install target points at the configured output.
    visible: bool,
    /// Instruction files touched by install/uninstall operations.
    install_targets: Vec<String>,
    /// Backup files written by render/install/uninstall operations.
    backup_paths: Vec<String>,
}

fn render_json_output(
    adapter_name: &str,
    output: &std::path::Path,
    install_target: Option<&std::path::Path>,
    report: Option<&render::RenderFileReport>,
    install_report: Option<&render::InstallAdapterReport>,
    uninstall_report: Option<&render::UninstallAdapterReport>,
) -> Result<RenderJsonOutput> {
    let inspection = match install_target {
        Some(install_target) => Some(render::inspect_adapter_install(
            render::InspectAdapterInstallInput {
                adapter: adapter_name,
                output,
                install_target,
            },
        )?),
        None => None,
    };
    let mut install_targets = Vec::new();
    if let Some(report) = install_report {
        install_targets.push(report.target.display().to_string());
    }
    if let Some(report) = uninstall_report {
        install_targets.push(report.target.display().to_string());
    }
    let mut backup_paths = Vec::new();
    if let Some(path) = report.and_then(|report| report.backup_path.as_ref()) {
        backup_paths.push(path.display().to_string());
    }
    if let Some(path) = install_report.and_then(|report| report.backup_path.as_ref()) {
        backup_paths.push(path.display().to_string());
    }
    if let Some(path) = uninstall_report.and_then(|report| report.backup_path.as_ref()) {
        backup_paths.push(path.display().to_string());
    }

    Ok(RenderJsonOutput {
        adapter: adapter_name.to_owned(),
        output_path: report
            .map(|report| report.output.display().to_string())
            .unwrap_or_else(|| output.display().to_string()),
        written: report.map(|report| report.written).unwrap_or(false),
        sha256: report
            .map(|report| report.sha256.clone())
            .unwrap_or_default(),
        installed: inspection
            .as_ref()
            .map(|inspection| inspection.installed)
            .unwrap_or(false),
        visible: inspection
            .as_ref()
            .map(|inspection| {
                inspection.target_exists && inspection.installed && inspection.include_matches
            })
            .unwrap_or(false),
        install_targets,
        backup_paths,
    })
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

    let mut report = perform_refresh(&config, &context, args.force)?;
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
            "refresh: indexes={} flushed={} skipped={} failed={} unbound={} pending={} rendered={} written={} render_skipped={} forced={} write_receipts={} refreshed={} coalesced={}",
            report.indexes,
            report.flushed,
            report.skipped,
            report.failed,
            report.unbound,
            report.pending,
            report.rendered,
            report.written,
            report.render_skipped,
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
    if std::env::var("HIVE_MEMORY_HOOK_ACTIVE").ok().as_deref() != Some("1") {
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

fn perform_refresh(
    config: &Config,
    context: &CliContext,
    forced: bool,
) -> Result<HookRefreshReport> {
    // Refresh is the one maintenance command hooks need to call after writes.
    // Flushing first makes any locally queued memory visible to the index/render
    // path in the same cycle without teaching hook scripts outbox policy.
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
    for store_name in config.stores.keys() {
        rebuild_store_index(config, store_name)?;
        indexes += 1;
    }

    // Hooks may need fresh indexes without touching agent instruction files.
    // The env switch gives dotfiles hooks a simple safety valve while keeping
    // refresh policy centralized in `hm` instead of in shell glue.
    let render_skipped = std::env::var("HIVE_MEMORY_NO_RENDER").ok().as_deref() == Some("1");
    let render_summary = if render_skipped {
        RenderRefreshSummary::default()
    } else {
        refresh_render_outputs(config, context)?
    };

    Ok(HookRefreshReport {
        indexes,
        flushed: flush.flushed,
        skipped: flush.skipped,
        failed: flush.failed,
        unbound: flush.unbound,
        pending: flush.pending,
        rendered: render_summary.rendered,
        written: render_summary.written,
        render_skipped,
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
        rendered: 0,
        written: 0,
        render_skipped: std::env::var("HIVE_MEMORY_NO_RENDER").ok().as_deref() == Some("1"),
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
        rendered: 0,
        written: 0,
        render_skipped: std::env::var("HIVE_MEMORY_NO_RENDER").ok().as_deref() == Some("1"),
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
            scopes: Vec::new(),
            sources: Vec::new(),
            project_id: std::env::var("HIVE_MEMORY_PROJECT_ID").ok(),
            path_hint: path_hint.clone(),
        },
    )?;
    if let Some(session_id) = hook_session_id(&mut warnings) {
        memory_hook::mark_context_key(
            &config.state_dir,
            &session_id,
            hook_context_key(&config, &context, path_hint.as_deref())?,
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
    )? {
        context_emitted = true;
        actions.push(action);
    }

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

    let path_hint = args
        .project
        .or_else(|| std::env::var("HIVE_MEMORY_PROJECT").ok());
    // Tool completion may refresh generated context, so use the same
    // project-aware store selection that the refreshable context path uses.
    validate_hook_context_read_policy(&config, &context, path_hint.as_deref())?;

    let session_id = hook_session_id(&mut warnings);
    let mut memory_pending = if let Some(session_id) = session_id.as_deref() {
        memory_hook::load_state(&config.state_dir, session_id)?.memory_pending
    } else {
        false
    };
    let mut context_emitted = false;
    if let Some(action) = hook_context_action_if_changed(
        &config,
        &context,
        path_hint.as_deref(),
        session_id.as_deref(),
    )? {
        context_emitted = true;
        actions.push(action);
    }

    if args.status == 0
        && let Some(session_id) = session_id.as_deref()
    {
        let receipts = memory_hook::load_write_receipts(&config.state_dir, session_id)?;
        let mut state = memory_hook::load_state(&config.state_dir, session_id)?;
        let unrefreshed_receipts = receipts.len().saturating_sub(state.refreshed_receipts);

        if unrefreshed_receipts > 0 {
            let mut report = perform_refresh(&config, &context, false)?;
            report.write_receipts = unrefreshed_receipts;
            refresh = Some(report);

            state = memory_hook::mark_receipts_refreshed(
                &config.state_dir,
                session_id,
                receipts.len(),
                true,
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
    };
    emit_hook_response(&response, args.json)?;

    Ok(())
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
    /// Enabled adapter outputs considered for refresh.
    rendered: usize,
    /// Adapter outputs whose bytes changed.
    written: usize,
    /// Whether hook policy disabled render refresh for this invocation.
    render_skipped: bool,
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
) -> Result<Option<HookAction>> {
    let Some(session_id) = session_id else {
        return Ok(None);
    };

    // Long-lived agents can move between projects while the process stays
    // alive. Cache the resolved selection, not just "context was already sent",
    // so hooks can reinject when path/project/store policy changes.
    let context_key = hook_context_key(config, context, path_hint)?;
    let state = memory_hook::load_state(&config.state_dir, session_id)?;
    if state.context_key.as_deref() == Some(context_key.as_str()) {
        return Ok(None);
    }

    let assembly = assemble_cli_context(
        config,
        context,
        ContextSelection {
            max_tokens: Some(usize::try_from(config.defaults.hook_context_max_tokens)?),
            include_inbox: false,
            scopes: Vec::new(),
            sources: Vec::new(),
            project_id: std::env::var("HIVE_MEMORY_PROJECT_ID").ok(),
            path_hint: path_hint.map(str::to_owned),
        },
    )?;
    memory_hook::mark_context_key(
        &config.state_dir,
        session_id,
        context_key,
        &hook_options(config),
    )?;

    Ok(Some(HookAction::new(
        "inject_context",
        assembly.output.markdown,
    )))
}

fn context_selection_key_from_assembly(assembly: &CliContextAssembly) -> String {
    let agent_id = assembly.agent_id.as_deref().unwrap_or("unknown");
    context_selection_key(
        agent_id,
        &assembly.stores,
        assembly.project_id.as_deref(),
        assembly.project_hint.as_deref(),
        &assembly.scopes,
        &assembly.sources,
    )
}

/// Return the stable cursor used by `hm context --if-changed` and hook refreshes.
///
/// This key intentionally tracks selection identity, not memory file mtimes.
/// New memory writes are handled by write receipts and refresh; this cursor is
/// only for long-lived agents moving between projects, stores, or render policy.
fn context_selection_key(
    agent_id: &str,
    stores: &[String],
    project_id: Option<&str>,
    path_hint: Option<&str>,
    scopes: &[String],
    sources: &[String],
) -> String {
    format!(
        "agent={agent_id}\nstores={}\nproject_id={}\npath={}\nscopes={}\nsources={}",
        stores.join(","),
        project_id.unwrap_or_default(),
        path_hint.unwrap_or_default(),
        scopes.join(","),
        sources.join(",")
    )
}

fn hook_context_key(
    config: &Config,
    context: &CliContext,
    path_hint: Option<&str>,
) -> Result<String> {
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let agent_label = agent_id.clone().unwrap_or_else(|| "unknown".to_owned());
    let project_id = resolve_project_id(None, path_hint)?;
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

#[derive(Debug, Default)]
struct RenderRefreshSummary {
    rendered: usize,
    written: usize,
}

/// Refresh generated adapter outputs for all enabled adapters.
///
/// This uses the same render path as `hm render --configured`, but never forces
/// drifted files. A refresh should keep agents current; it should not silently
/// clobber user edits or turn hook-time work into install-time policy changes.
fn refresh_render_outputs(config: &Config, context: &CliContext) -> Result<RenderRefreshSummary> {
    let options = write::AtomicWriteOptions {
        fsync: config.storage.fsync.into(),
        ..write::AtomicWriteOptions::default()
    };
    let mut summary = RenderRefreshSummary::default();

    for (adapter_name, adapter) in config
        .adapters
        .iter()
        .filter(|(_name, adapter)| adapter.enabled)
    {
        let Some(output) = adapter.output.as_ref() else {
            anyhow::bail!("adapter {adapter_name} has no output configured");
        };
        let body = render_adapter_body(config, adapter_name, adapter, context)?;
        let report = render::write_rendered_file(render::RenderFileInput {
            output,
            body: &body,
            options: options.clone(),
            force: false,
            backup: false,
        })?;
        summary.rendered += 1;
        if report.written {
            summary.written += 1;
        }
    }

    Ok(summary)
}

/// Resolve the adapters targeted by one render command.
///
/// Explicit adapter renders may target disabled adapters for manual debugging.
/// `--configured` intentionally narrows to enabled adapters because that path is
/// used by refresh/update automation.
fn selected_adapters(config: &Config, args: &RenderArgs) -> Result<Vec<String>> {
    if args.configured {
        let adapters = config
            .adapters
            .iter()
            .filter(|(_name, adapter)| adapter.enabled)
            .map(|(name, _adapter)| name.clone())
            .collect::<Vec<_>>();
        if adapters.is_empty() {
            anyhow::bail!("no enabled adapters configured");
        }
        return Ok(adapters);
    }

    let Some(adapter) = args.adapter.as_ref() else {
        anyhow::bail!("hm render requires an adapter or --configured");
    };
    if !config.adapters.contains_key(adapter) {
        anyhow::bail!("unknown adapter: {adapter}");
    }
    Ok(vec![adapter.clone()])
}

/// Render one adapter's generated include body.
///
/// Adapter config owns the store allowlist and render scopes. The active CLI
/// store may further narrow that list for debugging, but cannot expand past the
/// adapter allowlist. This keeps render-time policy with `hm` rather than with
/// agent-specific shell hooks.
fn render_adapter_body(
    config: &Config,
    adapter_name: &str,
    adapter: &AdapterConfig,
    context: &CliContext,
) -> Result<String> {
    let stores = render_stores(config, adapter_name, adapter, context.store.as_deref())?;
    let scopes = if adapter.scopes.is_empty() {
        config.defaults.render_scopes.clone()
    } else {
        adapter.scopes.clone()
    };
    let sources = config.defaults.context_sources.clone();
    let include_inbox = sources
        .iter()
        .any(|source| source == "inbox" || source == "all");
    let project_id = std::env::var("HIVE_MEMORY_PROJECT_ID").ok();
    let path_hint = std::env::var("HIVE_MEMORY_PROJECT").ok();
    let mut body = String::new();

    for store_name in stores {
        resolve_store(
            config,
            Some(store_name.as_str()),
            None,
            Some(adapter_name),
            StoreAccess::Read,
        )?;
        let store_config = &config.stores[store_name.as_str()];
        let report = rebuild_store_index(config, store_name.as_str())?;
        let output = memory_context::assemble_context(memory_context::ContextInput {
            store_name: store_name.as_str(),
            store_root: &store_config.root,
            entries: &report.entries,
            scopes: &scopes,
            sources: &sources,
            include_inbox,
            agent_id: Some(adapter_name),
            project_id: project_id.as_deref(),
            path_hint: path_hint.as_deref(),
            max_tokens: 4000,
        })?;
        body.push_str(&output.markdown);
        if !body.ends_with('\n') {
            body.push('\n');
        }
    }

    Ok(body)
}

/// Resolve which stores an adapter may render for this invocation.
///
/// Empty adapter store lists are rejected here instead of being treated as
/// "all stores"; broad renders should always be explicit in config.
fn render_stores(
    config: &Config,
    adapter_name: &str,
    adapter: &AdapterConfig,
    explicit_store: Option<&str>,
) -> Result<Vec<String>> {
    if let Some(store) = explicit_store {
        if !adapter.stores.is_empty() && !adapter.stores.iter().any(|allowed| allowed == store) {
            anyhow::bail!("adapter {adapter_name} may not render store {store}");
        }
        if !config.stores.contains_key(store) {
            anyhow::bail!("unknown store: {store}");
        }
        return Ok(vec![store.to_owned()]);
    }

    if adapter.stores.is_empty() {
        anyhow::bail!("adapter {adapter_name} has no render stores configured");
    }
    Ok(adapter.stores.clone())
}

/// Print configured store inventory.
///
/// The JSON form is intentionally policy-aware when an agent id is active:
/// hooks and adapter installers can ask `hm` whether a store is readable or
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

fn run_store_doctor(config: &Config, name: Option<&str>) -> Result<()> {
    let reports = doctor_reports(config, name)?;
    let mut has_error = false;

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

/// Resolve the single store a CLI command should use and enforce agent policy.
///
/// All one-store commands share the same precedence: explicit `--store`, then
/// `HIVE_MEMORY_STORE`, then local project binding, then the active agent's
/// configured default store, then the global default. Centralizing that order
/// keeps read, write, context, and hook commands from drifting as the command
/// surface grows. Callers that do not have project context pass `None` for the
/// binding slot rather than trying to derive path policy locally.
fn resolve_store(
    config: &Config,
    explicit_store: Option<&str>,
    project_binding: Option<&str>,
    agent_id: Option<&str>,
    access: StoreAccess,
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

    if let Some(agent_id) = agent_id {
        let policy = config.effective_agent_policy(agent_id);
        let (allowed_stores, access_name) = match access {
            StoreAccess::Read => (&policy.read_stores, "read"),
            StoreAccess::Write => (&policy.write_stores, "write"),
        };
        if !policy.allow_all_stores && !allowed_stores.iter().any(|store| store == &name) {
            anyhow::bail!(
                "agent {agent_id} may not {access_name} store {name}; configured {access_name} stores: {}",
                allowed_stores.join(",")
            );
        }
    }

    Ok(ResolvedStore { name, source })
}

fn resolve_agent_id(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| std::env::var("HIVE_MEMORY_AGENT_ID").ok())
}

/// Resolve the host label written into memory metadata.
///
/// `auto` intentionally stays lightweight here. A richer machine identity can
/// be configured explicitly without making every hook pay for hostname probes.
fn resolve_host_id(config: &Config) -> String {
    if config.host_id != "auto" {
        return config.host_id.clone();
    }

    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown-host".to_owned())
}

fn validate_secret_write(
    config: &Config,
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
        anyhow::bail!(
            "Hive Memory does not store likely secrets by default; detectors: {detector_ids}; rerun with --allow-secret-write only for intentional secret-store writes"
        );
    }
    if store.sensitivity != Sensitivity::Secret {
        anyhow::bail!(
            "--allow-secret-write requires a resolved secret store; detectors: {detector_ids}"
        );
    }
    if !config.privacy.allow_secret_writes {
        anyhow::bail!(
            "--allow-secret-write requires privacy.allow_secret_writes = true; detectors: {detector_ids}"
        );
    }
    if std::env::var("HIVE_MEMORY_HOOK_ACTIVE").ok().as_deref() == Some("1")
        && !config.privacy.allow_hook_secret_writes
    {
        anyhow::bail!(
            "hook secret writes require privacy.allow_hook_secret_writes = true; detectors: {detector_ids}"
        );
    }

    Ok(())
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
        anyhow::bail!("agent-private writes require --audience or --audience-writer-only");
    }

    Ok(args.audience.clone())
}
