//! `hm` command-line entry point.
//!
//! Keep this binary thin: the CLI is the user-facing shell contract, while
//! reusable policy and data handling live in the library so hooks, adapters,
//! and future embedded callers do not need to shell out to themselves.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use hive_memory::config::Sensitivity;
use hive_memory::store;
use std::path::PathBuf;
use std::str::FromStr;

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
}

/// Store lifecycle commands.
#[derive(Debug, Subcommand)]
enum StoresCommand {
    /// Initialize a store root with a manifest and canonical directories.
    Init(StoreInitArgs),
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Stores(command)) => run_stores(command),
        None => Ok(()),
    }
}

fn run_stores(command: StoresCommand) -> Result<()> {
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
    }
}

fn parse_sensitivity(input: &str) -> std::result::Result<Sensitivity, String> {
    Sensitivity::from_str(input)
        .map_err(|_| "expected one of: public, internal, private, secret".to_owned())
}
