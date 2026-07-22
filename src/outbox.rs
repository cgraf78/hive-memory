//! Durable local outbox and flush support.
//!
//! The outbox lives under `data_dir`, not `state_dir`, because an offline write
//! is user data until it reaches a store root. Flush is deliberately local and
//! identity-checked: cloud drives move bytes between machines, while `hm flush`
//! only reconciles hive-memory's own pending payloads into a reachable store.
//! Store aliases are convenience labels; the manifest id recorded in each item
//! is the safety check that prevents a later alias/path change from publishing
//! memory into the wrong hive.

use crate::{config, event, note, store, write};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Component, Path, PathBuf};
use time::OffsetDateTime;

/// Outbox metadata schema supported by this build.
pub const OUTBOX_SCHEMA_VERSION: u32 = 1;

/// Last-seen store identity cache schema supported by this build.
pub const STORE_IDENTITIES_SCHEMA_VERSION: u32 = 1;

/// Serialized state of one outbox item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutboxState {
    /// Store identity is known; auto-flush may attempt the item.
    Pending,
    /// Store identity was not known at enqueue time; requires explicit binding.
    ///
    /// Unbound items are intentionally ignored by automatic flush. A human or
    /// higher-level repair command must decide which stable store identity owns
    /// the payload before it can leave the local data directory.
    Unbound,
}

/// Metadata stored at `data_dir/outbox/<store>/<id>/meta.toml`.
///
/// This file is the recovery contract for an offline write. Payload files hold
/// the bytes, while metadata records exactly where those bytes may land and
/// which store manifest id must be present before publishing is allowed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxMeta {
    /// Outbox metadata schema version.
    pub schema_version: u32,
    /// Stable outbox item id.
    pub id: String,
    /// Target store alias.
    pub store: String,
    /// Expected store manifest id. Missing only for unbound items.
    pub expected_store_id: Option<String>,
    /// Store-relative final Markdown note path.
    ///
    /// Flush rejects absolute paths and parent components so outbox metadata
    /// cannot write outside the target store root even if a file is corrupted.
    pub final_note_path: String,
    /// SHA-256 of `note.md`.
    pub note_sha256: String,
    /// Store-relative final JSON event path, when present.
    pub final_event_path: Option<String>,
    /// SHA-256 of `event.json`, when present.
    pub event_sha256: Option<String>,
    /// When this item was enqueued.
    pub created_at: String,
    /// Number of prior flush attempts.
    pub attempt_count: u32,
    /// Last flush error, if any.
    pub last_error: Option<String>,
    /// Current outbox state.
    pub state: OutboxState,
}

/// Request to flush durable outbox data.
#[derive(Debug, Clone)]
pub struct FlushInput<'a> {
    /// Durable tool data directory containing `outbox/`.
    pub data_dir: &'a Path,
    /// Configured stores keyed by local alias.
    pub stores: &'a BTreeMap<String, config::StoreConfig>,
    /// Host id used for the per-store archive path.
    ///
    /// This is not an authorization or ownership check. It only partitions
    /// archive snapshots so multiple machines can keep their flush receipts
    /// under the same synced store without clobbering each other.
    pub host_id: &'a str,
    /// Atomic writer options for final payload and archive writes.
    pub options: write::AtomicWriteOptions,
}

/// Request to bind one unbound outbox item to a reachable store.
#[derive(Debug, Clone)]
pub struct BindInput<'a> {
    /// Durable tool data directory containing `outbox/`.
    pub data_dir: &'a Path,
    /// Configured stores keyed by local alias.
    pub stores: &'a BTreeMap<String, config::StoreConfig>,
    /// Outbox item id to bind.
    pub item_id: &'a str,
    /// Target store alias that should own the item.
    ///
    /// The alias selects local configuration only. The reachable manifest id is
    /// copied into the item during binding and remains the durable ownership
    /// check used by later automatic flushes.
    pub store: &'a str,
    /// Atomic writer options for payload and metadata rewrites.
    pub options: write::AtomicWriteOptions,
}

/// Result of binding one unbound outbox item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindReport {
    /// Outbox item id.
    pub id: String,
    /// Target store alias.
    pub store: String,
    /// Target manifest id recorded on the item.
    pub expected_store_id: String,
    /// Item directory after binding.
    ///
    /// Binding may move an item that was queued under an old alias into the
    /// selected store alias. The metadata, not this directory name, remains the
    /// source of truth for flush policy.
    pub item_dir: PathBuf,
}

/// Request to enqueue one offline write for a later flush.
#[derive(Debug, Clone)]
pub struct EnqueueInput<'a> {
    /// Durable tool data directory containing `outbox/`.
    pub data_dir: &'a Path,
    /// Target store alias from current policy resolution.
    pub store: &'a str,
    /// Stable outbox item id, normally the memory id.
    pub id: &'a str,
    /// Expected store manifest id. Missing means the item stays unbound.
    pub expected_store_id: Option<String>,
    /// Store-relative final Markdown note path.
    pub final_note_path: String,
    /// Rendered Markdown note payload.
    pub note: Vec<u8>,
    /// Store-relative final JSON event path, when present.
    pub final_event_path: Option<String>,
    /// Rendered JSON event payload, when present.
    pub event: Option<Vec<u8>>,
    /// Initial outbox state.
    pub state: OutboxState,
    /// Atomic writer options for payload and metadata writes.
    pub options: write::AtomicWriteOptions,
}

/// Result of enqueueing an offline write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnqueueReport {
    /// Outbox item directory.
    pub item_dir: PathBuf,
    /// Metadata path written inside the item directory.
    pub meta_path: PathBuf,
}

/// Last-seen identity cache stored under `data_dir/store-identities.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreIdentityCache {
    /// Cache schema version.
    pub schema_version: u32,
    /// Cached store identities keyed by local alias.
    pub stores: BTreeMap<String, CachedStoreIdentity>,
}

/// One cached store identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedStoreIdentity {
    /// Stable manifest id last observed for this alias.
    pub store_id: String,
    /// RFC3339 timestamp when this identity was observed.
    pub seen_at: String,
}

/// Summary of one flush run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FlushReport {
    /// Items whose payloads were newly written into the target store.
    pub flushed: usize,
    /// Items whose final paths already contained the same payload hash.
    pub skipped: usize,
    /// Items that could not be flushed because continuing would be unsafe.
    ///
    /// Failures are policy or consistency problems, such as identity mismatch or
    /// different content already at the final path. They require attention.
    pub failed: usize,
    /// Items that require explicit store binding before flush.
    pub unbound: usize,
    /// Items left pending because the target store is currently unavailable.
    ///
    /// Pending is non-fatal. It usually means a removable disk or cloud folder
    /// is not mounted yet, so the item should be retried later unchanged.
    pub pending: usize,
    /// Per-item results in deterministic scan order.
    pub items: Vec<FlushItemReport>,
}

/// Result for one outbox item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlushItemReport {
    /// Outbox item id.
    pub id: String,
    /// Target store alias from metadata.
    pub store: String,
    /// Original outbox state: `pending` or `unbound`.
    pub state: String,
    /// Flush result: `flushed`, `skipped`, `failed`, `unbound`, or `pending`.
    pub result: String,
    /// Human-readable result detail.
    pub message: String,
}

/// Outbox operation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxError {
    /// Filesystem operation failed.
    Io {
        /// Operation that failed.
        action: &'static str,
        /// Path involved in the failure.
        path: PathBuf,
        /// Original error rendered for CLI diagnostics.
        message: String,
    },
    /// Metadata TOML was malformed.
    ParseMeta {
        /// Metadata path that failed to parse.
        path: PathBuf,
        /// TOML parse error.
        message: String,
    },
    /// Metadata TOML could not be rendered.
    RenderMeta(String),
    /// Outbox item could not be bound safely.
    Bind(String),
    /// Store identity cache TOML was malformed.
    ParseIdentityCache(String),
    /// Store identity cache TOML could not be rendered.
    RenderIdentityCache(String),
    /// Metadata schema is unsupported.
    UnsupportedSchema {
        /// Metadata path with the unsupported schema.
        path: PathBuf,
        /// Schema version found on disk.
        version: u32,
    },
}

impl Display for OutboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                message,
            } => write!(f, "failed to {action} {}: {message}", path.display()),
            Self::ParseMeta { path, message } => {
                write!(
                    f,
                    "failed to parse outbox metadata {}: {message}",
                    path.display()
                )
            }
            Self::RenderMeta(message) => write!(f, "failed to render outbox metadata: {message}"),
            Self::Bind(message) => write!(f, "failed to bind outbox item: {message}"),
            Self::ParseIdentityCache(message) => {
                write!(f, "failed to parse store identity cache: {message}")
            }
            Self::RenderIdentityCache(message) => {
                write!(f, "failed to render store identity cache: {message}")
            }
            Self::UnsupportedSchema { path, version } => write!(
                f,
                "unsupported outbox schema_version {version} in {}",
                path.display()
            ),
        }
    }
}

impl Error for OutboxError {}

/// Return the local last-seen store identity cache path.
pub fn store_identities_path(data_dir: &Path) -> PathBuf {
    data_dir.join("store-identities.toml")
}

/// Record a successfully observed manifest identity for a configured alias.
///
/// The cache is advisory policy input for future offline writes, not canonical
/// memory. Flush still verifies the target manifest id before publishing any
/// queued payload, so a stale cache can at worst create a pending item that will
/// later refuse to flush.
pub fn record_store_identity(
    data_dir: &Path,
    store_name: &str,
    store_id: &str,
    options: &write::AtomicWriteOptions,
) -> Result<PathBuf, OutboxError> {
    let path = store_identities_path(data_dir);
    let mut cache = load_store_identity_cache(data_dir)?;
    cache.schema_version = STORE_IDENTITIES_SCHEMA_VERSION;
    cache.stores.insert(
        store_name.to_owned(),
        CachedStoreIdentity {
            store_id: store_id.to_owned(),
            seen_at: OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .expect("RFC3339 formatting should not fail"),
        },
    );
    let contents = toml::to_string_pretty(&cache)
        .map_err(|err| OutboxError::RenderIdentityCache(err.to_string()))?;
    write::write_atomic(&path, contents.as_bytes(), options).map_err(|err| OutboxError::Io {
        action: "write store identity cache",
        path: path.clone(),
        message: err.to_string(),
    })?;
    Ok(path)
}

/// Return the cached manifest id for a store alias, when known.
pub fn cached_store_identity(
    data_dir: &Path,
    store_name: &str,
) -> Result<Option<String>, OutboxError> {
    Ok(load_store_identity_cache(data_dir)?
        .stores
        .get(store_name)
        .map(|entry| entry.store_id.clone()))
}

fn load_store_identity_cache(data_dir: &Path) -> Result<StoreIdentityCache, OutboxError> {
    let path = store_identities_path(data_dir);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoreIdentityCache {
                schema_version: STORE_IDENTITIES_SCHEMA_VERSION,
                stores: BTreeMap::new(),
            });
        }
        Err(err) => return Err(io_error("read store identity cache", &path, err)),
    };
    let cache = toml::from_str::<StoreIdentityCache>(&contents)
        .map_err(|err| OutboxError::ParseIdentityCache(err.to_string()))?;
    if cache.schema_version != STORE_IDENTITIES_SCHEMA_VERSION {
        return Err(OutboxError::UnsupportedSchema {
            path,
            version: cache.schema_version,
        });
    }
    Ok(cache)
}

/// Enqueue one fully rendered memory payload under `data_dir/outbox`.
///
/// Offline enqueue receives already rendered note/event bytes because write
/// policy belongs to the caller: store affinity, scope, audience, and secret
/// checks must be resolved before durable user data enters the outbox. This
/// function owns only the recovery envelope and payload hashing contract.
pub fn enqueue(input: EnqueueInput<'_>) -> Result<EnqueueReport, OutboxError> {
    let item_dir = input
        .data_dir
        .join("outbox")
        .join(input.store)
        .join(input.id);
    fs::create_dir_all(&item_dir).map_err(|err| io_error("create outbox item", &item_dir, err))?;
    let event_sha256 = input.event.as_ref().map(|event| sha256(event));
    let meta = OutboxMeta {
        schema_version: OUTBOX_SCHEMA_VERSION,
        id: input.id.to_owned(),
        store: input.store.to_owned(),
        expected_store_id: input.expected_store_id,
        final_note_path: input.final_note_path,
        note_sha256: sha256(&input.note),
        final_event_path: input.final_event_path,
        event_sha256,
        created_at: OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("RFC3339 formatting should not fail"),
        attempt_count: 0,
        last_error: None,
        state: input.state,
    };

    write::write_atomic_create_new(&item_dir.join("note.md"), &input.note, &input.options)
        .map_err(|err| OutboxError::Io {
            action: "write outbox note",
            path: item_dir.join("note.md"),
            message: err.to_string(),
        })?;
    if let Some(event) = input.event {
        write::write_atomic_create_new(&item_dir.join("event.json"), &event, &input.options)
            .map_err(|err| OutboxError::Io {
                action: "write outbox event",
                path: item_dir.join("event.json"),
                message: err.to_string(),
            })?;
    }
    let meta_path = item_dir.join("meta.toml");
    write::write_atomic_create_new(&meta_path, render_meta(&meta)?.as_bytes(), &input.options)
        .map_err(|err| OutboxError::Io {
            action: "write outbox metadata",
            path: meta_path.clone(),
            message: err.to_string(),
        })?;

    Ok(EnqueueReport {
        item_dir,
        meta_path,
    })
}

/// Bind one unbound outbox item to a reachable store manifest.
///
/// Binding rewrites the queued note/event metadata with the target manifest id
/// before changing the item to `pending`. That avoids publishing placeholder
/// identity data into canonical memory when an unbound item is later flushed.
pub fn bind_item(input: BindInput<'_>) -> Result<BindReport, OutboxError> {
    let original_item_dir = find_item_dir(input.data_dir, input.item_id)?
        .ok_or_else(|| OutboxError::Bind(format!("outbox item not found: {}", input.item_id)))?;
    let mut meta = read_meta(&original_item_dir.join("meta.toml"))?;
    if meta.id != input.item_id {
        return Err(OutboxError::Bind(format!(
            "outbox item id mismatch: directory is {}, metadata is {}",
            input.item_id, meta.id
        )));
    }
    if meta.state != OutboxState::Unbound {
        return Err(OutboxError::Bind(format!(
            "outbox item {} is not unbound",
            meta.id
        )));
    }
    let store_config = input.stores.get(input.store).ok_or_else(|| {
        OutboxError::Bind(format!("target store is not configured: {}", input.store))
    })?;
    let manifest = store::read_manifest(&store_config.root)
        .map_err(|err| OutboxError::Bind(format!("target store is unavailable: {err}")))?;

    let target_dir = input
        .data_dir
        .join("outbox")
        .join(input.store)
        .join(&meta.id);
    let item_dir = if target_dir == original_item_dir {
        original_item_dir
    } else {
        if target_dir.exists() {
            return Err(OutboxError::Bind(format!(
                "target outbox item already exists: {}",
                target_dir.display()
            )));
        }
        if let Some(parent) = target_dir.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| io_error("create target outbox parent", parent, err))?;
        }
        // Move first, then rewrite metadata. If the move fails, the item stays
        // unbound and safe to retry; a failed semantic rewrite after the move
        // still leaves automatic flush blocked by `state = "unbound"`.
        fs::rename(&original_item_dir, &target_dir)
            .map_err(|err| io_error("move bound outbox item", &target_dir, err))?;
        target_dir
    };

    rewrite_payload_identity(
        &item_dir,
        &mut meta,
        input.store,
        &manifest.store.id,
        &manifest.store.name,
        &input.options,
    )?;
    record_store_identity(
        input.data_dir,
        input.store,
        &manifest.store.id,
        &input.options,
    )?;

    Ok(BindReport {
        id: meta.id,
        store: input.store.to_owned(),
        expected_store_id: manifest.store.id,
        item_dir,
    })
}

/// Flush all auto-bindable pending outbox items.
///
/// Same-hash collisions are treated as already flushed and remove the outbox
/// item after archiving. Different-hash collisions are recorded as `failed`
/// because continuing would overwrite unrelated canonical memory. Unbound items
/// are counted and left untouched for a future explicit bind operation.
/// Temporarily unreachable stores are reported as pending rather than failures
/// so hook-time refresh can run safely when a user is away from one of their
/// store locations.
///
/// A single corrupt or unwritable item must never strand the rest of the queue:
/// the outbox exists precisely to preserve offline writes, so every recoverable
/// per-item problem (unparsable metadata, payload I/O failure, archive write
/// failure) is bucketed into the item's own `FlushItemReport` and the loop keeps
/// going. The only hard error is failing to scan the outbox root itself, because
/// without a directory listing there is no per-item work to report against.
pub fn flush(input: FlushInput<'_>) -> Result<FlushReport, OutboxError> {
    let mut report = FlushReport::default();
    let outbox_root = input.data_dir.join("outbox");
    let item_dirs = match collect_item_dirs(&outbox_root) {
        Ok(paths) => paths,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(err) => return Err(io_error("scan outbox", &outbox_root, err)),
    };

    for item_dir in item_dirs {
        let item = flush_item(&input, &item_dir);
        match item.result.as_str() {
            "flushed" => report.flushed += 1,
            "skipped" => report.skipped += 1,
            "failed" => report.failed += 1,
            "unbound" => report.unbound += 1,
            "pending" => report.pending += 1,
            _ => {}
        }
        report.items.push(item);
    }

    Ok(report)
}

/// Flush one outbox item, returning its bucketed result.
///
/// This function never returns `Err`: a flush batch must survive a single bad
/// item, so every recoverable failure is mapped to a `failed`/`pending`/
/// `skipped` `FlushItemReport` instead of aborting the whole run. Policy and
/// safety refusals (identity mismatch, different-content conflict, hash
/// mismatch, incomplete event metadata) are likewise `failed` reports, so the
/// manifest-identity safety gate and idempotency behavior are unchanged.
fn flush_item(input: &FlushInput<'_>, item_dir: &Path) -> FlushItemReport {
    let meta_path = item_dir.join("meta.toml");
    let meta = match read_meta(&meta_path) {
        Ok(meta) => meta,
        Err(err) => {
            // A concurrent flusher may have removed the item (and its meta.toml)
            // between the directory scan and this read. A missing meta file is
            // "already done", not a corruption to surface. Anything else
            // (unparsable, unreadable, unsupported schema) is a per-item repair
            // problem: bucket it as failed -- deriving id/store from the path,
            // since we have no parsed metadata -- and keep flushing the rest.
            if meta_path
                .try_exists()
                .map(|exists| !exists)
                .unwrap_or(false)
            {
                return path_item_report(
                    item_dir,
                    "skipped",
                    "item already removed by another flush",
                );
            }
            return path_item_report(item_dir, "failed", err.to_string());
        }
    };
    if meta.state == OutboxState::Unbound {
        return item_report(&meta, "unbound", "item requires explicit binding");
    }
    let Some(expected_store_id) = meta.expected_store_id.as_deref() else {
        return item_report(&meta, "unbound", "item has no expected store id");
    };
    let Some(store_config) = input.stores.get(&meta.store) else {
        return item_report(&meta, "failed", "target store is not configured");
    };
    let manifest = match store::read_manifest(&store_config.root) {
        Ok(manifest) => manifest,
        Err(err) => {
            return item_report(
                &meta,
                "pending",
                format!("target store is unavailable: {err}"),
            );
        }
    };
    if manifest.store.id != expected_store_id {
        return item_report(
            &meta,
            "failed",
            "target store manifest id does not match outbox metadata",
        );
    }

    let event_payload = match (&meta.final_event_path, &meta.event_sha256) {
        (Some(path), Some(hash)) => Some((path.as_str(), hash.as_str())),
        (None, None) => None,
        _ => {
            return item_report(&meta, "failed", "event path/hash metadata is incomplete");
        }
    };

    let note_source = item_dir.join("note.md");
    // Publish both payloads before removing the local recovery copy. If either
    // payload collides or fails validation, the item remains in the outbox so a
    // later human repair can inspect the original bytes and metadata together.
    // A payload I/O failure (missing/unreadable source, unreadable destination)
    // is recoverable per-item: report it as failed and leave the item in place
    // rather than aborting the whole flush.
    let note_result = match publish_payload(
        &note_source,
        &store_config.root,
        &meta.final_note_path,
        &meta.note_sha256,
        &input.options,
    ) {
        Ok(result) => result,
        Err(err) => return item_report(&meta, "failed", err.to_string()),
    };
    let event_result = match event_payload {
        Some((path, hash)) => match publish_payload(
            &item_dir.join("event.json"),
            &store_config.root,
            path,
            hash,
            &input.options,
        ) {
            Ok(result) => Some(result),
            Err(err) => return item_report(&meta, "failed", err.to_string()),
        },
        None => None,
    };

    if note_result == PublishResult::HashMismatch
        || event_result == Some(PublishResult::HashMismatch)
    {
        return item_report(
            &meta,
            "failed",
            "payload hash does not match outbox metadata",
        );
    }
    if note_result == PublishResult::Conflict || event_result == Some(PublishResult::Conflict) {
        return item_report(&meta, "failed", "final path exists with different content");
    }

    // Payloads are now safely in the store. Archiving and removing the local
    // recovery copy are best-effort post-flush bookkeeping: an archive write
    // failure should not poison the rest of the batch, and a NotFound while
    // removing the item means a concurrent flusher already cleaned it up.
    if let Err(err) = write_archive(input, &store_config.root, item_dir, &meta) {
        return item_report(&meta, "failed", err.to_string());
    }
    match fs::remove_dir_all(item_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return item_report(&meta, "skipped", "item already removed by another flush");
        }
        Err(err) => {
            return item_report(
                &meta,
                "failed",
                io_error("remove flushed item", item_dir, err).to_string(),
            );
        }
    }

    if note_result == PublishResult::AlreadyPresent
        && event_result
            .map(|result| result == PublishResult::AlreadyPresent)
            .unwrap_or(true)
    {
        item_report(&meta, "skipped", "payload already present")
    } else {
        item_report(&meta, "flushed", "payload flushed")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishResult {
    Written,
    AlreadyPresent,
    HashMismatch,
    Conflict,
}

fn publish_payload(
    source: &Path,
    store_root: &Path,
    final_relative_path: &str,
    expected_hash: &str,
    options: &write::AtomicWriteOptions,
) -> Result<PublishResult, OutboxError> {
    let contents = fs::read(source).map_err(|err| io_error("read outbox payload", source, err))?;
    let actual_hash = sha256(&contents);
    if actual_hash != expected_hash {
        return Ok(PublishResult::HashMismatch);
    }
    let relative = validate_relative_path(final_relative_path)?;
    let final_path = store_root.join(relative);
    // The payload hash check happens before the final-path collision check so a
    // corrupted outbox file cannot be mistaken for "already present" just
    // because the destination currently contains valid bytes from another run.
    match fs::read(&final_path) {
        Ok(existing) if sha256(&existing) == expected_hash => Ok(PublishResult::AlreadyPresent),
        Ok(_) => Ok(PublishResult::Conflict),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            match write::write_atomic_create_new(&final_path, &contents, options) {
                Ok(_) => Ok(PublishResult::Written),
                // The destination was absent at the read above but `create_new`
                // refused to publish: a concurrent flusher of the same item won
                // the race between our NotFound check and our publish. This is a
                // benign lost race, not a real failure — re-read the destination
                // and let identical bytes collapse into the idempotent
                // already-present path. We re-check by content (not by parsing the
                // write error's display text) so the genuine different-content
                // conflict and any other I/O failure still surface as errors. If
                // the re-read finds the file gone again (the winner's own cleanup,
                // or a transient FS state), the original write error stands.
                Err(write_err) => match fs::read(&final_path) {
                    Ok(existing) if sha256(&existing) == expected_hash => {
                        Ok(PublishResult::AlreadyPresent)
                    }
                    Ok(_) => Ok(PublishResult::Conflict),
                    Err(_) => Err(OutboxError::Io {
                        action: "write final payload",
                        path: final_path,
                        message: write_err.to_string(),
                    }),
                },
            }
        }
        Err(err) => Err(io_error("read final payload", &final_path, err)),
    }
}

fn write_archive(
    input: &FlushInput<'_>,
    store_root: &Path,
    item_dir: &Path,
    meta: &OutboxMeta,
) -> Result<(), OutboxError> {
    let date = OffsetDateTime::now_utc().date();
    let archive = store_root
        .join(".outbox-archive")
        .join(input.host_id)
        .join(format!(
            "{:04}-{:02}-{:02}",
            date.year(),
            u8::from(date.month()),
            date.day()
        ))
        .join(&meta.id);
    fs::create_dir_all(&archive).map_err(|err| io_error("create outbox archive", &archive, err))?;
    // The archive is a post-flush recovery aid, not the canonical record. It is
    // still written through the same atomic/durability policy so a successful
    // flush does not leave behind a torn diagnostic snapshot.
    archive_file(item_dir, &archive, "meta.toml", &input.options)?;
    archive_file(item_dir, &archive, "note.md", &input.options)?;
    if meta.final_event_path.is_some() {
        archive_file(item_dir, &archive, "event.json", &input.options)?;
    }
    Ok(())
}

fn archive_file(
    item_dir: &Path,
    archive: &Path,
    name: &str,
    options: &write::AtomicWriteOptions,
) -> Result<(), OutboxError> {
    let source = item_dir.join(name);
    if !source.is_file() {
        return Ok(());
    }
    let contents =
        fs::read(&source).map_err(|err| io_error("read archive source", &source, err))?;
    let target = archive.join(name);
    write::write_atomic(&target, &contents, options).map_err(|err| OutboxError::Io {
        action: "write archive file",
        path: target,
        message: err.to_string(),
    })?;
    Ok(())
}

fn collect_item_dirs(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for store_entry in fs::read_dir(root)? {
        let store_entry = store_entry?;
        if !store_entry.file_type()?.is_dir() {
            continue;
        }
        for item_entry in fs::read_dir(store_entry.path())? {
            let item_entry = item_entry?;
            if item_entry.file_type()?.is_dir() {
                dirs.push(item_entry.path());
            }
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn find_item_dir(data_dir: &Path, item_id: &str) -> Result<Option<PathBuf>, OutboxError> {
    let outbox_root = data_dir.join("outbox");
    let item_dirs = match collect_item_dirs(&outbox_root) {
        Ok(paths) => paths,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_error("scan outbox", &outbox_root, err)),
    };
    let mut matches = item_dirs
        .into_iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some(item_id));
    let first = matches.next();
    // Item ids are globally generated, but unbound recovery may have queued an
    // item under a stale or placeholder alias. Binding by id is ergonomic only
    // if ambiguity is refused rather than silently picking one store directory.
    if matches.next().is_some() {
        return Err(OutboxError::Bind(format!(
            "outbox item id is ambiguous: {item_id}"
        )));
    }
    Ok(first)
}

fn rewrite_payload_identity(
    item_dir: &Path,
    meta: &mut OutboxMeta,
    store_name: &str,
    store_id: &str,
    manifest_store_name: &str,
    options: &write::AtomicWriteOptions,
) -> Result<(), OutboxError> {
    // Binding changes ownership data embedded in the payloads, not just the
    // sidecar metadata. Re-render through the note/event parsers so the hashes
    // recorded below describe the exact bytes that a later flush will publish.
    let note_path = item_dir.join("note.md");
    let note_contents = fs::read_to_string(&note_path)
        .map_err(|err| io_error("read outbox note", &note_path, err))?;
    let mut parsed_note =
        note::parse_note(&note_contents).map_err(|err| OutboxError::Bind(err.to_string()))?;
    parsed_note.front_matter.store_id = store_id.to_owned();
    parsed_note.front_matter.store_name = manifest_store_name.to_owned();
    let rendered_note =
        note::render_note(&parsed_note).map_err(|err| OutboxError::Bind(err.to_string()))?;
    write::write_atomic(&note_path, rendered_note.as_bytes(), options).map_err(|err| {
        OutboxError::Io {
            action: "rewrite outbox note",
            path: note_path.clone(),
            message: err.to_string(),
        }
    })?;

    let event_sha256 = if meta.final_event_path.is_some() {
        let event_path = item_dir.join("event.json");
        let event_contents = fs::read_to_string(&event_path)
            .map_err(|err| io_error("read outbox event", &event_path, err))?;
        let mut parsed_event = event::parse_event(&event_contents)
            .map_err(|err| OutboxError::Bind(err.to_string()))?;
        parsed_event.store_id = store_id.to_owned();
        parsed_event.store_name = manifest_store_name.to_owned();
        let rendered_event =
            event::render_event(&parsed_event).map_err(|err| OutboxError::Bind(err.to_string()))?;
        write::write_atomic(&event_path, rendered_event.as_bytes(), options).map_err(|err| {
            OutboxError::Io {
                action: "rewrite outbox event",
                path: event_path.clone(),
                message: err.to_string(),
            }
        })?;
        Some(sha256(rendered_event.as_bytes()))
    } else {
        None
    };

    meta.store = store_name.to_owned();
    meta.expected_store_id = Some(store_id.to_owned());
    meta.note_sha256 = sha256(rendered_note.as_bytes());
    meta.event_sha256 = event_sha256;
    meta.state = OutboxState::Pending;
    let meta_path = item_dir.join("meta.toml");
    write::write_atomic(&meta_path, render_meta(meta)?.as_bytes(), options).map_err(|err| {
        OutboxError::Io {
            action: "rewrite outbox metadata",
            path: meta_path,
            message: err.to_string(),
        }
    })?;
    Ok(())
}

fn read_meta(path: &Path) -> Result<OutboxMeta, OutboxError> {
    let contents =
        fs::read_to_string(path).map_err(|err| io_error("read outbox metadata", path, err))?;
    let meta: OutboxMeta = toml::from_str(&contents).map_err(|err| OutboxError::ParseMeta {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    if meta.schema_version != OUTBOX_SCHEMA_VERSION {
        return Err(OutboxError::UnsupportedSchema {
            path: path.to_path_buf(),
            version: meta.schema_version,
        });
    }
    Ok(meta)
}

/// Render outbox metadata with stable TOML formatting.
pub fn render_meta(meta: &OutboxMeta) -> Result<String, OutboxError> {
    toml::to_string_pretty(meta).map_err(|err| OutboxError::RenderMeta(err.to_string()))
}

/// SHA-256 helper for tests and enqueue code.
pub fn payload_sha256(contents: &[u8]) -> String {
    sha256(contents)
}

fn validate_relative_path(path: &str) -> Result<PathBuf, OutboxError> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(OutboxError::Io {
            action: "validate final path",
            path: path.to_path_buf(),
            message: "path must be relative".to_owned(),
        });
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            Component::CurDir => {}
            _ => {
                return Err(OutboxError::Io {
                    action: "validate final path",
                    path: path.to_path_buf(),
                    message: "path must not contain parent or prefix components".to_owned(),
                });
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(OutboxError::Io {
            action: "validate final path",
            path: path.to_path_buf(),
            message: "path must not be empty".to_owned(),
        });
    }
    Ok(normalized)
}

fn item_report(meta: &OutboxMeta, result: &str, message: impl Into<String>) -> FlushItemReport {
    FlushItemReport {
        id: meta.id.clone(),
        store: meta.store.clone(),
        state: match meta.state {
            OutboxState::Pending => "pending",
            OutboxState::Unbound => "unbound",
        }
        .to_owned(),
        result: result.to_owned(),
        message: message.into(),
    }
}

/// Build a per-item report when metadata could not be parsed.
///
/// Without parsed metadata we still want a stable, human-locatable result, so
/// the id and store are recovered from the on-disk layout
/// (`outbox/<store>/<id>/`). The `state` is reported as `unknown` because the
/// metadata that records it is exactly what we failed to read.
fn path_item_report(item_dir: &Path, result: &str, message: impl Into<String>) -> FlushItemReport {
    let id = item_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_owned();
    let store = item_dir
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_owned();
    FlushItemReport {
        id,
        store,
        state: "unknown".to_owned(),
        result: result.to_owned(),
        message: message.into(),
    }
}

fn sha256(contents: &[u8]) -> String {
    format!("{:x}", Sha256::digest(contents))
}

fn io_error(action: &'static str, path: &Path, err: std::io::Error) -> OutboxError {
    OutboxError::Io {
        action,
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `validate_relative_path` is the last-line guard that keeps corrupted or
    /// malicious outbox metadata from writing outside the target store root, so
    /// it must reject every escape shape and accept a normal store-relative path.
    #[test]
    fn validate_relative_path_rejects_parent_components() {
        let err = validate_relative_path("../escape.md").expect_err("`..` must be rejected");
        match err {
            OutboxError::Io { message, .. } => {
                assert!(
                    message.contains("parent or prefix"),
                    "unexpected: {message}"
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn validate_relative_path_rejects_parent_in_middle() {
        // A `..` deeper in the path must be refused too, not just a leading one.
        validate_relative_path("inbox/../../escape.md")
            .expect_err("interior `..` must be rejected");
    }

    #[test]
    fn validate_relative_path_rejects_absolute() {
        let err = validate_relative_path("/etc/passwd").expect_err("absolute must be rejected");
        match err {
            OutboxError::Io { message, .. } => {
                assert!(
                    message.contains("must be relative"),
                    "unexpected: {message}"
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn validate_relative_path_rejects_empty() {
        let err = validate_relative_path("").expect_err("empty must be rejected");
        match err {
            OutboxError::Io { message, .. } => {
                assert!(
                    message.contains("must not be empty"),
                    "unexpected: {message}"
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn validate_relative_path_rejects_curdir_only() {
        // `.` normalizes away to nothing, which is an empty target, not a file.
        validate_relative_path("./").expect_err("curdir-only must be rejected");
    }

    #[test]
    fn validate_relative_path_accepts_normal_note_path() {
        let normalized = validate_relative_path("inbox/notes/2026/05/16/note.md")
            .expect("normal store-relative path must be accepted");
        assert_eq!(normalized, PathBuf::from("inbox/notes/2026/05/16/note.md"));
    }

    #[test]
    fn validate_relative_path_strips_curdir_segments() {
        // Leading/interior `.` segments are noise, not traversal; they should be
        // dropped while the rest of the path is preserved.
        let normalized =
            validate_relative_path("./inbox/./note.md").expect("curdir segments are harmless");
        assert_eq!(normalized, PathBuf::from("inbox/note.md"));
    }

    /// Build an isolated temp store + outbox source for a `publish_payload` test.
    /// Returns `(store_root, source_path)` with `payload` written to the source.
    fn publish_fixture(tag: &str, payload: &[u8]) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "hm-outbox-publish-{tag}-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = fs::remove_dir_all(&base);
        let store_root = base.join("store");
        let source_dir = base.join("item");
        fs::create_dir_all(&store_root).expect("create store root");
        fs::create_dir_all(&source_dir).expect("create source dir");
        let source = source_dir.join("note.md");
        fs::write(&source, payload).expect("write source payload");
        (store_root, source)
    }

    /// A concurrent flusher that already published byte-identical content must not
    /// fail the loser: `write_atomic_create_new` returns `AlreadyExists`, and the
    /// content re-check must collapse the lost race into the idempotent
    /// already-present outcome so `cli::sync::perform` does not bail on `failed>0`.
    #[test]
    fn publish_payload_matching_existing_is_already_present_not_failed() {
        let payload = b"durable memory body";
        let (store_root, source) = publish_fixture("match", payload);
        let expected_hash = sha256(payload);
        let relative = "inbox/notes/2026/06/15/note.md";
        // The winner already published identical bytes at the destination.
        let final_path = store_root.join(relative);
        fs::create_dir_all(final_path.parent().expect("parent")).expect("create dest parent");
        fs::write(&final_path, payload).expect("pre-create winner output");

        let result = publish_payload(
            &source,
            &store_root,
            relative,
            &expected_hash,
            &write::AtomicWriteOptions::default(),
        )
        .expect("matching content must not error");
        assert_eq!(result, PublishResult::AlreadyPresent);
        let _ = fs::remove_dir_all(store_root.parent().expect("base"));
    }

    /// A destination that already exists with DIFFERENT content is a genuine
    /// conflict and must still be reported as such — the lost-race re-check must
    /// not weaken real different-content detection.
    #[test]
    fn publish_payload_different_existing_is_conflict() {
        let payload = b"intended body";
        let (store_root, source) = publish_fixture("conflict", payload);
        let expected_hash = sha256(payload);
        let relative = "inbox/notes/2026/06/15/note.md";
        let final_path = store_root.join(relative);
        fs::create_dir_all(final_path.parent().expect("parent")).expect("create dest parent");
        fs::write(&final_path, b"someone else's different bytes").expect("pre-create conflict");

        let result = publish_payload(
            &source,
            &store_root,
            relative,
            &expected_hash,
            &write::AtomicWriteOptions::default(),
        )
        .expect("conflict is a reported result, not a hard error");
        assert_eq!(result, PublishResult::Conflict);
        let _ = fs::remove_dir_all(store_root.parent().expect("base"));
    }

    /// The clean path (destination absent) must still publish and report
    /// `Written`, proving the race-recovery branch did not regress the common case.
    #[test]
    fn publish_payload_absent_destination_writes() {
        let payload = b"fresh body";
        let (store_root, source) = publish_fixture("fresh", payload);
        let expected_hash = sha256(payload);
        let relative = "inbox/notes/2026/06/15/note.md";

        let result = publish_payload(
            &source,
            &store_root,
            relative,
            &expected_hash,
            &write::AtomicWriteOptions::default(),
        )
        .expect("clean write must succeed");
        assert_eq!(result, PublishResult::Written);
        let written = fs::read(store_root.join(relative)).expect("destination written");
        assert_eq!(written, payload);
        let _ = fs::remove_dir_all(store_root.parent().expect("base"));
    }
}
