//! Rebuildable local triage index.
//!
//! The index is a cache, not canonical memory. It stores one compact JSON line
//! per inbox note so search/context can filter by metadata before reading note
//! bodies. If it is deleted or stale, it can always be rebuilt from notes and
//! paired JSON events in the store root.

use crate::note;
use crate::{event, write};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};
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
    /// Agent that wrote the record.
    pub agent_id: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
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
        let relative_note_path = relative_path(input.store_root, &note_path);
        let contents =
            fs::read_to_string(&note_path).map_err(|err| io_error("read note", &note_path, err))?;
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
        let relative_event_path = relative_path(input.store_root, &expected_event_path);
        let event = read_paired_event(&expected_event_path, &parsed.front_matter.id, &mut warnings);
        entries.push(entry_from_note(
            &parsed.front_matter,
            &relative_note_path,
            event.as_ref().map(|_| relative_event_path.as_str()),
            event.as_ref(),
        ));
    }

    let path = index_path(input.cache_dir, input.store_name);
    let jsonl = render_jsonl(&entries)?;
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
    note_path: &str,
    event_path: Option<&str>,
    event: Option<&event::MemoryEvent>,
) -> IndexEntry {
    if let Some(event) = event {
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
            agent_id: event.agent_id.clone(),
            created_at: event.created_at.clone(),
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
        agent_id: front_matter.agent_id.clone(),
        created_at: front_matter.created_at.clone(),
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

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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
        })
        .expect("rebuild index");

        assert_eq!(report.entries.len(), 1);
        assert!(report.warnings.is_empty());
        assert_eq!(report.path, cache.join("indexes/personal.jsonl"));
        assert_eq!(
            report.entries[0].note_path,
            relative_path(&root, &written.note_path)
        );
        assert_eq!(
            report.entries[0].event_path.as_deref(),
            Some(relative_path(&root, &written.event_path.expect("event")).as_str())
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
        })
        .expect("rebuild index");

        assert!(report.entries.is_empty());
        assert_eq!(report.warnings.len(), 1);
    }
}
