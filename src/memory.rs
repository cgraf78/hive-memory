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
}

impl Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CollisionLimit => write!(f, "could not find a unique memory id"),
            Self::Note(message) => write!(f, "failed to write note: {message}"),
            Self::Event(message) => write!(f, "failed to write event: {message}"),
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
