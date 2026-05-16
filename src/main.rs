//! `hm` command-line entry point.
//!
//! Keep this binary thin: the CLI is the user-facing shell contract, while
//! reusable policy and data handling live in the library so hooks, adapters,
//! and future embedded callers do not need to shell out to themselves.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use hive_memory::config::{Config, ConfigPaths, EventSidecarPolicy, Sensitivity};
use hive_memory::{memory, note, store, write};
use std::path::PathBuf;
use std::str::FromStr;
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
}

/// Store lifecycle commands.
#[derive(Debug, Subcommand)]
enum StoresCommand {
    /// Initialize a store root with a manifest and canonical directories.
    Init(StoreInitArgs),
    /// List configured stores and root availability.
    List,
    /// Show one configured store, defaulting to the global default store.
    Show(StoreShowArgs),
    /// Run store diagnostics.
    Doctor(StoreDoctorArgs),
    /// Run schema migrators when a future schema is available.
    Migrate(StoreMigrateArgs),
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

/// Arguments for `hm stores show`.
#[derive(Debug, Args)]
struct StoreShowArgs {
    /// Store alias to show. Defaults to config.default_store.
    name: Option<String>,
}

/// Arguments for `hm stores doctor`.
#[derive(Debug, Args)]
struct StoreDoctorArgs {
    /// Store alias to diagnose. Defaults to all configured stores.
    name: Option<String>,
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let context = CliContext {
        config_path: cli.config,
        store: cli.store,
        as_agent: cli.as_agent,
    };
    match cli.command {
        Some(Command::Stores(command)) => run_stores(command, context.config_path),
        Some(Command::Remember(args)) => run_write_memory(note::EntryKind::Remember, args, context),
        Some(Command::Note(args)) => run_write_memory(note::EntryKind::Note, args, context),
        None => Ok(()),
    }
}

struct CliContext {
    config_path: Option<PathBuf>,
    store: Option<String>,
    as_agent: Option<String>,
}

fn run_stores(command: StoresCommand, config_path: Option<PathBuf>) -> Result<()> {
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
        StoresCommand::List => {
            let config = load_config(config_path.as_deref())?;
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
        StoresCommand::Show(args) => {
            let config = load_config(config_path.as_deref())?;
            show_store(&config, args.name.as_deref())?;
            Ok(())
        }
        StoresCommand::Doctor(args) => {
            let config = load_config(config_path.as_deref())?;
            run_store_doctor(&config, args.name.as_deref())
        }
        StoresCommand::Migrate(args) => {
            let config = load_config(config_path.as_deref())?;
            run_store_migrate(&config, args.store.as_deref(), args.dry_run)
        }
    }
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

fn run_write_memory(
    entry_kind: note::EntryKind,
    args: WriteMemoryArgs,
    context: CliContext,
) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent);
    let resolved_store =
        resolve_write_store(&config, context.store.as_deref(), agent_id.as_deref())?;
    let store_config = &config.stores[resolved_store.name.as_str()];
    let manifest = store::read_manifest(&store_config.root)?;
    let created_at = OffsetDateTime::now_utc();
    let host_id = resolve_host_id(&config);
    let writer_agent_id = agent_id.unwrap_or_else(|| "human".to_owned());
    let scope = args
        .scope
        .clone()
        .unwrap_or_else(|| config.defaults.write_scope.clone());
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
    let result = memory::write_record(memory::WriteRecordInput {
        root: &store_config.root,
        manifest: &manifest,
        entry_kind,
        created_at,
        agent_id: writer_agent_id,
        host_id,
        user_id: config.user_id.clone(),
        session_id: std::env::var("HIVE_MEMORY_SESSION_ID").ok(),
        scope,
        confidence: args.confidence,
        body: args.text,
        project_id: args
            .project_id
            .or_else(|| std::env::var("HIVE_MEMORY_PROJECT_ID").ok()),
        subject: args.subject,
        tags: args.tags,
        audience,
        source_kind: args.source_kind,
        source_ref: args.source_ref,
        write_event: should_write_event,
        options,
    })?;

    println!("id: {}", result.id);
    println!("store: {}", resolved_store.name);
    println!("note: {}", result.note_path.display());
    if let Some(path) = result.event_path {
        println!("event: {}", path.display());
    }
    Ok(())
}

fn show_store(config: &Config, name: Option<&str>) -> Result<()> {
    let name = name.unwrap_or(&config.default_store);
    let Some(store) = config.stores.get(name) else {
        anyhow::bail!("unknown store: {name}");
    };

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
}

fn resolve_write_store(
    config: &Config,
    explicit_store: Option<&str>,
    agent_id: Option<&str>,
) -> Result<ResolvedStore> {
    let name = if let Some(store) = explicit_store {
        store.to_owned()
    } else if let Ok(store) = std::env::var("HIVE_MEMORY_STORE") {
        store
    } else if let Some(agent_id) = agent_id {
        config.effective_agent_policy(agent_id).default_store
    } else {
        config.default_store.clone()
    };

    let Some(_store) = config.stores.get(&name) else {
        anyhow::bail!("unknown store: {name}");
    };

    if let Some(agent_id) = agent_id {
        let policy = config.effective_agent_policy(agent_id);
        if !policy.write_stores.iter().any(|store| store == &name) {
            anyhow::bail!(
                "agent {agent_id} may not write store {name}; configured write stores: {}",
                policy.write_stores.join(",")
            );
        }
    }

    Ok(ResolvedStore { name })
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
