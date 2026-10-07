//! Read-only store and index synchronization diagnostics.

use crate::{CliContext, StoreAccess, load_config_with_warnings, resolve_agent_id, resolve_store};
use anyhow::{Result, anyhow};
use clap::Args;
use hive_memory::{doctor, index, store};
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
    /// Walk the whole store tree to measure newest record times, index
    /// staleness, and cloud conflict copies. Off by default: on a cloud mount
    /// the walk costs one remote directory listing per directory.
    #[arg(long)]
    scan: bool,
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
    /// True only when the manifest reads and the store probe (or, with
    /// `--scan`, the full tree walk) finds no I/O error.
    reachable: bool,
    manifest_error: Option<String>,
    /// First I/O failure while probing or scanning the store tree, with its
    /// path. A dead network or FUSE mount lands here (ENOTCONN, EIO) instead of
    /// aborting the report, so callers can tell "store down" from "hm broken".
    store_error: Option<String>,
    /// Whether this report walked the whole store tree (`--scan`) and finished.
    /// The walk-derived fields (`newest_*`, `index_stale`,
    /// `cloud_conflict_files`) are measured only when this is true; otherwise
    /// they hold their empty values (null, false, 0).
    store_scanned: bool,
    index_path: PathBuf,
    index_exists: bool,
    index_modified_at: Option<String>,
    newest_note_at: Option<String>,
    newest_event_at: Option<String>,
    newest_canonical_at: Option<String>,
    index_stale: bool,
    cloud_conflict_files: usize,
    hosts: Vec<HostSyncStatus>,
    /// Sorted dotted paths of config keys this `hm` does not understand.
    ///
    /// The config syncs independently of `hm` releases, so a key can run ahead
    /// of (or outlive) the installed binary; unknown keys only warn, leaving
    /// that policy silently on defaults. This is the structured form of the
    /// `warning: unknown config key:` stderr lines, which are still printed.
    unknown_config_keys: Vec<String>,
}

/// Result of one read-only walk over a store's tree.
#[derive(Debug, Default)]
struct StoreScan {
    newest_note: Option<SystemTime>,
    newest_event: Option<SystemTime>,
    cloud_conflict_files: usize,
}

impl StoreScan {
    /// Walk the store, listing each directory once, for every walk-derived
    /// field.
    ///
    /// Cost is one directory listing per store directory plus one `stat` per
    /// inbox file, which is why only `--scan` runs it: on an rclone mount whose
    /// directory cache has expired, each listing is a Drive API call behind
    /// the mount-wide rate pacer (rclone's default allows 10 calls/s after a
    /// burst), so a store with a few hundred dated inbox directories takes
    /// seconds to tens of seconds.
    fn run(root: &Path) -> Result<Self> {
        let notes = root.join("inbox/notes");
        let events = root.join("inbox/events");
        let mut scan = Self::default();
        // The inbox trees are walked from their own tops (which follows a
        // symlinked tree, as recall does) and then skipped by the root walk.
        for (tree, newest) in [
            (&notes, &mut scan.newest_note),
            (&events, &mut scan.newest_event),
        ] {
            // Conflict copies count only where `hm doctor` looks, which is
            // not inside a symlinked tree: a count `hm doctor --fix` cannot
            // clear would warn forever.
            let doctor_walks = tree
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.is_dir());
            visit_files(tree, &[], &mut |path| {
                if doctor_walks {
                    scan.cloud_conflict_files += usize::from(is_conflict_copy(path));
                }
                if let Some(modified) = file_mtime(path)? {
                    *newest = Some(newest.map_or(modified, |current| current.max(modified)));
                }
                Ok(())
            })?;
        }
        visit_files(root, &[&notes, &events], &mut |path| {
            scan.cloud_conflict_files += usize::from(is_conflict_copy(path));
            Ok(())
        })?;
        Ok(scan)
    }
}

fn is_conflict_copy(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(doctor::is_cloud_conflict_name)
}

/// Bounded reachability probe: list the top level of each canonical tree.
///
/// Work is constant (one directory listing per tree) whatever the store's
/// size, so callers on a deadline can afford it, while still catching a dead
/// mount or an unreadable tree that would break recall. A missing tree, or
/// one that is not a directory, is empty, as the index fingerprint treats it.
fn probe_store(root: &Path) -> Result<()> {
    for tree in index::FINGERPRINT_ROOTS {
        let tree = root.join(tree);
        match std::fs::read_dir(&tree) {
            // Pull one batch of entries: opening a FUSE directory can succeed
            // while listing it is what reaches the backend and fails. (Tests
            // cannot fake that split; chmod makes the open itself fail.)
            Ok(mut entries) => {
                if let Some(Err(err)) = entries.next() {
                    return Err(anyhow!("read {}: {err}", tree.display()));
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(err) => return Err(anyhow!("read {}: {err}", tree.display())),
        }
    }
    Ok(())
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
    let loaded = load_config_with_warnings(context.config_path.as_deref())?;
    let unknown_config_keys = loaded
        .warnings
        .iter()
        .filter_map(|warning| warning.unknown_key().map(str::to_owned))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let config = loaded.config;
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
    let (manifest_read, store_id, manifest_schema_version, manifest_error) = match manifest {
        Ok(manifest) => (
            true,
            Some(manifest.store.id),
            Some(manifest.schema_version),
            None,
        ),
        Err(err) => (false, None, None, Some(err.to_string())),
    };

    // A probe or scan failure is a finding about the store, not a failure of
    // this command: report it and keep the rest of the (local) diagnostics.
    // Only a store whose manifest reads is walked: a missing root, or the
    // empty directory an unmounted cloud mountpoint leaves behind, would
    // otherwise "scan" clean and report zeros as measured. It still gets the
    // probe, so a dead mount keeps its `store_error`.
    let (scan, store_error) = if args.scan && manifest_read {
        match StoreScan::run(&store_config.root) {
            Ok(scan) => (Some(scan), None),
            Err(err) => (None, Some(err.to_string())),
        }
    } else {
        (
            None,
            probe_store(&store_config.root)
                .err()
                .map(|err| err.to_string()),
        )
    };
    let reachable = manifest_read && store_error.is_none();
    let store_scanned = scan.is_some();
    let StoreScan {
        newest_note,
        newest_event,
        cloud_conflict_files,
    } = scan.unwrap_or_default();
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
    let hosts = host_sync_status(&index_path);

    let output = SyncStatusJsonOutput {
        store: resolved_store.name,
        store_source: resolved_store.source.to_string(),
        store_id,
        manifest_schema_version,
        root: store_config.root.clone(),
        reachable,
        manifest_error,
        store_error,
        store_scanned,
        index_path,
        index_exists,
        index_modified_at: system_time_rfc3339(index_modified),
        newest_note_at: system_time_rfc3339(newest_note),
        newest_event_at: system_time_rfc3339(newest_event),
        newest_canonical_at: system_time_rfc3339(newest_canonical),
        index_stale,
        cloud_conflict_files,
        hosts,
        unknown_config_keys,
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
    if let Some(error) = output.store_error.as_deref() {
        println!("store_error: {error}");
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
    if output.store_scanned {
        println!(
            "index_stale: {}",
            if output.index_stale { "yes" } else { "no" }
        );
        println!("cloud_conflict_files: {}", output.cloud_conflict_files);
    } else if args.scan {
        // Asked for, but the walk failed or the root is missing: the reason
        // is in `store_error` or `manifest_error` above.
        println!("store_scan: incomplete");
    } else {
        // Do not print the unmeasured defaults as if they were findings.
        println!("store_scan: skipped (pass --scan for index_stale and cloud_conflict_files)");
    }
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
        // Keep the OS cause in the message itself: the CLI error printer
        // shows only the outermost anyhow context.
        Err(err) => Err(anyhow!("stat {}: {err}", path.display())),
    }
}

/// Visit every regular file under `root`, skipping `.quarantine` directories
/// and the directories in `skip`.
///
/// The quarantine holds conflict copies `hm doctor --fix` already set aside;
/// counting them (or failing on them) would report a resolved problem forever.
/// A missing `root`, or one that is not a directory, is an empty tree, as the
/// index fingerprint treats it; any other I/O error carries its path.
fn visit_files<F>(root: &Path, skip: &[&PathBuf], visit: &mut F) -> Result<()>
where
    F: FnMut(&Path) -> Result<()>,
{
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(());
        }
        Err(err) => return Err(anyhow!("read {}: {err}", root.display())),
    };

    for entry in entries {
        let entry = entry.map_err(|err| anyhow!("read {}: {err}", root.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|err| anyhow!("stat {}: {err}", entry.path().display()))?;
        let path = entry.path();
        if file_type.is_dir() {
            if doctor::is_quarantine_dir(&path) || skip.contains(&&path) {
                continue;
            }
            visit_files(&path, skip, visit)?;
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
