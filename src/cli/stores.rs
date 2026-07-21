//! Store lifecycle CLI arguments, output models, and command handlers.

use crate::{CliContext, load_config, resolve_agent_id};
use anyhow::Result;
use clap::{Args, Subcommand};
use hive_memory::config::{self, Config, Sensitivity, StoreConfig};
use hive_memory::store;
use serde::Serialize;
use std::path::PathBuf;
use std::str::FromStr;

/// Store lifecycle commands.
#[derive(Debug, Subcommand)]
pub(crate) enum StoresCommand {
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

impl StoresCommand {
    /// Return whether this invocation requires structured error output.
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::Init(args) => args.json,
            Self::List(args) => args.json,
            Self::Show(args) => args.json,
            Self::Doctor(args) => args.json,
            Self::Migrate(_) => false,
        }
    }
}

/// Arguments for `hm stores init`.
///
/// The CLI captures explicit user intent only. Identity generation, directory
/// layout, and atomic manifest writes are delegated to the store library.
#[derive(Debug, Args)]
pub(crate) struct StoreInitArgs {
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
pub(crate) struct StoreListArgs {
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm stores show`.
#[derive(Debug, Args)]
pub(crate) struct StoreShowArgs {
    /// Store alias to show. Defaults to config.default_store.
    name: Option<String>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm stores doctor`.
#[derive(Debug, Args)]
pub(crate) struct StoreDoctorArgs {
    /// Store alias to diagnose. Defaults to all configured stores.
    name: Option<String>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm stores migrate`.
#[derive(Debug, Args)]
pub(crate) struct StoreMigrateArgs {
    /// Check what would migrate without changing stores.
    #[arg(long)]
    dry_run: bool,
    /// Store alias to migrate. Defaults to all configured stores.
    #[arg(long)]
    store: Option<String>,
}

#[derive(Debug, Serialize)]
struct StoreInitOutput {
    name: String,
    root: String,
    store_id: String,
    sensitivity: String,
}

/// Run one store lifecycle command.
pub(crate) fn run(command: StoresCommand, context: CliContext) -> Result<()> {
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

fn parse_sensitivity(input: &str) -> std::result::Result<Sensitivity, String> {
    Sensitivity::from_str(input)
        .map_err(|_| "expected one of: public, internal, private, secret".to_owned())
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
