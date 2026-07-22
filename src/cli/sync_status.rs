//! Read-only store and index synchronization diagnostics.

use crate::{CliContext, StoreAccess, load_config, resolve_agent_id, resolve_store};
use anyhow::Result;
use clap::Args;
use hive_memory::{index, store};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use time::OffsetDateTime;

/// Arguments for `hm sync-status`.
#[derive(Debug, Args)]
pub(crate) struct SyncStatusArgs {
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

impl SyncStatusArgs {
    pub(crate) fn wants_json(&self) -> bool {
        self.json
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

pub(crate) fn run(args: SyncStatusArgs, context: CliContext) -> Result<()> {
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
