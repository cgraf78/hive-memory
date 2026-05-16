//! Manual inbox triage and curated-memory promotion.
//!
//! Raw `hm note` entries are durable evidence, but they are not trusted enough
//! to render by default. This module owns the human curation bridge: list raw
//! inbox notes, detect which ones were already promoted, append a concise
//! curated entry, and record a promotion event for audit/idempotency.

use crate::index::IndexEntry;
use crate::{event, id, note, store, write};
use fs2::FileExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{self, Display};
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use time::OffsetDateTime;

/// Default curated target for global raw-note promotions.
pub const DEFAULT_PROMOTION_TARGET: &str = "memories/global/MEMORY.md";

/// One raw inbox note and its promotion state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InboxItem {
    /// Indexed metadata for the raw note.
    pub entry: IndexEntry,
    /// Whether a promotion event already references this note id.
    pub promoted: bool,
}

/// Input for listing raw inbox notes.
#[derive(Debug, Clone)]
pub struct InboxListInput<'a> {
    /// Store root containing canonical notes and promotion events.
    pub store_root: &'a Path,
    /// Rebuilt index entries for the selected store.
    pub entries: &'a [IndexEntry],
    /// Include already-promoted notes in the result.
    pub include_promoted: bool,
    /// Optional cutoff; only unpromoted notes older than this are returned.
    pub stale_before: Option<OffsetDateTime>,
}

/// Input for promoting one raw inbox note.
#[derive(Debug, Clone)]
pub struct PromotionInput<'a> {
    /// Store root receiving curated output and the promotion event.
    pub store_root: &'a Path,
    /// Parsed store manifest for stable event identity metadata.
    pub manifest: &'a store::StoreManifest,
    /// Rebuilt index entries for finding the source note by id.
    pub entries: &'a [IndexEntry],
    /// Raw inbox note id to promote.
    pub note_id: &'a str,
    /// Store-relative curated target path.
    pub target: &'a Path,
    /// Whether to preserve the source body instead of making a bullet.
    pub verbatim: bool,
    /// Agent/human identity recording the promotion event.
    pub agent_id: &'a str,
    /// Host identity recording the promotion event.
    pub host_id: &'a str,
    /// User identity recording the promotion event.
    pub user_id: &'a str,
    /// Atomic write durability options for target and event writes.
    pub options: write::AtomicWriteOptions,
}

/// Result of a promotion request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromotionReport {
    /// Source raw note id.
    pub note_id: String,
    /// Store-relative curated target path.
    pub target_path: String,
    /// Final curated target path on disk.
    pub target_full_path: PathBuf,
    /// Final promotion event path when a new event was written.
    pub event_path: Option<PathBuf>,
    /// Whether this invocation appended new curated content.
    pub promoted: bool,
}

/// Curation workflow failure.
#[derive(Debug)]
pub enum CurationError {
    /// Requested note id is not a raw inbox note in the selected store.
    NoteNotFound(String),
    /// Target path is absolute, escapes the store, or targets a non-curated area.
    InvalidTarget(String),
    /// Filesystem operation failed.
    Io {
        /// Operation that failed.
        action: &'static str,
        /// Path involved in the failure.
        path: PathBuf,
        /// Original error rendered for CLI diagnostics.
        message: String,
    },
    /// Source note could not be parsed.
    Note(String),
    /// Promotion event could not be rendered or written.
    Event(String),
    /// Target file changed after the promotion lock was acquired.
    ConcurrentEdit(PathBuf),
}

impl Display for CurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoteNotFound(id) => write!(f, "raw inbox note not found: {id}"),
            Self::InvalidTarget(message) => write!(f, "invalid promotion target: {message}"),
            Self::Io {
                action,
                path,
                message,
            } => write!(f, "failed to {action} {}: {message}", path.display()),
            Self::Note(message) => write!(f, "failed to read source note: {message}"),
            Self::Event(message) => write!(f, "failed to write promotion event: {message}"),
            Self::ConcurrentEdit(path) => {
                write!(
                    f,
                    "curated target changed during promotion: {}",
                    path.display()
                )
            }
        }
    }
}

impl Error for CurationError {}

/// List raw inbox notes with promotion state.
///
/// Promotion state comes from promotion events, not from editing the source
/// notes. That keeps raw inbox material immutable and lets `hm inbox` answer
/// "what remains to triage?" even after curated files move or are edited.
pub fn list_inbox(input: InboxListInput<'_>) -> Result<Vec<InboxItem>, CurationError> {
    let promoted = promoted_note_ids(input.store_root)?;
    let mut items = input
        .entries
        .iter()
        .filter(|entry| entry.entry_kind == note::EntryKind::Note)
        .filter_map(|entry| {
            let is_promoted = promoted.contains(&entry.id);
            if is_promoted && !input.include_promoted {
                return None;
            }
            if let Some(cutoff) = input.stale_before
                && (is_promoted
                    || timestamp_rank(&entry.created_at) >= cutoff.unix_timestamp_nanos())
            {
                return None;
            }
            Some(InboxItem {
                entry: entry.clone(),
                promoted: is_promoted,
            })
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        timestamp_rank(&left.entry.created_at)
            .cmp(&timestamp_rank(&right.entry.created_at))
            .then_with(|| left.entry.note_path.cmp(&right.entry.note_path))
    });
    Ok(items)
}

/// Return one raw inbox item by note id.
pub fn show_inbox_item(
    store_root: &Path,
    entries: &[IndexEntry],
    note_id: &str,
) -> Result<(InboxItem, note::MarkdownNote), CurationError> {
    let promoted = promoted_note_ids(store_root)?;
    let entry = entries
        .iter()
        .find(|entry| entry.id == note_id && entry.entry_kind == note::EntryKind::Note)
        .ok_or_else(|| CurationError::NoteNotFound(note_id.to_owned()))?
        .clone();
    let parsed = read_note(store_root, &entry.note_path)?;
    Ok((
        InboxItem {
            promoted: promoted.contains(&entry.id),
            entry,
        },
        parsed,
    ))
}

/// Promote one raw inbox note into a curated Markdown file.
///
/// V1 curation is intentionally local-host only. The lock prevents two local
/// `hm promote` processes from interleaving writes, but it is not a cloud-sync
/// distributed lock. The provenance marker and promotion event make retries
/// idempotent for the same source-note/target-file pair.
pub fn promote(input: PromotionInput<'_>) -> Result<PromotionReport, CurationError> {
    let target = validate_target(input.target)?;
    let target_path = input.store_root.join(&target);
    let target_key = path_string(&target);
    if promotion_exists(input.store_root, input.note_id, &target_key)? {
        return Ok(PromotionReport {
            note_id: input.note_id.to_owned(),
            target_path: target_key,
            target_full_path: target_path,
            event_path: None,
            promoted: false,
        });
    }

    let source = input
        .entries
        .iter()
        .find(|entry| entry.id == input.note_id && entry.entry_kind == note::EntryKind::Note)
        .ok_or_else(|| CurationError::NoteNotFound(input.note_id.to_owned()))?;
    let parsed = read_note(input.store_root, &source.note_path)?;
    let Some(parent) = target_path.parent() else {
        return Err(CurationError::InvalidTarget(
            "target path has no parent".to_owned(),
        ));
    };
    fs::create_dir_all(parent).map_err(|err| io_error("create target parent", parent, err))?;

    let lock_path = target_path.with_extension(format!(
        "{}lock",
        target_path
            .extension()
            .and_then(|value| value.to_str())
            .map(|ext| format!("{ext}."))
            .unwrap_or_default()
    ));
    let lock = lock_target(&lock_path)?;
    let before = read_optional(&target_path)?;
    let before_hash = hash_bytes(&before);
    if contains_promotion_marker(&before, input.note_id, &target_key) {
        // If a prior run appended curated text but failed before the audit
        // event was published, retry should repair the missing event instead
        // of leaving inbox state permanently "pending".
        let event_path = write_promotion_event(&input, source, &target_key)?;
        unlock_target(lock);
        return Ok(PromotionReport {
            note_id: input.note_id.to_owned(),
            target_path: target_key,
            target_full_path: target_path,
            event_path: Some(event_path),
            promoted: false,
        });
    }

    let appended = render_curated_append(input.note_id, &target_key, &parsed.body, input.verbatim);
    let mut next = before.clone();
    if !next.is_empty() && !next.ends_with(b"\n") {
        next.push(b'\n');
    }
    next.extend_from_slice(appended.as_bytes());

    let current = read_optional(&target_path)?;
    if hash_bytes(&current) != before_hash {
        unlock_target(lock);
        return Err(CurationError::ConcurrentEdit(target_path));
    }
    write::write_atomic(&target_path, &next, &input.options).map_err(|err| CurationError::Io {
        action: "write curated target",
        path: target_path.clone(),
        message: err.to_string(),
    })?;
    let event_path = write_promotion_event(&input, source, &target_key)?;
    unlock_target(lock);
    Ok(PromotionReport {
        note_id: input.note_id.to_owned(),
        target_path: target_key,
        target_full_path: target_path,
        event_path: Some(event_path),
        promoted: true,
    })
}

fn promoted_note_ids(store_root: &Path) -> Result<BTreeSet<String>, CurationError> {
    let mut ids = BTreeSet::new();
    for event in promotion_events(store_root)? {
        if let Some(source) = event.source.and_then(|source| source.r#ref) {
            ids.insert(source);
        }
    }
    Ok(ids)
}

fn promotion_exists(
    store_root: &Path,
    note_id: &str,
    target_path: &str,
) -> Result<bool, CurationError> {
    Ok(promotion_events(store_root)?.into_iter().any(|event| {
        event.source.and_then(|source| source.r#ref).as_deref() == Some(note_id)
            && event.body.lines().any(|line| {
                line.strip_prefix("target_path = ")
                    .map(|value| value.trim_matches('"') == target_path)
                    .unwrap_or(false)
            })
    }))
}

fn promotion_events(store_root: &Path) -> Result<Vec<event::MemoryEvent>, CurationError> {
    let events_root = store_root.join("inbox/events");
    let paths = match collect_json_files(&events_root) {
        Ok(paths) => paths,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io_error("scan events", &events_root, err)),
    };
    let mut events = Vec::new();
    for path in paths {
        let contents =
            fs::read_to_string(&path).map_err(|err| io_error("read event", &path, err))?;
        let parsed =
            event::parse_event(&contents).map_err(|err| CurationError::Event(err.to_string()))?;
        if parsed.event_type == event::EventType::Promotion {
            events.push(parsed);
        }
    }
    Ok(events)
}

fn write_promotion_event(
    input: &PromotionInput<'_>,
    source: &IndexEntry,
    target_path: &str,
) -> Result<PathBuf, CurationError> {
    let created_at = OffsetDateTime::now_utc();
    let event_id = id::new_write_id(&id::WriteIdContext {
        host_id: input.host_id.to_owned(),
        agent_id: input.agent_id.to_owned(),
    });
    let event = event::MemoryEvent {
        schema_version: event::EVENT_SCHEMA_VERSION,
        event_type: event::EventType::Promotion,
        id: event_id.clone(),
        store_id: input.manifest.store.id.clone(),
        store_name: input.manifest.store.name.clone(),
        created_at: rfc3339(created_at),
        agent_id: input.agent_id.to_owned(),
        host_id: input.host_id.to_owned(),
        user_id: Some(input.user_id.to_owned()),
        session_id: None,
        scope: source.scope.clone(),
        project_id: source.project_id.clone(),
        subject: Some("inbox promotion".to_owned()),
        tags: vec!["promotion".to_owned()],
        confidence: source.confidence,
        audience: Vec::new(),
        body: format!(
            "source_note_id = \"{}\"\ntarget_path = \"{}\"\n",
            input.note_id, target_path
        ),
        note_path: Some(source.note_path.clone()),
        source: Some(event::EventSource {
            kind: "promotion".to_owned(),
            r#ref: Some(input.note_id.to_owned()),
        }),
    };
    let path = input
        .store_root
        .join(event::event_relative_path(&event_id, created_at));
    let rendered =
        event::render_event(&event).map_err(|err| CurationError::Event(err.to_string()))?;
    write::write_atomic_create_new(&path, rendered.as_bytes(), &input.options)
        .map_err(|err| CurationError::Event(err.to_string()))?;
    Ok(path)
}

fn read_note(store_root: &Path, relative_path: &str) -> Result<note::MarkdownNote, CurationError> {
    let path = store_root.join(relative_path);
    let contents = fs::read_to_string(&path).map_err(|err| io_error("read note", &path, err))?;
    note::parse_note(&contents).map_err(|err| CurationError::Note(err.to_string()))
}

fn render_curated_append(note_id: &str, target_path: &str, body: &str, verbatim: bool) -> String {
    let marker =
        format!("<!-- hive-memory:promoted source=\"{note_id}\" target=\"{target_path}\" -->");
    if verbatim {
        return format!("\n{marker}\n\n{}\n", body.trim());
    }
    let one_line = body.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("\n- {one_line}\n  {marker}\n")
}

fn contains_promotion_marker(contents: &[u8], note_id: &str, target_path: &str) -> bool {
    let Ok(contents) = std::str::from_utf8(contents) else {
        return false;
    };
    contents.contains(&format!(
        "hive-memory:promoted source=\"{note_id}\" target=\"{target_path}\""
    ))
}

fn validate_target(path: &Path) -> Result<PathBuf, CurationError> {
    if path.is_absolute() {
        return Err(CurationError::InvalidTarget(
            "target must be store-relative".to_owned(),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let Some(value) = value.to_str() else {
                    return Err(CurationError::InvalidTarget(
                        "target must be valid UTF-8".to_owned(),
                    ));
                };
                if value.contains(['"', '\n', '\r', '\\']) {
                    return Err(CurationError::InvalidTarget(
                        "target components must not contain quotes, newlines, or backslashes"
                            .to_owned(),
                    ));
                }
                normalized.push(value);
            }
            Component::CurDir => {}
            _ => {
                return Err(CurationError::InvalidTarget(
                    "target must not contain parent or prefix components".to_owned(),
                ));
            }
        }
    }
    let first = normalized
        .components()
        .next()
        .and_then(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        });
    if !matches!(first, Some("memories" | "people" | "rules")) {
        return Err(CurationError::InvalidTarget(
            "target must live under memories/, people/, or rules/".to_owned(),
        ));
    }
    if normalized.as_os_str().is_empty() {
        return Err(CurationError::InvalidTarget(
            "target must not be empty".to_owned(),
        ));
    }
    Ok(normalized)
}

fn collect_json_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    collect_json_files_into(root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn collect_json_files_into(root: &Path, paths: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_json_files_into(&path, paths)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    Ok(())
}

fn read_optional(path: &Path) -> Result<Vec<u8>, CurationError> {
    match fs::read(path) {
        Ok(contents) => Ok(contents),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(io_error("read curated target", path, err)),
    }
}

fn lock_target(path: &Path) -> Result<File, CurationError> {
    let Some(parent) = path.parent() else {
        return Err(CurationError::InvalidTarget(
            "lock path has no parent".to_owned(),
        ));
    };
    fs::create_dir_all(parent).map_err(|err| io_error("create lock parent", parent, err))?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(path)
        .map_err(|err| io_error("open promotion lock", path, err))?;
    file.lock_exclusive()
        .map_err(|err| io_error("lock promotion target", path, err))?;
    Ok(file)
}

fn unlock_target(file: File) {
    let _ = file.unlock();
}

fn hash_bytes(contents: &[u8]) -> String {
    format!("{:x}", Sha256::digest(contents))
}

fn path_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn timestamp_rank(value: &str) -> i128 {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map(|timestamp| timestamp.unix_timestamp_nanos())
        .unwrap_or_default()
}

fn rfc3339(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting should not fail")
}

fn io_error(action: &'static str, path: &Path, err: std::io::Error) -> CurationError {
    CurationError::Io {
        action,
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}
