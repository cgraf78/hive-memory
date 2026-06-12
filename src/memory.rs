//! High-level memory write workflow.
//!
//! The lower-level `note`, `event`, and `write` modules each own one file
//! format or filesystem primitive. This module composes them into the logical
//! operation users and agents care about: one memory record, optionally backed
//! by both a Markdown note and JSON sidecar with the same id.

use crate::{event, note, store, write};
use std::error::Error;
use std::fmt::{self, Display};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// Input for writing one logical memory record.
///
/// The caller resolves policy first: store affinity, scope, audience, secret
/// handling, and event-sidecar choice should already be decided before this
/// function touches the filesystem.
#[derive(Debug, Clone)]
pub struct WriteRecordInput<'a> {
    /// Store root receiving the canonical files.
    pub root: &'a Path,
    /// Parsed store manifest for identity metadata.
    pub manifest: &'a store::StoreManifest,
    /// Whether this is a remembered fact or lower-confidence note.
    pub entry_kind: note::EntryKind,
    /// Timestamp shared by the note and optional event.
    pub created_at: OffsetDateTime,
    /// Agent identity recorded in metadata and write ids.
    pub agent_id: String,
    /// Host identity recorded in metadata and write ids.
    pub host_id: String,
    /// User identity recorded in metadata.
    pub user_id: String,
    /// Optional agent session id.
    pub session_id: Option<String>,
    /// Memory scope.
    pub scope: String,
    /// Writer confidence.
    pub confidence: note::Confidence,
    /// Human-readable Markdown body and event indexing body.
    pub body: String,
    /// Optional project identity.
    pub project_id: Option<String>,
    /// Optional short subject.
    pub subject: Option<String>,
    /// Optional explicit memory kind driving inject selection.
    pub kind: Option<note::MemoryKind>,
    /// Optional tags.
    pub tags: Vec<String>,
    /// Explicit audience for agent-private records.
    pub audience: Vec<String>,
    /// Optional source category.
    pub source_kind: Option<String>,
    /// Optional source reference.
    pub source_ref: Option<String>,
    /// Whether to write a paired JSON event.
    pub write_event: bool,
    /// Atomic write durability/collision options.
    pub options: write::AtomicWriteOptions,
}

/// Result of writing one logical memory record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRecordResult {
    /// Shared logical record id.
    pub id: String,
    /// Final Markdown note path.
    pub note_path: PathBuf,
    /// Final JSON event path when a sidecar was written.
    pub event_path: Option<PathBuf>,
}

/// High-level memory write failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryError {
    /// Could not find a collision-free id/path pair.
    CollisionLimit,
    /// Note rendering or writing failed.
    Note(String),
    /// Event rendering or writing failed after the note path was selected.
    Event(String),
    /// A memory kind was incompatible with its record scope/project metadata.
    InvalidKindContext(String),
}

impl Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CollisionLimit => write!(f, "could not find a unique memory id"),
            Self::Note(message) => write!(f, "failed to write note: {message}"),
            Self::Event(message) => write!(f, "failed to write event: {message}"),
            Self::InvalidKindContext(message) => write!(f, "{message}"),
        }
    }
}

impl Error for MemoryError {}

/// Write one memory record to the canonical inbox.
///
/// This function deliberately does not promise a cross-file transaction: normal
/// filesystems do not give us an atomic "write note and event together" commit.
/// The function minimizes partial pairs by checking both candidate paths before
/// writing and by using create-if-absent publishing for each file. If the second
/// file still fails, doctor/search can detect the incomplete pair from the
/// shared id and repair or warn without losing the canonical Markdown note.
pub fn write_record(input: WriteRecordInput<'_>) -> Result<WriteRecordResult, MemoryError> {
    for _ in 0..input.options.max_attempts.max(1) {
        let id = crate::id::new_write_id(&crate::id::WriteIdContext {
            host_id: input.host_id.clone(),
            agent_id: input.agent_id.clone(),
        });
        let note_relative_path = note::note_relative_path(&id, input.created_at);
        let event_relative_path = event::event_relative_path(&id, input.created_at);
        if input.root.join(&note_relative_path).exists()
            || (input.write_event && input.root.join(&event_relative_path).exists())
        {
            continue;
        }

        let event = if input.write_event {
            Some(
                event::MemoryEvent::observation(event::EventObservationInput {
                    id: id.clone(),
                    store_id: input.manifest.store.id.clone(),
                    store_name: input.manifest.store.name.clone(),
                    created_at: input.created_at,
                    agent_id: input.agent_id.clone(),
                    host_id: input.host_id.clone(),
                    user_id: Some(input.user_id.clone()),
                    session_id: input.session_id.clone(),
                    scope: input.scope.clone(),
                    project_id: input.project_id.clone(),
                    subject: input.subject.clone(),
                    tags: input.tags.clone(),
                    confidence: input.confidence,
                    kind: input.kind,
                    classified: None,
                    audience: input.audience.clone(),
                    body: input.body.clone(),
                    note_path: Some(note_relative_path.clone()),
                    source: input.source_kind.clone().map(|kind| event::EventSource {
                        kind,
                        r#ref: input.source_ref.clone(),
                    }),
                })
                .map_err(|err| MemoryError::Event(err.to_string()))?,
            )
        } else {
            None
        };

        let note_input = note::NoteWriteInput {
            entry_kind: input.entry_kind,
            store_id: input.manifest.store.id.clone(),
            store_name: input.manifest.store.name.clone(),
            created_at: input.created_at,
            agent_id: input.agent_id.clone(),
            host_id: input.host_id.clone(),
            scope: input.scope.clone(),
            confidence: input.confidence,
            body: input.body.clone(),
            user_id: Some(input.user_id.clone()),
            session_id: input.session_id.clone(),
            project_id: input.project_id.clone(),
            subject: input.subject.clone(),
            tags: input.tags.clone(),
            source_kind: input.source_kind.clone(),
            source_ref: input.source_ref.clone(),
            related_event_id: input.write_event.then(|| id.clone()),
            expires_at: None,
            kind: input.kind,
            classified: None,
            audience: input.audience.clone(),
        };
        let note_result =
            note::write_note_with_id_generator(input.root, &note_input, &input.options, || {
                id.clone()
            })
            .map_err(|err| MemoryError::Note(err.to_string()))?;

        let event_path = if let Some(event) = event {
            Some(
                event::write_event(input.root, &event, &input.options)
                    .map_err(|err| MemoryError::Event(err.to_string()))?
                    .path,
            )
        } else {
            None
        };

        return Ok(WriteRecordResult {
            id,
            note_path: note_result.path,
            event_path,
        });
    }

    Err(MemoryError::CollisionLimit)
}

/// Validate that a persisted kind is compatible with record metadata.
///
/// `project-fact` only has safe injection semantics when a project filter is
/// available. Centralizing the check keeps write, retag, and background LLM
/// classification from drifting into subtly different definitions.
pub fn validate_kind_context(
    kind: Option<note::MemoryKind>,
    scope: &str,
    project_id: Option<&str>,
) -> Result<(), MemoryError> {
    if kind != Some(note::MemoryKind::ProjectFact) {
        return Ok(());
    }

    if scope == "project" && project_id.is_some() {
        return Ok(());
    }

    Err(MemoryError::InvalidKindContext(
        "`--kind project-fact` requires `--scope project` and a resolved project id".to_owned(),
    ))
}

/// How to update stored provenance alongside a kind verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassifiedUpdate {
    /// Leave existing provenance untouched.
    Keep,
    /// Remove provenance entirely.
    Clear,
    /// Persist this provenance.
    Set(note::ClassifiedBy),
}

/// Input for retagging one existing record's memory kind.
///
/// Retag exists so a wrong write-time inference is correctable: kind is a
/// persisted, behavior-shaping verdict (search-only vs always-on), and the
/// only previous remedy was hand-editing synced Markdown.
#[derive(Debug, Clone)]
pub struct RetagRecordInput<'a> {
    /// Store root holding the canonical files.
    pub root: &'a Path,
    /// Store-relative Markdown note path, as carried by the index.
    pub note_path: &'a str,
    /// New kind; `None` clears the tag so read-time classification applies.
    pub kind: Option<note::MemoryKind>,
    /// Provenance update applied together with the kind.
    pub classified: ClassifiedUpdate,
    /// Atomic write durability options.
    pub options: write::AtomicWriteOptions,
}

/// Result of retagging one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetagRecordResult {
    /// Shared logical record id.
    pub id: String,
    /// Kind carried by the record before the rewrite.
    pub previous_kind: Option<note::MemoryKind>,
    /// Kind persisted by the rewrite.
    pub kind: Option<note::MemoryKind>,
    /// Absolute note path that was rewritten.
    pub note_path: PathBuf,
    /// Absolute event path that was rewritten, when a sidecar existed.
    pub event_path: Option<PathBuf>,
}

/// Rewrite the persisted memory kind on an existing note/event pair.
///
/// Both copies must change together: the index prefers event metadata, so a
/// note-only rewrite would be silently undone by the next index rebuild. Like
/// `write_record`, this does not promise a cross-file transaction; the note is
/// rewritten first and a missing sidecar is tolerated (note-only records).
/// Rewrites use replace-mode atomic publishing, and the resulting mtime bump
/// invalidates the index fingerprint so read paths pick up the new kind.
pub fn retag_record(input: RetagRecordInput<'_>) -> Result<RetagRecordResult, MemoryError> {
    let note_path = input.root.join(input.note_path);
    let contents = std::fs::read_to_string(&note_path)
        .map_err(|err| MemoryError::Note(format!("read {}: {err}", note_path.display())))?;
    let mut parsed =
        note::parse_note(&contents).map_err(|err| MemoryError::Note(err.to_string()))?;
    validate_kind_context(
        input.kind,
        &parsed.front_matter.scope,
        parsed.front_matter.project_id.as_deref(),
    )?;
    let previous_kind = parsed.front_matter.kind;
    parsed.front_matter.kind = input.kind;
    apply_classified_update(&mut parsed.front_matter.classified, &input.classified);
    let rendered = note::render_note(&parsed).map_err(|err| MemoryError::Note(err.to_string()))?;
    write::write_atomic(&note_path, rendered.as_bytes(), &input.options)
        .map_err(|err| MemoryError::Note(err.to_string()))?;

    let created_at = OffsetDateTime::parse(
        &parsed.front_matter.created_at,
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|err| MemoryError::Note(format!("parse created_at: {err}")))?;
    let event_path = input.root.join(event::event_relative_path(
        &parsed.front_matter.id,
        created_at,
    ));
    let event_path = match std::fs::read_to_string(&event_path) {
        Ok(event_contents) => {
            let mut event = event::parse_event(&event_contents)
                .map_err(|err| MemoryError::Event(err.to_string()))?;
            event.kind = input.kind;
            apply_classified_update(&mut event.classified, &input.classified);
            let rendered_event =
                event::render_event(&event).map_err(|err| MemoryError::Event(err.to_string()))?;
            write::write_atomic(&event_path, rendered_event.as_bytes(), &input.options)
                .map_err(|err| MemoryError::Event(err.to_string()))?;
            Some(event_path)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(MemoryError::Event(format!(
                "read {}: {err}",
                event_path.display()
            )));
        }
    };

    Ok(RetagRecordResult {
        id: parsed.front_matter.id,
        previous_kind,
        kind: input.kind,
        note_path,
        event_path,
    })
}

fn apply_classified_update(target: &mut Option<note::ClassifiedBy>, update: &ClassifiedUpdate) {
    match update {
        ClassifiedUpdate::Keep => {}
        ClassifiedUpdate::Clear => *target = None,
        ClassifiedUpdate::Set(classified) => *target = Some(classified.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Sensitivity;
    use crate::write::{AtomicWriteOptions, FsyncPolicy};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hive-memory-memory-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn manifest() -> store::StoreManifest {
        store::StoreManifest::with_identity(
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

    fn write(root: &Path, kind: Option<note::MemoryKind>, write_event: bool) -> WriteRecordResult {
        write_record(WriteRecordInput {
            root,
            manifest: &manifest(),
            entry_kind: note::EntryKind::Remember,
            created_at: OffsetDateTime::from_unix_timestamp(1_778_946_153).expect("timestamp"),
            agent_id: "codex".to_owned(),
            host_id: "taylor".to_owned(),
            user_id: "chris".to_owned(),
            session_id: None,
            scope: "global".to_owned(),
            confidence: note::Confidence::High,
            body: "Retag should rewrite this record.".to_owned(),
            project_id: None,
            subject: None,
            kind,
            tags: Vec::new(),
            audience: Vec::new(),
            source_kind: None,
            source_ref: None,
            write_event,
            options: options(),
        })
        .expect("write memory record")
    }

    fn relative(root: &Path, path: &Path) -> String {
        path.strip_prefix(root)
            .expect("store-relative path")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn retag_rewrites_note_and_event_together() {
        let root = temp_dir("retag-pair");
        let written = write(&root, Some(note::MemoryKind::Preference), true);

        let result = retag_record(RetagRecordInput {
            root: &root,
            note_path: &relative(&root, &written.note_path),
            kind: Some(note::MemoryKind::Incident),
            classified: ClassifiedUpdate::Keep,
            options: options(),
        })
        .expect("retag");

        assert_eq!(result.id, written.id);
        assert_eq!(result.previous_kind, Some(note::MemoryKind::Preference));
        assert_eq!(result.kind, Some(note::MemoryKind::Incident));
        let note = note::parse_note(&fs::read_to_string(&written.note_path).expect("read note"))
            .expect("parse note");
        assert_eq!(note.front_matter.kind, Some(note::MemoryKind::Incident));
        assert_eq!(note.body, "Retag should rewrite this record.");
        let event_path = result.event_path.expect("event rewritten");
        let event = event::parse_event(&fs::read_to_string(event_path).expect("read event"))
            .expect("parse event");
        assert_eq!(event.kind, Some(note::MemoryKind::Incident));
    }

    #[test]
    fn retag_clears_kind() {
        let root = temp_dir("retag-clear");
        let written = write(&root, Some(note::MemoryKind::Reference), true);

        let result = retag_record(RetagRecordInput {
            root: &root,
            note_path: &relative(&root, &written.note_path),
            kind: None,
            classified: ClassifiedUpdate::Clear,
            options: options(),
        })
        .expect("retag");

        assert_eq!(result.previous_kind, Some(note::MemoryKind::Reference));
        assert_eq!(result.kind, None);
        let note = note::parse_note(&fs::read_to_string(&written.note_path).expect("read note"))
            .expect("parse note");
        assert_eq!(note.front_matter.kind, None);
    }

    #[test]
    fn retag_tolerates_missing_event_sidecar() {
        let root = temp_dir("retag-note-only");
        let written = write(&root, None, false);

        let result = retag_record(RetagRecordInput {
            root: &root,
            note_path: &relative(&root, &written.note_path),
            kind: Some(note::MemoryKind::Reference),
            classified: ClassifiedUpdate::Keep,
            options: options(),
        })
        .expect("retag");

        assert_eq!(result.previous_kind, None);
        assert_eq!(result.event_path, None);
        let note = note::parse_note(&fs::read_to_string(&written.note_path).expect("read note"))
            .expect("parse note");
        assert_eq!(note.front_matter.kind, Some(note::MemoryKind::Reference));
    }

    #[test]
    fn retag_fails_on_missing_note() {
        let root = temp_dir("retag-missing");
        let result = retag_record(RetagRecordInput {
            root: &root,
            note_path: "inbox/notes/2026-05-16/missing.md",
            kind: Some(note::MemoryKind::Incident),
            classified: ClassifiedUpdate::Keep,
            options: options(),
        });
        assert!(matches!(result, Err(MemoryError::Note(_))));
    }

    #[test]
    fn retag_persists_provenance_on_note_and_event() {
        let root = temp_dir("retag-provenance");
        let written = write(&root, None, true);
        let classified = note::ClassifiedBy {
            source: note::ClassifierSource::Manual,
            backend: None,
            at: "2026-06-12T00:00:00Z".to_owned(),
            verdict_version: 0,
            confidence: None,
        };

        retag_record(RetagRecordInput {
            root: &root,
            note_path: &relative(&root, &written.note_path),
            kind: Some(note::MemoryKind::Preference),
            classified: ClassifiedUpdate::Set(classified.clone()),
            options: options(),
        })
        .expect("retag");

        let note = note::parse_note(&fs::read_to_string(&written.note_path).expect("read note"))
            .expect("parse note");
        assert_eq!(note.front_matter.classified, Some(classified.clone()));
        let created_at = OffsetDateTime::parse(
            &note.front_matter.created_at,
            &time::format_description::well_known::Rfc3339,
        )
        .expect("created_at");
        let event_path = root.join(event::event_relative_path(
            &note.front_matter.id,
            created_at,
        ));
        let event = event::parse_event(&fs::read_to_string(event_path).expect("read event"))
            .expect("parse event");
        assert_eq!(event.classified, Some(classified));
    }

    #[test]
    fn retag_clears_provenance_on_note_and_event() {
        let root = temp_dir("retag-clear-provenance");
        let written = write(&root, None, true);
        let classified = note::ClassifiedBy {
            source: note::ClassifierSource::Manual,
            backend: None,
            at: "2026-06-12T00:00:00Z".to_owned(),
            verdict_version: 0,
            confidence: None,
        };
        retag_record(RetagRecordInput {
            root: &root,
            note_path: &relative(&root, &written.note_path),
            kind: Some(note::MemoryKind::Preference),
            classified: ClassifiedUpdate::Set(classified),
            options: options(),
        })
        .expect("set provenance");

        retag_record(RetagRecordInput {
            root: &root,
            note_path: &relative(&root, &written.note_path),
            kind: None,
            classified: ClassifiedUpdate::Clear,
            options: options(),
        })
        .expect("clear provenance");

        let note = note::parse_note(&fs::read_to_string(&written.note_path).expect("read note"))
            .expect("parse note");
        assert_eq!(note.front_matter.classified, None);
        let created_at = OffsetDateTime::parse(
            &note.front_matter.created_at,
            &time::format_description::well_known::Rfc3339,
        )
        .expect("created_at");
        let event_path = root.join(event::event_relative_path(
            &note.front_matter.id,
            created_at,
        ));
        let event = event::parse_event(&fs::read_to_string(event_path).expect("read event"))
            .expect("parse event");
        assert_eq!(event.classified, None);
    }
}
