//! Inbox triage and promotion CLI arguments, output models, and handlers.

use crate::{
    CliContext, StoreAccess, hook_options, load_config, read_store_manifest, rebuild_store_index,
    resolve_agent_id, resolve_host_id, resolve_store,
};
use anyhow::Result;
use clap::{Args, Subcommand};
use hive_memory::{curation, index};
use serde::Serialize;
use std::path::PathBuf;
use time::OffsetDateTime;

/// Raw inbox triage commands.
#[derive(Debug, Subcommand)]
pub(crate) enum InboxCommand {
    /// List raw inbox notes that still need triage.
    List(InboxListArgs),
    /// List unpromoted raw notes older than N days.
    Stale(InboxStaleArgs),
    /// Show one raw inbox note.
    Show(InboxShowArgs),
}

impl InboxCommand {
    /// Return whether this invocation requires structured error output.
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::List(args) => args.json,
            Self::Stale(args) => args.json,
            Self::Show(args) => args.json,
        }
    }
}

/// Arguments for `hm inbox list`.
#[derive(Debug, Args)]
pub(crate) struct InboxListArgs {
    /// Include notes that already have a promotion event.
    #[arg(long)]
    all: bool,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm inbox stale`.
#[derive(Debug, Args)]
pub(crate) struct InboxStaleArgs {
    /// Minimum age in days for unpromoted notes.
    #[arg(long)]
    days: i64,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm inbox show`.
#[derive(Debug, Args)]
pub(crate) struct InboxShowArgs {
    /// Raw note id to show.
    note_id: String,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm promote`.
#[derive(Debug, Args)]
pub(crate) struct PromoteArgs {
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

impl PromoteArgs {
    /// Return whether this invocation requires structured error output.
    pub(crate) fn wants_json(&self) -> bool {
        self.json
    }
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

/// Run one raw inbox triage command.
pub(crate) fn run_inbox(command: InboxCommand, context: CliContext) -> Result<()> {
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

/// Promote one raw inbox note into curated memory.
pub(crate) fn run_promote(args: PromoteArgs, context: CliContext) -> Result<()> {
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
