//! `hm` command-line entry point.
//!
//! Keep this binary thin: the CLI is the user-facing shell contract, while
//! reusable policy and data handling live in the library so hooks, adapters,
//! and future embedded callers do not need to shell out to themselves.

use anyhow::Result;
use clap::Parser;

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
struct Cli {}

fn main() -> Result<()> {
    // Parsing is currently the behavior under test. Future command dispatch
    // should stay behind library APIs so the binary remains a narrow adapter.
    let _cli = Cli::parse();
    Ok(())
}
