//! Rebuildable local triage index.
//!
//! The index is a cache, not canonical memory. It stores one compact JSON line
//! per inbox note so search/context can filter by metadata before reading note
//! bodies. If it is deleted or stale, it can always be rebuilt from notes and
//! paired JSON events in the store root.

use crate::note;
use crate::{event, path as memory_path, write};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

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
    /// Optional explicit memory kind, used by inject classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<note::MemoryKind>,
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
        }
    }
}

impl Error for IndexError {}

/// Read a fresh index when possible, otherwise rebuild it from canonical files.
///
/// Context and search run on latency-sensitive hook paths. They should pay for
/// full Markdown/event parsing only when canonical inbox files changed; the
/// fingerprint sidecar uses directory metadata so hot reads do not stat every
/// note. That catches create/delete/rename changes cheaply; content-only manual
/// edits rely on explicit `hm refresh`, which is the same maintenance path hooks
/// already run after writes.
pub fn load_or_rebuild_index(input: LoadIndexInput<'_>) -> Result<LoadIndexReport, IndexError> {
    let path = scoped_index_path(input.cache_dir, input.store_name, input.store_root);
    let fingerprint_path =
        index_fingerprint_path(input.cache_dir, input.store_name, input.store_root);
    let current = canonical_fingerprint(input.store_root)?;
    if let Ok(cached) = read_fingerprint(&fingerprint_path)
        && cached == current
        && path.is_file()
        && let Ok(entries) = read_index(&path)
    {
        return Ok(LoadIndexReport {
            path,
            entries,
            warnings: Vec::new(),
            rebuilt: false,
        });
    }

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

/// Rebuild one store's JSONL triage index from canonical inbox files.
///
/// Rebuilds are intentionally deterministic: note paths are sorted before
/// parsing, and entries are written in path order. Paired event metadata wins
/// when it parses cleanly; malformed events warn and fall back to note metadata
/// so one bad sidecar does not make the whole index unusable.
pub fn rebuild_index(input: RebuildIndexInput<'_>) -> Result<RebuildIndexReport, IndexError> {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
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
        ));
    }

    let path = scoped_index_path(input.cache_dir, input.store_name, input.store_root);
    let jsonl = render_jsonl(&entries)?;
    write::write_atomic(&path, jsonl.as_bytes(), &input.options).map_err(|err| IndexError::Io {
        action: "write index",
        path: path.clone(),
        message: err.to_string(),
    })?;
    let fingerprint = canonical_fingerprint(input.store_root)?;
    let fingerprint_path =
        index_fingerprint_path(input.cache_dir, input.store_name, input.store_root);
    write_fingerprint(&fingerprint_path, &fingerprint, &input.options)?;

    Ok(RebuildIndexReport {
        path,
        entries,
        warnings,
    })
}

/// Read an existing JSONL index file.
pub fn read_index(path: &Path) -> Result<Vec<IndexEntry>, IndexError> {
    let contents = fs::read_to_string(path).map_err(|err| io_error("read index", path, err))?;
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(|err| IndexError::Json(err.to_string())))
        .collect()
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

fn index_fingerprint_path(cache_dir: &Path, store_name: &str, store_root: &Path) -> PathBuf {
    cache_dir.join("indexes").join(format!(
        "{}.fingerprint.json",
        store_cache_key(store_name, store_root)
    ))
}

fn store_cache_key(store_name: &str, store_root: &Path) -> String {
    // Store aliases are local labels, not durable identities. Include the root
    // spelling in the cache filename so tests, alternate configs, and moved
    // stores do not race through one shared `personal.jsonl` file.
    let mut hasher = Sha256::new();
    hasher.update(store_root.display().to_string().as_bytes());
    let digest = hasher.finalize();
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{store_name}-{suffix}")
}

fn read_fingerprint(path: &Path) -> Result<IndexFingerprint, IndexError> {
    let contents =
        fs::read_to_string(path).map_err(|err| io_error("read index fingerprint", path, err))?;
    serde_json::from_str(&contents).map_err(|err| IndexError::Json(err.to_string()))
}

fn write_fingerprint(
    path: &Path,
    fingerprint: &IndexFingerprint,
    options: &write::AtomicWriteOptions,
) -> Result<(), IndexError> {
    let contents =
        serde_json::to_string(fingerprint).map_err(|err| IndexError::Json(err.to_string()))?;
    write::write_atomic(path, contents.as_bytes(), options).map_err(|err| IndexError::Io {
        action: "write index fingerprint",
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    Ok(())
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
) -> IndexEntry {
    if let Some(event) = event {
        // Event sidecars are the structured machine contract. The Markdown note
        // remains the human-readable canonical record, but paired event fields
        // are preferred for filters so future migrations/repairs can update
        // machine metadata without rewriting user prose.
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
            // Prefer the event's kind, falling back to the note's so a note that
            // carries kind without an event copy is still classified correctly.
            kind: event.kind.or(front_matter.kind),
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
        kind: front_matter.kind,
        classified: front_matter.classified.clone(),
        agent_id: front_matter.agent_id.clone(),
        host_id: front_matter.host_id.clone(),
        created_at: front_matter.created_at.clone(),
        body: body.to_owned(),
        note_path: note_path.to_owned(),
        event_path: None,
    }
}

fn render_jsonl(entries: &[IndexEntry]) -> Result<String, IndexError> {
    let mut output = String::new();
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

fn canonical_fingerprint(store_root: &Path) -> Result<IndexFingerprint, IndexError> {
    let mut dirs = Vec::new();
    collect_canonical_dirs(&store_root.join("inbox/notes"), &mut dirs)?;
    collect_canonical_dirs(&store_root.join("inbox/events"), &mut dirs)?;
    let mut latest_directory_modified_nanos = 0u128;
    // Directory metadata is the deliberate hot-path compromise: writes, deletes,
    // renames, and cloud-sync file arrivals update parent directories without
    // forcing every `hm context` or `hm search` call to stat thousands of notes.
    // Content-only manual edits are handled by explicit `hm refresh --force`.
    for path in &dirs {
        let metadata = fs::metadata(path)
            .map_err(|err| io_error("read canonical directory metadata", path, err))?;
        latest_directory_modified_nanos = latest_directory_modified_nanos.max(modified_nanos(
            metadata
                .modified()
                .map_err(|err| io_error("read canonical directory modified time", path, err))?,
        ));
    }
    Ok(IndexFingerprint {
        // v5: entries carry `classified` provenance for the LLM review queue.
        schema_version: 5,
        store_root: store_root.display().to_string(),
        canonical_dirs: dirs.len(),
        latest_directory_modified_nanos,
    })
}

fn collect_canonical_dirs(root: &Path, paths: &mut Vec<PathBuf>) -> Result<(), IndexError> {
    if !root.is_dir() {
        return Ok(());
    }
    paths.push(root.to_path_buf());
    for entry in
        fs::read_dir(root).map_err(|err| io_error("read canonical directory", root, err))?
    {
        let entry = entry.map_err(|err| io_error("read canonical directory", root, err))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| io_error("read canonical file type", &path, err))?;
        if file_type.is_dir() {
            collect_canonical_dirs(&path, paths)?;
        }
    }
    Ok(())
}

fn modified_nanos(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
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
}
