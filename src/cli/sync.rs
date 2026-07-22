//! Outbox flushing and index refresh command adapters.

use crate::{
    CliContext, context_session_id, hook_active, hook_options, load_config, rebuild_store_index,
    resolve_agent_id, resolve_host_id,
};
use anyhow::Result;
use clap::{Args, Subcommand};
use hive_memory::config::Config;
use hive_memory::{hook as memory_hook, index, outbox};
use serde::Serialize;

/// Local outbox commands.
#[derive(Debug, Subcommand)]
pub(crate) enum OutboxCommand {
    /// Flush local outbox writes to reachable stores.
    Flush(FlushArgs),
}

impl OutboxCommand {
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::Flush(args) => args.wants_json(),
        }
    }
}

/// Arguments for `hm refresh`.
#[derive(Debug, Args)]
pub(crate) struct RefreshArgs {
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

impl RefreshArgs {
    pub(crate) fn wants_json(&self) -> bool {
        self.json
    }
}

/// Arguments for `hm flush`.
#[derive(Debug, Args)]
pub(crate) struct FlushArgs {
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

impl FlushArgs {
    pub(crate) fn wants_json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RefreshReport {
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

impl RefreshReport {
    /// Attach the receipt count observed by the lifecycle adapter.
    pub(crate) fn record_receipts(&mut self, count: usize) {
        self.write_receipts = count;
    }
}

pub(crate) fn run_outbox(command: OutboxCommand, context: CliContext) -> Result<()> {
    match command {
        OutboxCommand::Flush(args) => run_flush(args, context),
    }
}

pub(crate) fn run_refresh(args: RefreshArgs, context: CliContext) -> Result<()> {
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

    let mut report = perform(&config, args.force)?;
    if let Some(cursor) = receipt_cursor {
        report.record_receipts(cursor.unrefreshed);
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

fn emit_refresh_report(report: &RefreshReport, args: &RefreshArgs) -> Result<()> {
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

pub(crate) fn run_flush(args: FlushArgs, context: CliContext) -> Result<()> {
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

pub(crate) fn perform(config: &Config, forced: bool) -> Result<RefreshReport> {
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
        super::search::refresh_tantivy_index(
            config,
            store_name,
            &store_config.root,
            &report.entries,
        );
        indexes += 1;
    }

    Ok(RefreshReport {
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
fn skipped_refresh_report(forced: bool) -> RefreshReport {
    RefreshReport {
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
fn coalesced_refresh_report(forced: bool, write_receipts: usize) -> RefreshReport {
    RefreshReport {
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
