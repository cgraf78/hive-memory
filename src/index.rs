//! Rebuildable local triage index.
//!
//! The index is a cache, not canonical memory. It stores one compact JSON line
//! per inbox note so search/context can filter by metadata before reading note
//! bodies. If it is deleted or stale, it can always be rebuilt from notes and
//! paired JSON events in the store root.

use crate::{entity, note};
use crate::{event, path as memory_path, write};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt::{self, Display};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

// Bumped to 9 when the freshness fingerprint started folding canonical file
// count and max file mtime alongside directory mtime, AND the JSONL index began
// carrying its fingerprint in an embedded header line. Older sidecar-based
// caches (schema 8 and earlier, header-less) are treated as stale and rebuilt.
// Bumped to 10 when the fingerprint added an order-independent XOR of per-file
// name hashes, so a delete+add netting the same file count under an
// mtime-preserving cloud sync still invalidates the cache. Schema-9 caches are
// treated as stale and rebuilt cleanly on first read.
const INDEX_FINGERPRINT_SCHEMA_VERSION: u32 = 10;

/// Format version for the embedded index header line.
///
/// The header is the first physical line of `cache/indexes/<key>.jsonl`. It
/// publishes the freshness fingerprint INSIDE the data file so a single atomic
/// rename makes data and fingerprint visible together; a reader can no longer
/// validate a fingerprint against entries from a different rebuild run.
const INDEX_HEADER_FORMAT_VERSION: u32 = 1;

/// Cap on how many bytes a cached index file may be before a read declines it.
///
/// A torn or pathologically large synced file must not OOM the prompt hot path.
/// Real stores stay far under this: ~1 KiB per entry, so 256 MiB tolerates well
/// over 100k notes while still bounding a runaway/corrupt file. A file over the
/// cap is treated as a cache miss (rebuild), never a hard error.
const MAX_CACHED_INDEX_BYTES: u64 = 256 * 1024 * 1024;

/// One line in `cache/indexes/<store-alias>.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Shared note/event id.
    pub id: String,
    /// Stable store manifest id.
    pub store_id: String,
    /// Note entry kind.
    pub entry_kind: note::EntryKind,
    /// Memory scope.
    pub scope: String,
    /// Optional project identity.
    pub project_id: Option<String>,
    /// Explicit agent-private audience.
    pub audience: Vec<String>,
    /// Tags used for filtering.
    pub tags: Vec<String>,
    /// Optional subject.
    pub subject: Option<String>,
    /// Writer confidence.
    pub confidence: note::Confidence,
    /// Optional RFC3339 timestamp when this fact starts being valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    /// Optional RFC3339 timestamp when this fact stops being current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    /// Explicit records superseded by this entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    /// Optional explicit memory kind, used by inject classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<note::MemoryKind>,
    /// Canonical entity ids extracted from subject, tags, kind, and body.
    ///
    /// This is cache metadata, not canonical user-authored memory. Rebuilds can
    /// recompute it from durable note/event fields whenever extraction rules
    /// change, and search can use it as one ranking signal without reparsing
    /// Markdown on the hot hook path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<entity::EntityId>,
    /// Classification provenance; used to derive pending LLM review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classified: Option<note::ClassifiedBy>,
    /// Agent that wrote the record.
    pub agent_id: String,
    /// Host that wrote the record.
    ///
    /// Carried so diagnostics can report per-host last-seen activity from the
    /// index alone — the cheap way to notice that another machine's writes
    /// have stopped arriving through cloud sync. Empty only for entries from
    /// an older cache schema, which the fingerprint bump rebuilds.
    #[serde(default)]
    pub host_id: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
    /// Parsed note body cached for warm search.
    ///
    /// The Markdown note remains canonical. This field is populated only by
    /// index rebuilds and is safe to discard; changing the cache schema bumps
    /// the fingerprint so existing caches are rebuilt before search trusts it.
    #[serde(default)]
    pub body: String,
    /// Store-relative Markdown note path.
    pub note_path: String,
    /// Store-relative JSON event path when present.
    pub event_path: Option<String>,
}

/// Input for rebuilding one store's local index.
#[derive(Debug, Clone)]
pub struct RebuildIndexInput<'a> {
    /// Local store alias used as the index filename.
    pub store_name: &'a str,
    /// Canonical store root.
    pub store_root: &'a Path,
    /// Configured cache directory.
    pub cache_dir: &'a Path,
    /// Atomic writer options for publishing the JSONL file.
    pub options: write::AtomicWriteOptions,
    /// Case behavior for store-relative metadata paths.
    pub path_case: memory_path::PathCase,
}

/// Input for reading an index with lazy freshness validation.
#[derive(Debug, Clone)]
pub struct LoadIndexInput<'a> {
    /// Local store alias used as the index filename.
    pub store_name: &'a str,
    /// Canonical store root.
    pub store_root: &'a Path,
    /// Configured cache directory.
    pub cache_dir: &'a Path,
    /// Atomic writer options for publishing rebuilt cache files.
    pub options: write::AtomicWriteOptions,
    /// Case behavior for store-relative metadata paths.
    pub path_case: memory_path::PathCase,
}

/// Result of rebuilding one store index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildIndexReport {
    /// Final index file path.
    pub path: PathBuf,
    /// Entries written to the index.
    pub entries: Vec<IndexEntry>,
    /// Non-fatal parse warnings.
    pub warnings: Vec<IndexWarning>,
}

/// Result of loading or rebuilding one store index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadIndexReport {
    /// Final index file path.
    pub path: PathBuf,
    /// Entries read from or written to the index.
    pub entries: Vec<IndexEntry>,
    /// Non-fatal parse warnings from a rebuild.
    pub warnings: Vec<IndexWarning>,
    /// Whether the canonical store was parsed to refresh the cache.
    pub rebuilt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IndexFingerprint {
    // This is intentionally a cache schema, not a store schema. Bump it when
    // freshness semantics change so stale sidecars are rebuilt instead of being
    // silently trusted across releases.
    schema_version: u32,
    // Local cache directories can be shared by tests, alternate configs, and
    // renamed store aliases. Keep the root in the fingerprint as a second guard
    // against reading a same-named index from a different physical store.
    store_root: String,
    canonical_dirs: usize,
    latest_directory_modified_nanos: u128,
    // Canonical note/event FILE count and the newest file mtime are folded in
    // alongside directory mtime so adds/replaces that preserve a parent
    // directory's mtime — some cloud-sync arrivals land files with an older
    // mtime under an unchanged date dir — still invalidate the cache. Both are
    // computed during the same single walk that enumerates directories, so this
    // adds no extra directory traversal on the hot path (one extra `stat` per
    // file, which the OS dirent cache already warms during enumeration).
    canonical_files: usize,
    latest_file_modified_nanos: u128,
    // Order-independent combine (XOR) of a cheap hash of each canonical file's
    // store-relative path. File count + newest mtime miss the cloud-sync case
    // where a delete+add nets the SAME count and the added file's mtime is <=
    // the prior newest (mtime-preserving sync): both signals stay identical, so
    // the cache is served stale and the new note is invisible until `hm refresh`.
    // Folding the path-set membership in makes the fingerprint sensitive to WHICH
    // files exist, not just how many. The names are already in hand from
    // enumeration, so this adds no stat or file read — just a hash per dirent.
    canonical_names_combined: u64,
    entity_registry_modified_nanos: u128,
}

/// First physical line of a JSONL index file: the embedded freshness header.
///
/// Embedding the fingerprint in the data file is the atomicity fix: `rebuild_index`
/// publishes header + entries with one atomic rename, so a reader cannot pair a
/// "fresh" fingerprint with stale/partial entries from a different rebuild. The
/// distinctive `hm_index_format` key lets readers tell a header line apart from
/// an [`IndexEntry`] line (entries never carry that field).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IndexHeader {
    /// Header line format version; gates header parsing independent of the
    /// freshness schema so the two can evolve separately.
    hm_index_format: u32,
    /// Freshness fingerprint this index was built against.
    fingerprint: IndexFingerprint,
}

/// Non-fatal index rebuild warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexWarning {
    /// Path that caused the warning.
    pub path: PathBuf,
    /// Human-readable warning.
    pub message: String,
}

/// Index rebuild/read failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexError {
    /// Filesystem operation failed.
    Io {
        /// Operation that failed.
        action: &'static str,
        /// Path involved in the failure.
        path: PathBuf,
        /// Original error rendered for CLI diagnostics.
        message: String,
    },
    /// JSONL serialization or parsing failed.
    Json(String),
    /// Timestamp in note metadata was invalid.
    InvalidTimestamp {
        /// Path containing the bad timestamp.
        path: PathBuf,
        /// Invalid timestamp value.
        value: String,
    },
    /// Entity registry could not be loaded.
    EntityRegistry(String),
}

impl Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                message,
            } => write!(f, "failed to {action} {}: {message}", path.display()),
            Self::Json(message) => write!(f, "failed to process index JSON: {message}"),
            Self::InvalidTimestamp { path, value } => {
                write!(f, "invalid note timestamp in {}: {value}", path.display())
            }
            Self::EntityRegistry(message) => write!(f, "{message}"),
        }
    }
}

impl Error for IndexError {}

/// Held advisory lock serializing rebuilds of one store's cache artifact.
///
/// The cache artifact (JSONL + Tantivy dir) is SHARED across agents and
/// sessions, so the rebuild lock is keyed by the store CACHE KEY rather than by
/// agent/session identity — that is the wrong granularity for a shared file. It
/// is a host-local `flock`, not a distributed lock: it only serializes rebuilds
/// on this machine; another host rebuilds its own synced copy independently.
///
/// The lock lives under `cache_dir/locks/index/`, never inside a synced store,
/// and uses a different path namespace from the hook session refresh lock
/// (`state_dir/locks/refresh/...`), so the two locks can never nest into a
/// deadlock even when `hm refresh` holds both at once.
#[derive(Debug)]
pub struct RebuildLock {
    file: File,
    /// Lock file path, exposed for diagnostics and tests.
    pub path: PathBuf,
}

impl Drop for RebuildLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Return the rebuild lock path for one store cache key under `cache_dir`.
fn rebuild_lock_path(cache_dir: &Path, store_name: &str, store_root: &Path) -> PathBuf {
    cache_dir
        .join("locks")
        .join("index")
        .join(format!("{}.lock", store_cache_key(store_name, store_root)))
}

/// Try to acquire the non-blocking rebuild lock for one store cache key.
///
/// `Ok(None)` means another rebuild is already running on this host for the same
/// cache artifact; callers fall back gracefully (re-check freshness, then
/// rebuild lock-free) rather than block a latency-sensitive read. Any other I/O
/// error surfaces so the caller knows local coordination state is untrustworthy.
pub fn try_rebuild_lock(
    cache_dir: &Path,
    store_name: &str,
    store_root: &Path,
) -> Result<Option<RebuildLock>, IndexError> {
    let path = rebuild_lock_path(cache_dir, store_name, store_root);
    let Some(parent) = path.parent() else {
        return Err(IndexError::Io {
            action: "create rebuild lock parent",
            path,
            message: "lock path has no parent".to_owned(),
        });
    };
    fs::create_dir_all(parent)
        .map_err(|err| io_error("create rebuild lock parent", parent, err))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|err| io_error("open rebuild lock", &path, err))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(RebuildLock { file, path })),
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(err) => Err(io_error("lock rebuild", &path, err)),
    }
}

/// Read a fresh index when possible, otherwise rebuild it from canonical files.
///
/// Context and search run on latency-sensitive hook paths. They should pay for
/// full Markdown/event parsing only when canonical inbox files changed; the
/// embedded-header fingerprint uses directory + file metadata so hot reads do
/// not stat every note for content. That catches create/delete/rename/replace
/// changes cheaply; content-only manual edits rely on explicit `hm refresh`,
/// which is the same maintenance path hooks already run after writes.
///
/// Concurrent rebuilds of the same store are serialized by a cache-key-scoped
/// advisory lock so two sessions cannot redundantly scan the store or fight over
/// the Tantivy writer. If the lock is already held, another rebuild just ran, so
/// we re-check freshness once before falling back to a lock-free rebuild (the
/// atomic header publish keeps that safe).
pub fn load_or_rebuild_index(input: LoadIndexInput<'_>) -> Result<LoadIndexReport, IndexError> {
    if let Some(report) = load_fresh_index(&input)? {
        return Ok(report);
    }

    let _lock = match try_rebuild_lock(input.cache_dir, input.store_name, input.store_root)? {
        Some(lock) => Some(lock),
        None => {
            // Another rebuild is in flight. It may have just published a fresh
            // index; re-check before doing redundant work.
            if let Some(report) = load_fresh_index(&input)? {
                return Ok(report);
            }
            None
        }
    };

    let report = rebuild_index(RebuildIndexInput {
        store_name: input.store_name,
        store_root: input.store_root,
        cache_dir: input.cache_dir,
        options: input.options,
        path_case: input.path_case,
    })?;
    Ok(LoadIndexReport {
        path: report.path,
        entries: report.entries,
        warnings: report.warnings,
        rebuilt: true,
    })
}

/// Read a fresh cached index without rebuilding stale or missing cache files.
///
/// Callers that need freshness but cannot safely rebuild can use this contract
/// to degrade on a cache miss instead of turning a latency-sensitive boundary
/// into a full store scan.
pub fn load_fresh_index(input: &LoadIndexInput<'_>) -> Result<Option<LoadIndexReport>, IndexError> {
    let path = scoped_index_path(input.cache_dir, input.store_name, input.store_root);
    let current = canonical_fingerprint(input.store_root)?;
    // The fingerprint now lives in the index file's header, so one read both
    // proves freshness and yields entries — and the fingerprint can never refer
    // to a different rebuild's data. A missing/old-format header, a stale
    // fingerprint, an oversized file, or any read/parse error is a cache miss
    // (rebuild), never a hard error on this latency-sensitive boundary.
    if let Ok(Some((cached, entries))) = read_index_with_fingerprint(&path)
        && cached == current
    {
        return Ok(Some(LoadIndexReport {
            path,
            entries,
            warnings: Vec::new(),
            rebuilt: false,
        }));
    }

    Ok(None)
}

/// Read a locally cached index without touching the canonical store root.
///
/// This is intentionally weaker than [`load_fresh_index`]. Prompt-submit hooks
/// run inside short agent timeouts, and the canonical store may live on a
/// cloud-backed FUSE mount that takes seconds to wake after idle. For that
/// boundary, stale-but-local recall is more useful than blocking the prompt.
/// Refresh and normal read commands still own freshness validation.
pub fn load_cached_index(
    input: &LoadIndexInput<'_>,
) -> Result<Option<LoadIndexReport>, IndexError> {
    let path = scoped_index_path(input.cache_dir, input.store_name, input.store_root);
    // Prompt-submit is the most latency-sensitive boundary and must degrade, not
    // fail. A torn write or a partially synced line previously made the whole
    // read return `Err` and bubbled up as a hard error; now any unreadable
    // header (missing, old-format, oversized, or corrupt) is simply a cache miss
    // that callers turn into a rebuild. Per-line corruption inside the body is
    // already tolerated by `read_index_with_fingerprint` (skip-and-warn).
    let Ok(Some((cached, entries))) = read_index_with_fingerprint(&path) else {
        return Ok(None);
    };
    if cached.schema_version != INDEX_FINGERPRINT_SCHEMA_VERSION
        || cached.store_root != input.store_root.display().to_string()
    {
        return Ok(None);
    }

    Ok(Some(LoadIndexReport {
        path,
        entries,
        warnings: Vec::new(),
        rebuilt: false,
    }))
}

/// Rebuild one store's JSONL triage index from canonical inbox files.
///
/// Rebuilds are intentionally deterministic: note paths are sorted before
/// parsing, and entries are written in path order. Paired event metadata wins
/// when it parses cleanly; malformed events warn and fall back to note metadata
/// so one bad sidecar does not make the whole index unusable.
pub fn rebuild_index(input: RebuildIndexInput<'_>) -> Result<RebuildIndexReport, IndexError> {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    let registry = entity::EntityRegistry::load_for_store(input.store_root)
        .map_err(|err| IndexError::EntityRegistry(err.to_string()))?;
    let notes_root = input.store_root.join("inbox/notes");
    for note_path in note_paths(&notes_root)? {
        let relative_note_path = relative_path(input.store_root, &note_path, input.path_case);
        let contents = match fs::read_to_string(&note_path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                warnings.push(IndexWarning {
                    path: note_path,
                    message: "note disappeared during index rebuild".to_owned(),
                });
                continue;
            }
            Err(err) => return Err(io_error("read note", &note_path, err)),
        };
        let parsed = match note::parse_note(&contents) {
            Ok(note) => note,
            Err(err) => {
                warnings.push(IndexWarning {
                    path: note_path,
                    message: err.to_string(),
                });
                continue;
            }
        };

        let created_at = parse_rfc3339(&relative_note_path, &parsed.front_matter.created_at)?;
        let expected_event_path = input.store_root.join(event::event_relative_path(
            &parsed.front_matter.id,
            created_at,
        ));
        let relative_event_path =
            relative_path(input.store_root, &expected_event_path, input.path_case);
        let event = read_paired_event(&expected_event_path, &parsed.front_matter.id, &mut warnings);
        entries.push(entry_from_note(
            &parsed.front_matter,
            &parsed.body,
            &relative_note_path,
            event.as_ref().map(|_| relative_event_path.as_str()),
            event.as_ref(),
            &registry,
        ));
    }

    let path = scoped_index_path(input.cache_dir, input.store_name, input.store_root);
    // Compute the fingerprint BEFORE serializing so it is published in the same
    // atomic rename as the entries it describes. This closes the two-file race:
    // a reader can no longer see a fresh fingerprint paired with stale/partial
    // data, because there is exactly one file and one rename.
    let fingerprint = canonical_fingerprint(input.store_root)?;
    let jsonl = render_jsonl(&fingerprint, &entries)?;
    write::write_atomic(&path, jsonl.as_bytes(), &input.options).map_err(|err| IndexError::Io {
        action: "write index",
        path: path.clone(),
        message: err.to_string(),
    })?;

    Ok(RebuildIndexReport {
        path,
        entries,
        warnings,
    })
}

/// Read an existing JSONL index file, returning only its entries.
///
/// The index is a rebuildable cache, never a security boundary, so a single torn
/// or partially synced line must degrade — not abort the whole read. A leading
/// embedded fingerprint header is skipped, and any malformed body line is
/// skipped with a stderr warning (mirroring how `rebuild_index` tolerates one
/// bad note) so the remaining good entries are still returned. The file size is
/// capped so a pathological synced file cannot OOM the hot path; an oversized
/// file yields an `Io` error that callers treat as a cache miss.
pub fn read_index(path: &Path) -> Result<Vec<IndexEntry>, IndexError> {
    Ok(read_index_inner(path)?.1)
}

/// Read an index file as `(fingerprint, entries)` when it carries a valid
/// embedded header, or `Ok(None)` for an old/header-less or unreadable file.
///
/// Returning `None` (rather than an error) for the old two-file format is what
/// lets freshness checks treat a legacy index as stale and rebuild it without
/// panicking. Genuine I/O failures (including the size cap) still surface as
/// errors so the caller can decide; on the hot path that decision is a miss.
fn read_index_with_fingerprint(
    path: &Path,
) -> Result<Option<(IndexFingerprint, Vec<IndexEntry>)>, IndexError> {
    let (header, entries) = read_index_inner(path)?;
    Ok(header.map(|header| (header.fingerprint, entries)))
}

/// Shared index reader: parses an optional leading header and the entry body,
/// skipping malformed body lines with a warning and enforcing the size cap.
fn read_index_inner(path: &Path) -> Result<(Option<IndexHeader>, Vec<IndexEntry>), IndexError> {
    // Bound the read before slurping the file: a torn/runaway synced file must
    // not be loaded wholesale into memory. `metadata` is cheap relative to the
    // read it guards.
    if let Ok(metadata) = fs::metadata(path)
        && metadata.len() > MAX_CACHED_INDEX_BYTES
    {
        return Err(IndexError::Io {
            action: "read index",
            path: path.to_path_buf(),
            message: format!(
                "index file is {} bytes, over the {MAX_CACHED_INDEX_BYTES}-byte cache cap",
                metadata.len()
            ),
        });
    }
    let contents = fs::read_to_string(path).map_err(|err| io_error("read index", path, err))?;
    let mut lines = contents.lines().filter(|line| !line.trim().is_empty());

    // The header, when present, is always the first non-empty line. Peek it
    // without consuming a real entry: an entry line never parses as a header
    // (entries lack `hm_index_format`), and an old-format first line is an
    // entry that we must still keep.
    let mut entries = Vec::new();
    let mut header = None;
    if let Some(first) = lines.next() {
        match serde_json::from_str::<IndexHeader>(first) {
            Ok(parsed) if parsed.hm_index_format == INDEX_HEADER_FORMAT_VERSION => {
                header = Some(parsed);
            }
            // Not a header (old format or future header version): treat as a
            // body line so legacy header-less indexes are still readable.
            _ => push_entry_line(first, path, &mut entries),
        }
    }
    for line in lines {
        push_entry_line(line, path, &mut entries);
    }
    Ok((header, entries))
}

/// Parse one JSONL entry line, skipping it with a warning when it is malformed.
fn push_entry_line(line: &str, path: &Path, entries: &mut Vec<IndexEntry>) {
    match serde_json::from_str::<IndexEntry>(line) {
        Ok(entry) => entries.push(entry),
        Err(err) => eprintln!(
            "warning: {}: skipping malformed index line: {err}",
            path.display()
        ),
    }
}

/// Return the cache path for one store alias.
pub fn index_path(cache_dir: &Path, store_name: &str) -> PathBuf {
    cache_dir
        .join("indexes")
        .join(format!("{store_name}.jsonl"))
}

/// Return the scoped cache path for one store alias and root.
///
/// The root is part of the cache key so changing a configured store root cannot
/// accidentally reuse a stale index from an older location with the same alias.
pub fn scoped_index_path(cache_dir: &Path, store_name: &str, store_root: &Path) -> PathBuf {
    cache_dir
        .join("indexes")
        .join(format!("{}.jsonl", store_cache_key(store_name, store_root)))
}

/// Return the stable cache key for one store alias + root.
///
/// Store aliases are local labels, not durable identities, so the root spelling
/// is hashed into the key. This is the same key the JSONL filename is built
/// from, which makes it the right granularity for the rebuild lock: every host
/// process touching one store's cache artifact converges on one lock, regardless
/// of which agent or session triggered the rebuild.
pub fn store_cache_key(store_name: &str, store_root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(store_root.display().to_string().as_bytes());
    let digest = hasher.finalize();
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{store_name}-{suffix}")
}

fn read_paired_event(
    path: &Path,
    expected_id: &str,
    warnings: &mut Vec<IndexWarning>,
) -> Option<event::MemoryEvent> {
    if !path.is_file() {
        return None;
    }

    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) => {
            warnings.push(IndexWarning {
                path: path.to_path_buf(),
                message: err.to_string(),
            });
            return None;
        }
    };

    match event::parse_event(&contents) {
        Ok(event) if event.id == expected_id => Some(event),
        Ok(event) => {
            warnings.push(IndexWarning {
                path: path.to_path_buf(),
                message: format!(
                    "event id {} does not match paired note id {}",
                    event.id, expected_id
                ),
            });
            None
        }
        Err(err) => {
            warnings.push(IndexWarning {
                path: path.to_path_buf(),
                message: err.to_string(),
            });
            None
        }
    }
}

fn entry_from_note(
    front_matter: &note::NoteFrontMatter,
    body: &str,
    note_path: &str,
    event_path: Option<&str>,
    event: Option<&event::MemoryEvent>,
    registry: &entity::EntityRegistry,
) -> IndexEntry {
    if let Some(event) = event {
        // Event sidecars are the structured machine contract. The Markdown note
        // remains the human-readable canonical record, but paired event fields
        // are preferred for filters so future migrations/repairs can update
        // machine metadata without rewriting user prose.
        let entities = entry_entities(
            event.subject.as_deref().or(front_matter.subject.as_deref()),
            &event.tags,
            event.kind.or(front_matter.kind),
            body,
            registry,
        );
        return IndexEntry {
            id: event.id.clone(),
            store_id: event.store_id.clone(),
            entry_kind: front_matter.entry_kind,
            scope: event.scope.clone(),
            project_id: event.project_id.clone(),
            audience: event.audience.clone(),
            tags: event.tags.clone(),
            subject: event.subject.clone(),
            confidence: event.confidence,
            valid_from: event
                .valid_from
                .clone()
                .or_else(|| front_matter.valid_from.clone()),
            valid_to: event
                .valid_to
                .clone()
                .or_else(|| front_matter.valid_to.clone()),
            supersedes: if event.supersedes.is_empty() {
                front_matter.supersedes.clone()
            } else {
                event.supersedes.clone()
            },
            // Prefer the event's kind, falling back to the note's so a note that
            // carries kind without an event copy is still classified correctly.
            kind: event.kind.or(front_matter.kind),
            entities,
            // Provenance follows the same machine-metadata preference as kind:
            // event sidecars are the structured contract, with note front
            // matter as the repair/fallback source for note-only history.
            classified: event
                .classified
                .clone()
                .or_else(|| front_matter.classified.clone()),
            agent_id: event.agent_id.clone(),
            host_id: event.host_id.clone(),
            created_at: event.created_at.clone(),
            body: body.to_owned(),
            note_path: note_path.to_owned(),
            event_path: event_path.map(str::to_owned),
        };
    }

    let entities = entry_entities(
        front_matter.subject.as_deref(),
        &front_matter.tags,
        front_matter.kind,
        body,
        registry,
    );
    IndexEntry {
        id: front_matter.id.clone(),
        store_id: front_matter.store_id.clone(),
        entry_kind: front_matter.entry_kind,
        scope: front_matter.scope.clone(),
        project_id: front_matter.project_id.clone(),
        audience: front_matter.audience.clone(),
        tags: front_matter.tags.clone(),
        subject: front_matter.subject.clone(),
        confidence: front_matter.confidence,
        valid_from: front_matter.valid_from.clone(),
        valid_to: front_matter.valid_to.clone(),
        supersedes: front_matter.supersedes.clone(),
        kind: front_matter.kind,
        entities,
        classified: front_matter.classified.clone(),
        agent_id: front_matter.agent_id.clone(),
        host_id: front_matter.host_id.clone(),
        created_at: front_matter.created_at.clone(),
        body: body.to_owned(),
        note_path: note_path.to_owned(),
        event_path: None,
    }
}

fn entry_entities(
    subject: Option<&str>,
    tags: &[String],
    kind: Option<note::MemoryKind>,
    body: &str,
    registry: &entity::EntityRegistry,
) -> Vec<entity::EntityId> {
    let kind_label = kind.map(note::kind_label);
    entity::extract_fields_with_registry(
        subject
            .into_iter()
            .chain(tags.iter().map(String::as_str))
            .chain(kind_label)
            .chain(std::iter::once(body)),
        registry,
    )
}

fn render_jsonl(
    fingerprint: &IndexFingerprint,
    entries: &[IndexEntry],
) -> Result<String, IndexError> {
    let mut output = String::new();
    let header = IndexHeader {
        hm_index_format: INDEX_HEADER_FORMAT_VERSION,
        fingerprint: fingerprint.clone(),
    };
    let header_line =
        serde_json::to_string(&header).map_err(|err| IndexError::Json(err.to_string()))?;
    output.push_str(&header_line);
    output.push('\n');
    for entry in entries {
        let line = serde_json::to_string(entry).map_err(|err| IndexError::Json(err.to_string()))?;
        output.push_str(&line);
        output.push('\n');
    }
    Ok(output)
}

fn note_paths(root: &Path) -> Result<Vec<PathBuf>, IndexError> {
    let mut paths = Vec::new();
    if !root.is_dir() {
        return Ok(paths);
    }
    collect_note_paths(root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn collect_note_paths(root: &Path, paths: &mut Vec<PathBuf>) -> Result<(), IndexError> {
    for entry in fs::read_dir(root).map_err(|err| io_error("read note directory", root, err))? {
        let entry = entry.map_err(|err| io_error("read note directory", root, err))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| io_error("read note file type", &path, err))?;
        if file_type.is_dir() {
            collect_note_paths(&path, paths)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            paths.push(path);
        }
    }
    Ok(())
}

/// Running totals for the single canonical-files walk.
///
/// Folding file count and max file mtime in here keeps the freshness signal to
/// ONE directory traversal: directory mtime catches most create/delete/rename
/// changes cheaply, and the file count + newest file mtime catch in-place
/// replaces and the cloud-sync case where a file lands under a date dir whose
/// own mtime is preserved. The XOR-combined name hash additionally catches a
/// delete+add that nets the same count under an mtime-preserving sync, where
/// neither count nor newest mtime would move. We deliberately stop short of
/// per-file content hashing, which would re-read every note on the hot path.
#[derive(Default)]
struct CanonicalScan {
    dirs: usize,
    files: usize,
    latest_directory_modified_nanos: u128,
    latest_file_modified_nanos: u128,
    // Running XOR of each file's name hash. XOR is order-independent, so the
    // value depends on the file SET and not on enumeration order; a delete+add
    // that swaps one file for a differently-named one flips it.
    names_combined: u64,
}

fn canonical_fingerprint(store_root: &Path) -> Result<IndexFingerprint, IndexError> {
    let mut scan = CanonicalScan::default();
    collect_canonical(&store_root.join("inbox/notes"), &mut scan)?;
    collect_canonical(&store_root.join("inbox/events"), &mut scan)?;
    Ok(IndexFingerprint {
        // v8: entity extraction includes deterministic quoted/proper-name
        // phrase links in addition to built-in aliases and the store registry.
        // v9: folds canonical file count + newest file mtime into freshness and
        // moves the fingerprint into the index header for atomic publish.
        // v10: folds an order-independent XOR of per-file name hashes so a
        // delete+add netting the same count under an mtime-preserving sync still
        // invalidates the cache (file-SET membership, not just file count).
        schema_version: INDEX_FINGERPRINT_SCHEMA_VERSION,
        store_root: store_root.display().to_string(),
        canonical_dirs: scan.dirs,
        latest_directory_modified_nanos: scan.latest_directory_modified_nanos,
        canonical_files: scan.files,
        latest_file_modified_nanos: scan.latest_file_modified_nanos,
        canonical_names_combined: scan.names_combined,
        entity_registry_modified_nanos: optional_modified_nanos(&store_root.join("entities.toml"))?,
    })
}

fn optional_modified_nanos(path: &Path) -> Result<u128, IndexError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(modified_nanos(metadata.modified().map_err(|err| {
            io_error("read entity registry modified time", path, err)
        })?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(io_error("read entity registry metadata", path, err)),
    }
}

fn collect_canonical(root: &Path, scan: &mut CanonicalScan) -> Result<(), IndexError> {
    if !root.is_dir() {
        return Ok(());
    }
    scan.dirs += 1;
    let metadata = fs::metadata(root)
        .map_err(|err| io_error("read canonical directory metadata", root, err))?;
    scan.latest_directory_modified_nanos =
        scan.latest_directory_modified_nanos.max(modified_nanos(
            metadata
                .modified()
                .map_err(|err| io_error("read canonical directory modified time", root, err))?,
        ));
    for entry in
        fs::read_dir(root).map_err(|err| io_error("read canonical directory", root, err))?
    {
        let entry = entry.map_err(|err| io_error("read canonical directory", root, err))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| io_error("read canonical file type", &path, err))?;
        if file_type.is_dir() {
            collect_canonical(&path, scan)?;
        } else if file_type.is_file() {
            scan.files += 1;
            // Fold the file's identity (its name) into the set membership signal.
            // The name is already in hand from enumeration, so this is just a
            // hash — no stat, no read. XOR keeps the combine order-independent.
            scan.names_combined ^= name_hash(&entry.file_name());
            // One extra stat per file. The dirent is already warm from
            // enumeration, so this stays cheap relative to a full note re-read,
            // and it is what lets an mtime-preserving cloud-sync arrival be seen.
            let file_metadata = fs::metadata(&path)
                .map_err(|err| io_error("read canonical file metadata", &path, err))?;
            scan.latest_file_modified_nanos = scan.latest_file_modified_nanos.max(modified_nanos(
                file_metadata
                    .modified()
                    .map_err(|err| io_error("read canonical file modified time", &path, err))?,
            ));
        }
    }
    Ok(())
}

fn modified_nanos(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

/// Cheap, stable 64-bit hash of a canonical file's name (FNV-1a over the raw
/// `OsStr` bytes via its lossy UTF-8 view). Used only to give the freshness
/// fingerprint sensitivity to file-set membership; it never needs to be
/// cryptographic, just well-distributed and deterministic across runs so a
/// delete+add of differently-named files flips the XOR combine. Note names are
/// ULID-stamped (`note-<ulid>.md`), so even content-identical notes hash apart.
fn name_hash(name: &std::ffi::OsStr) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in name.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn relative_path(root: &Path, path: &Path, path_case: memory_path::PathCase) -> String {
    memory_path::relative_string(path.strip_prefix(root).unwrap_or(path), path_case)
}

fn parse_rfc3339(path: &str, value: &str) -> Result<OffsetDateTime, IndexError> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).map_err(|_| {
        IndexError::InvalidTimestamp {
            path: PathBuf::from(path),
            value: value.to_owned(),
        }
    })
}

fn io_error(action: &'static str, path: &Path, err: std::io::Error) -> IndexError {
    IndexError::Io {
        action,
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Sensitivity;
    use crate::memory;
    use crate::store::StoreManifest;
    use crate::write::{AtomicWriteOptions, FsyncPolicy};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hive-memory-index-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn timestamp() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_778_946_153)
            .expect("timestamp")
            .replace_nanosecond(184_921_000)
            .expect("nanos")
    }

    fn manifest() -> StoreManifest {
        StoreManifest::with_identity(
            "personal",
            Some("Personal memory".to_owned()),
            Sensitivity::Private,
            "018f5f57-bd9b-7d33-9e21-1f44f0c5a013".to_owned(),
            "2026-05-16T00:00:00Z".to_owned(),
        )
    }

    fn options() -> AtomicWriteOptions {
        AtomicWriteOptions {
            fsync: FsyncPolicy::Never,
            ..AtomicWriteOptions::default()
        }
    }

    fn write_record(root: &Path, write_event: bool) -> memory::WriteRecordResult {
        memory::write_record(memory::WriteRecordInput {
            root,
            manifest: &manifest(),
            entry_kind: note::EntryKind::Remember,
            created_at: timestamp(),
            agent_id: "codex".to_owned(),
            host_id: "taylor".to_owned(),
            user_id: "chris".to_owned(),
            session_id: Some("abc123".to_owned()),
            scope: "global".to_owned(),
            confidence: note::Confidence::High,
            body: "Chris prefers TOML config.".to_owned(),
            project_id: Some("github-com-cgraf78-hive-memory-018f5f57".to_owned()),
            subject: Some("workflow.preference".to_owned()),
            kind: None,
            valid_from: None,
            valid_to: None,
            supersedes: Vec::new(),
            tags: vec!["preference".to_owned(), "config".to_owned()],
            audience: Vec::new(),
            source_kind: Some("session".to_owned()),
            source_ref: Some("abc123".to_owned()),
            write_event,
            options: options(),
        })
        .expect("write memory record")
    }

    #[test]
    fn rebuilds_jsonl_index_from_notes_and_events() {
        let dir = temp_dir("rebuild");
        let root = dir.join("store");
        let cache = dir.join("cache");
        let written = write_record(&root, true);

        let report = rebuild_index(RebuildIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        })
        .expect("rebuild index");

        assert_eq!(report.entries.len(), 1);
        assert!(report.warnings.is_empty());
        assert_eq!(report.path, scoped_index_path(&cache, "personal", &root));
        assert_eq!(
            report.entries[0].note_path,
            relative_path(&root, &written.note_path, memory_path::PathCase::Sensitive)
        );
        assert_eq!(report.entries[0].body, "Chris prefers TOML config.");
        // Host identity rides along so diagnostics (sync-status) can report
        // per-host last-seen activity from the index alone.
        assert_eq!(report.entries[0].host_id, "taylor");
        assert_eq!(
            report.entries[0].event_path.as_deref(),
            Some(
                relative_path(
                    &root,
                    &written.event_path.expect("event"),
                    memory_path::PathCase::Sensitive
                )
                .as_str()
            )
        );
        let read = read_index(&report.path).expect("read index");
        assert_eq!(read, report.entries);
    }

    #[test]
    fn malformed_event_warns_and_uses_note_metadata() {
        let dir = temp_dir("bad-event");
        let root = dir.join("store");
        let cache = dir.join("cache");
        let written = write_record(&root, true);
        fs::write(written.event_path.expect("event"), "{bad json").expect("corrupt event");

        let report = rebuild_index(RebuildIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        })
        .expect("rebuild index");

        assert_eq!(report.entries.len(), 1);
        assert_eq!(
            report.entries[0].subject.as_deref(),
            Some("workflow.preference")
        );
        assert!(report.entries[0].event_path.is_none());
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn index_lowercases_paths_when_store_is_case_insensitive() {
        let dir = temp_dir("case-insensitive-paths");
        let root = dir.join("store");
        let cache = dir.join("cache");
        let written = write_record(&root, false);
        let renamed = written.note_path.with_file_name("RENAMED.md");
        fs::rename(&written.note_path, &renamed).expect("rename note");

        let report = rebuild_index(RebuildIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Insensitive,
        })
        .expect("rebuild index");

        assert_eq!(report.entries.len(), 1);
        assert!(report.entries[0].note_path.ends_with("/renamed.md"));
    }

    #[test]
    fn load_or_rebuild_reuses_fresh_index() {
        let dir = temp_dir("load-fresh");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(&root, true);

        let first = load_or_rebuild_index(LoadIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        })
        .expect("first load");
        let second = load_or_rebuild_index(LoadIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        })
        .expect("second load");

        assert!(first.rebuilt);
        assert!(!second.rebuilt);
        assert_eq!(second.entries, first.entries);
    }

    #[test]
    fn load_or_rebuild_refreshes_after_canonical_change() {
        let dir = temp_dir("load-stale");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(&root, false);
        let first = load_or_rebuild_index(LoadIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        })
        .expect("first load");
        write_record(&root, false);

        let second = load_or_rebuild_index(LoadIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        })
        .expect("second load");

        assert!(first.rebuilt);
        assert!(second.rebuilt);
        assert_eq!(second.entries.len(), 2);
    }

    #[test]
    fn load_or_rebuild_repairs_corrupt_cached_index() {
        let dir = temp_dir("load-corrupt");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(&root, true);
        let first = load_or_rebuild_index(LoadIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        })
        .expect("first load");
        fs::write(&first.path, "{bad json").expect("corrupt index");

        let second = load_or_rebuild_index(LoadIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        })
        .expect("second load");

        assert!(second.rebuilt);
        assert_eq!(second.entries.len(), 1);
    }

    #[test]
    fn fingerprint_includes_store_root_identity() {
        let dir = temp_dir("fingerprint-root");
        let first = dir.join("first");
        let second = dir.join("second");
        fs::create_dir_all(first.join("inbox/notes")).expect("first notes");
        fs::create_dir_all(first.join("inbox/events")).expect("first events");
        fs::create_dir_all(second.join("inbox/notes")).expect("second notes");
        fs::create_dir_all(second.join("inbox/events")).expect("second events");

        let first = canonical_fingerprint(&first).expect("first fingerprint");
        let second = canonical_fingerprint(&second).expect("second fingerprint");

        assert_ne!(first, second);
    }

    #[test]
    fn malformed_note_warns_and_skips_entry() {
        let dir = temp_dir("bad-note");
        let root = dir.join("store");
        let cache = dir.join("cache");
        let note_dir = root.join("inbox/notes/2026/05/16");
        fs::create_dir_all(&note_dir).expect("create note dir");
        fs::write(note_dir.join("bad.md"), "not front matter").expect("write bad note");

        let report = rebuild_index(RebuildIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        })
        .expect("rebuild index");

        assert!(report.entries.is_empty());
        assert_eq!(report.warnings.len(), 1);
    }

    fn load_input<'a>(store_name: &'a str, root: &'a Path, cache: &'a Path) -> LoadIndexInput<'a> {
        LoadIndexInput {
            store_name,
            store_root: root,
            cache_dir: cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        }
    }

    /// A single torn JSONL body line (a partial sync / interrupted write) must
    /// not make `load_cached_index` return `Err` on the prompt hot path. The
    /// header still validates, so the cache is reused and the good entry survives
    /// while the corrupt line is dropped.
    #[test]
    fn load_cached_index_tolerates_corrupt_body_line() {
        let dir = temp_dir("cached-corrupt-line");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(&root, true);
        let report = rebuild_index(RebuildIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        })
        .expect("rebuild");

        let contents = fs::read_to_string(&report.path).expect("read index");
        // Append a torn line after the valid header + entry.
        fs::write(&report.path, format!("{contents}{{partial line\n")).expect("corrupt body");

        let loaded = load_cached_index(&load_input("personal", &root, &cache))
            .expect("load cached must not error on a torn line");
        let loaded = loaded.expect("header still valid, cache reused");
        assert_eq!(loaded.entries.len(), 1);
    }

    /// `read_index` skips one malformed body line and returns the rest rather
    /// than aborting the whole index on a single torn write.
    #[test]
    fn read_index_skips_bad_line_returns_rest() {
        let dir = temp_dir("read-skip-line");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(&root, true);
        let report = rebuild_index(RebuildIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        })
        .expect("rebuild");
        let contents = fs::read_to_string(&report.path).expect("read index");
        fs::write(&report.path, format!("{contents}{{garbled\n")).expect("append bad line");

        let entries = read_index(&report.path).expect("read index skips bad line");
        assert_eq!(entries.len(), 1);
    }

    /// An index file over the size cap is declined as a cache miss, never loaded
    /// into memory and never a hard error to the caller.
    #[test]
    fn read_index_rejects_oversized_file() {
        let dir = temp_dir("read-oversized");
        let path = dir.join("huge.jsonl");
        // Sparse file: set a length past the cap without writing the bytes.
        let file = File::create(&path).expect("create");
        file.set_len(MAX_CACHED_INDEX_BYTES + 1).expect("grow");
        drop(file);

        let err = read_index(&path).expect_err("oversized file is an error");
        assert!(matches!(err, IndexError::Io { .. }));
    }

    /// The fingerprint travels inside the index header, so one read recovers both
    /// the freshness fingerprint and the entries that were published with it.
    #[test]
    fn embedded_fingerprint_round_trips_atomically() {
        let dir = temp_dir("embedded-fingerprint");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(&root, true);
        let report = rebuild_index(RebuildIndexInput {
            store_name: "personal",
            store_root: &root,
            cache_dir: &cache,
            options: options(),
            path_case: memory_path::PathCase::Sensitive,
        })
        .expect("rebuild");

        let (fingerprint, entries) = read_index_with_fingerprint(&report.path)
            .expect("read")
            .expect("header present");
        assert_eq!(
            fingerprint,
            canonical_fingerprint(&root).expect("fingerprint")
        );
        assert_eq!(entries, report.entries);
    }

    /// An old-format, header-less index (the pre-9 two-file layout) must be
    /// treated as stale and rebuilt, never trusted and never a panic.
    #[test]
    fn old_format_index_triggers_rebuild() {
        let dir = temp_dir("old-format");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(&root, true);
        let path = scoped_index_path(&cache, "personal", &root);
        fs::create_dir_all(path.parent().expect("parent")).expect("mk cache");
        // Simulate an old index: entry lines only, no embedded header.
        let entry_line = serde_json::to_string(
            &rebuild_index(RebuildIndexInput {
                store_name: "personal",
                store_root: &root,
                cache_dir: &cache,
                options: options(),
                path_case: memory_path::PathCase::Sensitive,
            })
            .expect("rebuild")
            .entries[0],
        )
        .expect("serialize entry");
        fs::write(&path, format!("{entry_line}\n")).expect("write old-format index");

        // Header-less file yields no fingerprint, so it is a cache miss.
        assert!(read_index_with_fingerprint(&path).expect("read").is_none());
        let loaded =
            load_cached_index(&load_input("personal", &root, &cache)).expect("load cached");
        assert!(loaded.is_none(), "old format must miss, not be trusted");

        // And the lazy path rebuilds cleanly into the new format.
        let rebuilt =
            load_or_rebuild_index(load_input("personal", &root, &cache)).expect("rebuild");
        assert!(rebuilt.rebuilt);
        assert!(
            read_index_with_fingerprint(&rebuilt.path)
                .expect("read")
                .is_some()
        );
    }

    /// Adding a new note file into an existing date directory changes the
    /// fingerprint and bumps the canonical file count. The file count + newest
    /// file mtime are the signals that catch an mtime-preserving cloud-sync
    /// arrival, which a directory-mtime-only fingerprint can miss.
    #[test]
    fn freshness_detects_new_file_in_existing_dir() {
        let dir = temp_dir("freshness-new-file");
        let root = dir.join("store");
        let note_dir = root.join("inbox/notes/2026/05/16");
        fs::create_dir_all(&note_dir).expect("note dir");
        fs::create_dir_all(root.join("inbox/events")).expect("events dir");
        fs::write(note_dir.join("a.md"), "placeholder").expect("first note");

        let before = canonical_fingerprint(&root).expect("before");
        fs::write(note_dir.join("b.md"), "placeholder").expect("second note");
        let after = canonical_fingerprint(&root).expect("after");

        assert_ne!(before, after, "adding a file must change the fingerprint");
        assert_eq!(after.canonical_files, before.canonical_files + 1);
    }

    /// A delete+add that nets the SAME file count, under an mtime-preserving
    /// cloud-sync arrival where the added file's mtime is <= the prior newest,
    /// leaves dir count, file count, and newest mtime all unchanged. Only the
    /// per-file name-hash combine moves, so without it the cache would be served
    /// stale and the new note invisible until `hm refresh`. This is the real
    /// false-negative the v10 name signal exists to close.
    #[test]
    fn freshness_detects_same_count_file_swap_under_preserved_mtime() {
        let dir = temp_dir("freshness-file-swap");
        let root = dir.join("store");
        let note_dir = root.join("inbox/notes/2026/05/16");
        fs::create_dir_all(&note_dir).expect("note dir");
        fs::create_dir_all(root.join("inbox/events")).expect("events dir");

        // Pin file A's mtime so we can land B at exactly the same instant,
        // neutralizing the newest-mtime signal as a mtime-preserving sync would.
        let pinned = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let path_a = note_dir.join("a.md");
        fs::write(&path_a, "placeholder").expect("note a");
        File::options()
            .write(true)
            .open(&path_a)
            .expect("open a")
            .set_modified(pinned)
            .expect("set a mtime");

        let before = canonical_fingerprint(&root).expect("before");

        // Swap A for B: same count, B's mtime forced equal to A's (the prior
        // newest), and the parent dir mtime left as-is to mimic the sync case.
        fs::remove_file(&path_a).expect("remove a");
        let path_b = note_dir.join("b.md");
        fs::write(&path_b, "placeholder").expect("note b");
        File::options()
            .write(true)
            .open(&path_b)
            .expect("open b")
            .set_modified(pinned)
            .expect("set b mtime");

        let after = canonical_fingerprint(&root).expect("after");

        assert_eq!(
            after.canonical_files, before.canonical_files,
            "the swap must keep the file count identical"
        );
        assert_eq!(
            after.latest_file_modified_nanos, before.latest_file_modified_nanos,
            "B's mtime must equal A's so the mtime signal cannot move"
        );
        assert_ne!(
            before.canonical_names_combined, after.canonical_names_combined,
            "the name-hash combine must change so the swap is visible"
        );
        assert_ne!(
            before, after,
            "a same-count file swap must change the fingerprint"
        );
    }

    /// The canonical file count participates in fingerprint equality, so a
    /// file-set change is detected even when every directory mtime is identical
    /// (the mtime-preserving cloud-sync case directory mtime alone would miss).
    #[test]
    fn fingerprint_file_count_drives_freshness() {
        let base = IndexFingerprint {
            schema_version: INDEX_FINGERPRINT_SCHEMA_VERSION,
            store_root: "/store".to_owned(),
            canonical_dirs: 3,
            latest_directory_modified_nanos: 42,
            canonical_files: 10,
            latest_file_modified_nanos: 42,
            canonical_names_combined: 0,
            entity_registry_modified_nanos: 0,
        };
        let one_more_file = IndexFingerprint {
            canonical_files: 11,
            ..base.clone()
        };
        assert_ne!(base, one_more_file);
    }

    /// Two threads racing `load_or_rebuild_index` against the same cache key must
    /// not corrupt the index: the cache-key lock serializes the rebuild+publish,
    /// and the atomic header publish means whoever wins leaves a valid index.
    #[test]
    fn cache_key_lock_serializes_concurrent_rebuilds() {
        let dir = temp_dir("lock-serialize");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(&root, true);

        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let root = root.clone();
                    let cache = cache.clone();
                    scope.spawn(move || {
                        load_or_rebuild_index(load_input("personal", &root, &cache))
                            .map(|report| report.entries.len())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread join"))
                .collect::<Vec<_>>()
        });

        for result in results {
            assert_eq!(result.expect("rebuild result"), 1);
        }
        // The final on-disk index is valid and fresh for the canonical store.
        let path = scoped_index_path(&cache, "personal", &root);
        let (fingerprint, entries) = read_index_with_fingerprint(&path)
            .expect("read")
            .expect("valid header after races");
        assert_eq!(
            fingerprint,
            canonical_fingerprint(&root).expect("fingerprint")
        );
        assert_eq!(entries.len(), 1);
    }

    /// The rebuild lock is held while taken and released on drop, so a second
    /// non-blocking acquire reports the existing holder rather than blocking.
    #[test]
    fn rebuild_lock_reports_existing_holder_without_blocking() {
        let dir = temp_dir("rebuild-lock");
        let root = dir.join("store");
        let cache = dir.join("cache");
        let first = try_rebuild_lock(&cache, "personal", &root)
            .expect("first lock")
            .expect("acquired");
        let second = try_rebuild_lock(&cache, "personal", &root).expect("second lock");
        assert!(second.is_none(), "held lock must report existing holder");
        drop(first);
        let third = try_rebuild_lock(&cache, "personal", &root).expect("third lock");
        assert!(third.is_some(), "lock is reacquirable after drop");
    }
}
