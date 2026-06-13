//! Markdown note front matter and inbox writing.
//!
//! Notes are the durable human-readable write format. The body stays plain
//! Markdown; TOML front matter carries enough structured context for search,
//! context assembly, audit, and future compaction without scraping prose.

use crate::id::WriteIdContext;
use crate::write::{self, AtomicWriteOptions};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::{self, Display};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// Markdown note front-matter schema version supported by this build.
pub const NOTE_SCHEMA_VERSION: u32 = 1;

/// Kind of inbox note being written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    /// User or agent explicitly asked `hm` to remember something durable.
    Remember,
    /// General note material that may still be useful for context.
    Note,
}

/// Durable classification of a memory, set explicitly by the writer.
///
/// Drives session-start inject selection: preferences are always-on, project
/// facts inject in their own project, incidents and references are search-only.
/// When absent (legacy records, or writers that don't set it), the inject
/// classifier falls back to a content heuristic. This lives with the note schema
/// because it is persisted metadata, mirrored into the event sidecar and index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryKind {
    /// Durable behavioral guidance; inject in every session.
    Preference,
    /// Fact about one project or system; inject only in that project.
    ProjectFact,
    /// Operational event or fix; search-only, never auto-injected at startup.
    Incident,
    /// Pointer or lookup fact; search-only.
    Reference,
}

/// Return the stable persisted label for a memory kind.
///
/// The string vocabulary is part of the note/event schema and CLI contract, so
/// it lives with `MemoryKind` instead of being duplicated by each caller that
/// needs to render or prompt with labels.
pub fn kind_label(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Preference => "preference",
        MemoryKind::ProjectFact => "project-fact",
        MemoryKind::Incident => "incident",
        MemoryKind::Reference => "reference",
    }
}

/// Who issued a persisted kind verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClassifierSource {
    /// Automated LLM classification pass.
    Llm,
    /// Explicit human verdict (`hm retag`); never overridden by the LLM pass.
    Manual,
}

/// Provenance for a persisted kind verdict.
///
/// Pending LLM review is derived from this field's absence instead of a local
/// queue. That keeps synced notes authoritative: any machine can classify a
/// record, and other machines see the settled verdict without coordinating
/// ephemeral worker state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifiedBy {
    /// Verdict origin.
    pub source: ClassifierSource,
    /// Backend label such as `claude`; diagnostics only, never control flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// RFC3339 timestamp of the verdict.
    pub at: String,
    /// Prompt/policy version. LLM verdicts older than the current version can
    /// be re-reviewed; manual verdicts are version-exempt.
    pub verdict_version: u32,
    /// Model-reported confidence for LLM verdicts. `None` for manual verdicts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
}

/// Confidence assigned to a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Confidence {
    /// Useful but uncertain; render/search should treat as weak evidence.
    Low,
    /// Normal confidence for ordinary agent-observed facts.
    Medium,
    /// Strong confidence, usually direct user instruction or verified fact.
    High,
}

/// TOML front matter for a Markdown note.
///
/// The required fields mirror the v1 spec. Optional fields are omitted when
/// absent, and `audience` is omitted when empty so non-agent-private notes stay
/// visually clean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteFrontMatter {
    /// Note schema version.
    pub schema_version: u32,
    /// Entry type discriminator. V1 requires `note`.
    #[serde(rename = "type")]
    pub note_type: String,
    /// Whether this note came from a remember workflow or general note flow.
    pub entry_kind: EntryKind,
    /// Stable write id, also used as the filename stem.
    pub id: String,
    /// Stable store manifest id at write time.
    pub store_id: String,
    /// Store alias/name at write time for readable browsing.
    pub store_name: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
    /// Agent identity that wrote the note.
    pub agent_id: String,
    /// Host identity that wrote the note.
    pub host_id: String,
    /// Memory scope, such as `global`, `project`, or `agent-private`.
    pub scope: String,
    /// Writer confidence used by later rendering and compaction.
    pub confidence: Confidence,
    /// Optional user identity when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Optional agent session id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional project identity for project-scoped notes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Optional short subject for grouping/search.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Optional tags for lightweight filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Optional source category, such as hook, command, or import.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    /// Optional source locator or opaque reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// Optional paired JSON event id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_event_id: Option<String>,
    /// Optional RFC3339 expiration timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Optional RFC3339 timestamp when this fact starts being valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    /// Optional RFC3339 timestamp when this fact stops being current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    /// Explicit records superseded by this note.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    /// Optional explicit memory kind driving inject selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MemoryKind>,
    /// Optional provenance for the persisted `kind` verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classified: Option<ClassifiedBy>,
    /// Explicit allowed agents for `agent-private` notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audience: Vec<String>,
}

/// Parsed Markdown note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownNote {
    /// Parsed structured metadata from the TOML front matter.
    pub front_matter: NoteFrontMatter,
    /// Human-readable Markdown body.
    pub body: String,
}

/// Input for writing a Markdown note.
///
/// This type intentionally does not include `id`: note ids are generated by the
/// write loop so filename collision retries update both the path and front
/// matter together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteWriteInput {
    /// Whether this note came from a remember workflow or general note flow.
    pub entry_kind: EntryKind,
    /// Stable store manifest id at write time.
    pub store_id: String,
    /// Store alias/name at write time for readable browsing.
    pub store_name: String,
    /// Timestamp used for both front matter and canonical day partition.
    pub created_at: OffsetDateTime,
    /// Agent identity that wrote the note.
    pub agent_id: String,
    /// Host identity that wrote the note.
    pub host_id: String,
    /// Memory scope, such as `global`, `project`, or `agent-private`.
    pub scope: String,
    /// Writer confidence used by later rendering and compaction.
    pub confidence: Confidence,
    /// Human-readable Markdown body.
    pub body: String,
    /// Optional user identity when known.
    pub user_id: Option<String>,
    /// Optional agent session id.
    pub session_id: Option<String>,
    /// Optional project identity for project-scoped notes.
    pub project_id: Option<String>,
    /// Optional short subject for grouping/search.
    pub subject: Option<String>,
    /// Optional tags for lightweight filtering.
    pub tags: Vec<String>,
    /// Optional source category, such as hook, command, or import.
    pub source_kind: Option<String>,
    /// Optional source locator or opaque reference.
    pub source_ref: Option<String>,
    /// Optional paired JSON event id.
    pub related_event_id: Option<String>,
    /// Optional RFC3339 expiration timestamp.
    pub expires_at: Option<String>,
    /// Optional RFC3339 timestamp when this fact starts being valid.
    pub valid_from: Option<String>,
    /// Optional RFC3339 timestamp when this fact stops being current.
    pub valid_to: Option<String>,
    /// Explicit records superseded by this note.
    pub supersedes: Vec<String>,
    /// Optional explicit memory kind driving inject selection.
    pub kind: Option<MemoryKind>,
    /// Optional provenance for the persisted `kind` verdict.
    pub classified: Option<ClassifiedBy>,
    /// Explicit allowed agents for `agent-private` notes.
    pub audience: Vec<String>,
}

/// Result of writing a Markdown note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteWriteResult {
    /// Stable write id used as the filename stem.
    pub id: String,
    /// Final path of the Markdown note.
    pub path: PathBuf,
}

/// Note parse or write failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteError {
    /// Input does not begin with a TOML front matter block.
    MissingFrontMatter,
    /// TOML front matter could not be parsed or serialized.
    InvalidFrontMatter(String),
    /// Required front matter field was present but empty.
    MissingRequiredField(&'static str),
    /// Note schema is newer or otherwise unsupported by this build.
    UnsupportedSchema(u32),
    /// Front matter `type` was not the required `note` discriminator.
    WrongType(String),
    /// Timestamp field was not valid RFC3339.
    InvalidTimestamp {
        /// Timestamp field name.
        field: &'static str,
        /// Invalid timestamp value.
        value: String,
    },
    /// `valid_from` must be earlier than `valid_to` when both are present.
    InvalidValidityWindow {
        /// Parsed `valid_from` value.
        valid_from: String,
        /// Parsed `valid_to` value.
        valid_to: String,
    },
    /// `agent-private` notes must declare which agents may read them.
    MissingAudienceForAgentPrivate,
    /// Filesystem publish failed.
    Write(String),
}

impl Display for NoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFrontMatter => write!(f, "note is missing TOML front matter"),
            Self::InvalidFrontMatter(message) => write!(f, "invalid note front matter: {message}"),
            Self::MissingRequiredField(field) => {
                write!(f, "note front matter field {field} is required")
            }
            Self::UnsupportedSchema(version) => {
                write!(f, "unsupported note schema_version: {version}")
            }
            Self::WrongType(note_type) => {
                write!(f, "front matter type must be note, got {note_type}")
            }
            Self::InvalidTimestamp { field, value } => {
                write!(f, "note front matter field {field} is not RFC3339: {value}")
            }
            Self::InvalidValidityWindow {
                valid_from,
                valid_to,
            } => write!(
                f,
                "note front matter valid_from must be earlier than valid_to: {valid_from} >= {valid_to}"
            ),
            Self::MissingAudienceForAgentPrivate => {
                write!(f, "agent-private notes require an explicit audience")
            }
            Self::Write(message) => write!(f, "failed to write note: {message}"),
        }
    }
}

impl Error for NoteError {}

impl NoteWriteInput {
    /// Build front matter for a concrete note id.
    ///
    /// The id is provided by the write loop so collision retries can regenerate
    /// both the filename and the front matter together.
    pub fn front_matter(&self, id: String) -> Result<NoteFrontMatter, NoteError> {
        let front_matter = NoteFrontMatter {
            schema_version: NOTE_SCHEMA_VERSION,
            note_type: "note".to_owned(),
            entry_kind: self.entry_kind,
            id,
            store_id: self.store_id.clone(),
            store_name: self.store_name.clone(),
            created_at: rfc3339(self.created_at),
            agent_id: self.agent_id.clone(),
            host_id: self.host_id.clone(),
            scope: self.scope.clone(),
            confidence: self.confidence,
            user_id: self.user_id.clone(),
            session_id: self.session_id.clone(),
            project_id: self.project_id.clone(),
            subject: self.subject.clone(),
            tags: self.tags.clone(),
            source_kind: self.source_kind.clone(),
            source_ref: self.source_ref.clone(),
            related_event_id: self.related_event_id.clone(),
            expires_at: self.expires_at.clone(),
            valid_from: self.valid_from.clone(),
            valid_to: self.valid_to.clone(),
            supersedes: self.supersedes.clone(),
            kind: self.kind,
            classified: self.classified.clone(),
            audience: if self.scope == "agent-private" {
                self.audience.clone()
            } else {
                Vec::new()
            },
        };
        validate_front_matter(&front_matter)?;
        Ok(front_matter)
    }
}

/// Render a Markdown note with TOML front matter.
///
/// Rendering validates the front matter before producing bytes so callers do
/// not accidentally persist malformed notes that future search/context commands
/// would have to special-case.
pub fn render_note(note: &MarkdownNote) -> Result<String, NoteError> {
    validate_front_matter(&note.front_matter)?;
    let front_matter = toml::to_string_pretty(&note.front_matter)
        .map_err(|err| NoteError::InvalidFrontMatter(err.to_string()))?;
    Ok(format!("+++\n{front_matter}+++\n\n{}", note.body))
}

/// Parse a Markdown note with TOML front matter.
///
/// The parser enforces the v1 required-field contract at the boundary where
/// search, context assembly, and repair tools will ingest on-disk notes.
pub fn parse_note(input: &str) -> Result<MarkdownNote, NoteError> {
    let Some(rest) = input.strip_prefix("+++\n") else {
        return Err(NoteError::MissingFrontMatter);
    };
    let (front_matter, body) = if let Some((front_matter, body)) = rest.split_once("\n+++\n\n") {
        (front_matter, body)
    } else {
        let Some((front_matter, body)) = rest.split_once("\n+++\n") else {
            return Err(NoteError::MissingFrontMatter);
        };
        (front_matter, body)
    };
    let front_matter: NoteFrontMatter = toml::from_str(front_matter)
        .map_err(|err| NoteError::InvalidFrontMatter(err.to_string()))?;
    validate_front_matter(&front_matter)?;
    Ok(MarkdownNote {
        front_matter,
        body: body.to_owned(),
    })
}

/// Write a note into `<store-root>/inbox/notes/YYYY/MM/DD/<id>.md`.
///
/// This is the production entry point for durable note creation. It chooses a
/// sortable id from host/agent context and retries collisions without mutating
/// existing notes.
pub fn write_note(
    store_root: &Path,
    input: &NoteWriteInput,
    options: &AtomicWriteOptions,
) -> Result<NoteWriteResult, NoteError> {
    write_note_with_id_generator(store_root, input, options, || {
        crate::id::new_write_id(&WriteIdContext {
            host_id: input.host_id.clone(),
            agent_id: input.agent_id.clone(),
        })
    })
}

/// Write a note with caller-supplied id generation.
///
/// Production code should use [`write_note`]. Tests and import tools can use
/// this entry point to reproduce collision behavior deterministically while
/// preserving the same validation and atomic publish path.
pub fn write_note_with_id_generator<F>(
    store_root: &Path,
    input: &NoteWriteInput,
    options: &AtomicWriteOptions,
    mut next_id: F,
) -> Result<NoteWriteResult, NoteError>
where
    F: FnMut() -> String,
{
    let parent = note_day_dir(store_root, input.created_at);
    let mut last_error = None;
    for _ in 0..options.max_attempts.max(1) {
        let id = next_id();
        let front_matter = input.front_matter(id.clone())?;
        let rendered = render_note(&MarkdownNote {
            front_matter,
            body: input.body.clone(),
        })?;
        let path = parent.join(format!("{id}.md"));
        if path.exists() {
            continue;
        }
        match write::write_atomic_create_new(&path, rendered.as_bytes(), options) {
            Ok(_) => return Ok(NoteWriteResult { id, path }),
            Err(write::AtomicWriteError::Io {
                action: "install final file",
                ..
            })
            | Err(write::AtomicWriteError::TempExists { .. }) => {
                last_error = Some("could not find a unique note path".to_owned());
                continue;
            }
            Err(err) => return Err(NoteError::Write(err.to_string())),
        }
    }

    Err(NoteError::Write(last_error.unwrap_or_else(|| {
        "could not find a unique note path".to_owned()
    })))
}

/// Return the store-relative canonical Markdown note path.
///
/// JSON sidecars and index records use this exact relative path to pair machine
/// metadata back to the canonical human-readable note without embedding a
/// host-specific absolute path.
pub fn note_relative_path(id: &str, created_at: OffsetDateTime) -> PathBuf {
    note_day_relative_dir(created_at).join(format!("{id}.md"))
}

fn validate_front_matter(front_matter: &NoteFrontMatter) -> Result<(), NoteError> {
    if front_matter.schema_version != NOTE_SCHEMA_VERSION {
        return Err(NoteError::UnsupportedSchema(front_matter.schema_version));
    }
    if front_matter.note_type != "note" {
        return Err(NoteError::WrongType(front_matter.note_type.clone()));
    }
    require_non_empty("id", &front_matter.id)?;
    require_non_empty("store_id", &front_matter.store_id)?;
    require_non_empty("store_name", &front_matter.store_name)?;
    require_non_empty("created_at", &front_matter.created_at)?;
    require_non_empty("agent_id", &front_matter.agent_id)?;
    require_non_empty("host_id", &front_matter.host_id)?;
    require_non_empty("scope", &front_matter.scope)?;
    validate_rfc3339("created_at", &front_matter.created_at)?;
    if let Some(expires_at) = &front_matter.expires_at {
        validate_rfc3339("expires_at", expires_at)?;
    }
    validate_validity_window(
        front_matter.valid_from.as_deref(),
        front_matter.valid_to.as_deref(),
    )?;
    if let Some(classified) = &front_matter.classified {
        validate_rfc3339("classified.at", &classified.at)?;
    }
    if front_matter.scope == "agent-private" && front_matter.audience.is_empty() {
        return Err(NoteError::MissingAudienceForAgentPrivate);
    }
    Ok(())
}

fn require_non_empty(field: &'static str, value: &str) -> Result<(), NoteError> {
    if value.trim().is_empty() {
        Err(NoteError::MissingRequiredField(field))
    } else {
        Ok(())
    }
}

fn validate_rfc3339(field: &'static str, value: &str) -> Result<(), NoteError> {
    parse_rfc3339(field, value).map(|_| ())
}

fn parse_rfc3339(field: &'static str, value: &str) -> Result<OffsetDateTime, NoteError> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).map_err(|_| {
        NoteError::InvalidTimestamp {
            field,
            value: value.to_owned(),
        }
    })
}

fn validate_validity_window(
    valid_from: Option<&str>,
    valid_to: Option<&str>,
) -> Result<(), NoteError> {
    let Some(valid_from_value) = valid_from else {
        if let Some(valid_to_value) = valid_to {
            parse_rfc3339("valid_to", valid_to_value)?;
        }
        return Ok(());
    };
    let valid_from_time = parse_rfc3339("valid_from", valid_from_value)?;
    let Some(valid_to_value) = valid_to else {
        return Ok(());
    };
    let valid_to_time = parse_rfc3339("valid_to", valid_to_value)?;
    if valid_from_time >= valid_to_time {
        return Err(NoteError::InvalidValidityWindow {
            valid_from: valid_from_value.to_owned(),
            valid_to: valid_to_value.to_owned(),
        });
    }
    Ok(())
}

fn note_day_dir(store_root: &Path, created_at: OffsetDateTime) -> PathBuf {
    store_root.join(note_day_relative_dir(created_at))
}

fn note_day_relative_dir(created_at: OffsetDateTime) -> PathBuf {
    PathBuf::from("inbox/notes")
        .join(format!("{:04}", created_at.year()))
        .join(format!("{:02}", u8::from(created_at.month())))
        .join(format!("{:02}", created_at.day()))
}

fn rfc3339(timestamp: OffsetDateTime) -> String {
    timestamp
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting is infallible for UTC timestamps")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::FsyncPolicy;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn timestamp() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_778_946_153)
            .expect("timestamp")
            .replace_nanosecond(184_921_000)
            .expect("nanos")
    }

    fn input() -> NoteWriteInput {
        NoteWriteInput {
            entry_kind: EntryKind::Remember,
            store_id: "018f5f57-bd9b-7d33-9e21-1f44f0c5a013".to_owned(),
            store_name: "personal".to_owned(),
            created_at: timestamp(),
            agent_id: "codex".to_owned(),
            host_id: "taylor".to_owned(),
            scope: "global".to_owned(),
            confidence: Confidence::High,
            body: "Chris prefers TOML config.".to_owned(),
            user_id: Some("chris".to_owned()),
            session_id: None,
            project_id: None,
            subject: Some("workflow.preference".to_owned()),
            tags: vec!["preference".to_owned(), "config".to_owned()],
            source_kind: None,
            source_ref: None,
            related_event_id: None,
            expires_at: None,
            valid_from: None,
            valid_to: None,
            supersedes: Vec::new(),
            kind: None,
            classified: None,
            audience: Vec::new(),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hive-memory-note-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn note_round_trips_toml_front_matter() {
        let mut input = input();
        input.valid_from = Some("2026-06-01T00:00:00Z".to_owned());
        input.valid_to = Some("2026-07-01T00:00:00Z".to_owned());
        input.supersedes = vec!["old-note-id".to_owned()];
        let front_matter = input
            .front_matter("note-id".to_owned())
            .expect("front matter");
        let note = MarkdownNote {
            front_matter,
            body: "Human-readable memory text.".to_owned(),
        };

        let rendered = render_note(&note).expect("render note");
        let parsed = parse_note(&rendered).expect("parse note");

        assert_eq!(parsed, note);
        assert_eq!(
            parsed.front_matter.valid_to.as_deref(),
            Some("2026-07-01T00:00:00Z")
        );
        assert_eq!(
            parsed.front_matter.supersedes,
            vec!["old-note-id".to_owned()]
        );
        assert!(rendered.starts_with("+++\nschema_version = 1\n"));
        assert!(!rendered.contains("audience ="));
    }

    #[test]
    fn note_body_preserves_leading_newline() {
        let front_matter = input()
            .front_matter("note-id".to_owned())
            .expect("front matter");
        let note = MarkdownNote {
            front_matter,
            body: "\nIndented body.".to_owned(),
        };

        let parsed = parse_note(&render_note(&note).expect("render note")).expect("parse note");

        assert_eq!(parsed.body, "\nIndented body.");
    }

    #[test]
    fn non_private_note_omits_accidental_audience() {
        let mut input = input();
        input.audience = vec!["codex".to_owned()];

        let front_matter = input
            .front_matter("note-id".to_owned())
            .expect("front matter");

        assert!(front_matter.audience.is_empty());
    }

    #[test]
    fn rejects_agent_private_note_without_audience() {
        let mut input = input();
        input.scope = "agent-private".to_owned();

        let err = input
            .front_matter("note-id".to_owned())
            .expect_err("front matter rejected");

        assert_eq!(err, NoteError::MissingAudienceForAgentPrivate);
    }

    #[test]
    fn rejects_empty_required_front_matter_field() {
        let mut front_matter = input()
            .front_matter("note-id".to_owned())
            .expect("front matter");
        front_matter.store_id.clear();
        let note = MarkdownNote {
            front_matter,
            body: "body".to_owned(),
        };

        let err = render_note(&note).expect_err("render rejected");

        assert_eq!(err, NoteError::MissingRequiredField("store_id"));
    }

    #[test]
    fn rejects_invalid_timestamp_field() {
        let mut front_matter = input()
            .front_matter("note-id".to_owned())
            .expect("front matter");
        front_matter.created_at = "not-a-time".to_owned();
        let note = MarkdownNote {
            front_matter,
            body: "body".to_owned(),
        };

        let err = render_note(&note).expect_err("render rejected");

        assert_eq!(
            err,
            NoteError::InvalidTimestamp {
                field: "created_at",
                value: "not-a-time".to_owned()
            }
        );
    }

    #[test]
    fn rejects_inverted_validity_window() {
        let mut front_matter = input()
            .front_matter("note-id".to_owned())
            .expect("front matter");
        front_matter.valid_from = Some("2030-01-01T00:00:00Z".to_owned());
        front_matter.valid_to = Some("2020-01-01T00:00:00Z".to_owned());
        let note = MarkdownNote {
            front_matter,
            body: "body".to_owned(),
        };

        let err = render_note(&note).expect_err("render rejected");

        assert_eq!(
            err,
            NoteError::InvalidValidityWindow {
                valid_from: "2030-01-01T00:00:00Z".to_owned(),
                valid_to: "2020-01-01T00:00:00Z".to_owned(),
            }
        );
    }

    #[test]
    fn classified_provenance_round_trips() {
        let mut input = input();
        input.classified = Some(ClassifiedBy {
            source: ClassifierSource::Llm,
            backend: Some("claude".to_owned()),
            at: "2026-06-12T00:00:00Z".to_owned(),
            verdict_version: 1,
            confidence: Some("high".to_owned()),
        });

        let front_matter = input
            .front_matter("note-id".to_owned())
            .expect("front matter");
        let note = MarkdownNote {
            front_matter,
            body: "body".to_owned(),
        };
        let parsed = parse_note(&render_note(&note).expect("render note")).expect("parse note");

        let classified = parsed.front_matter.classified.expect("classified");
        assert_eq!(classified.source, ClassifierSource::Llm);
        assert_eq!(classified.backend.as_deref(), Some("claude"));
        assert_eq!(classified.verdict_version, 1);
    }

    #[test]
    fn rejects_invalid_classified_timestamp() {
        let mut front_matter = input()
            .front_matter("note-id".to_owned())
            .expect("front matter");
        front_matter.classified = Some(ClassifiedBy {
            source: ClassifierSource::Llm,
            backend: Some("claude".to_owned()),
            at: "not-a-time".to_owned(),
            verdict_version: 1,
            confidence: Some("high".to_owned()),
        });
        let note = MarkdownNote {
            front_matter,
            body: "body".to_owned(),
        };

        let err = render_note(&note).expect_err("render rejected");

        assert_eq!(
            err,
            NoteError::InvalidTimestamp {
                field: "classified.at",
                value: "not-a-time".to_owned()
            }
        );
    }

    #[test]
    fn writes_note_to_canonical_day_path() {
        let root = temp_dir("write");
        let options = AtomicWriteOptions {
            fsync: FsyncPolicy::Never,
            ..AtomicWriteOptions::default()
        };

        let result =
            write_note_with_id_generator(&root, &input(), &options, || "note-id".to_owned())
                .expect("write note");

        assert_eq!(result.path, root.join("inbox/notes/2026/05/16/note-id.md"));
        let parsed =
            parse_note(&fs::read_to_string(&result.path).expect("read note")).expect("parse note");
        assert_eq!(parsed.front_matter.id, "note-id");
        assert_eq!(parsed.body, "Chris prefers TOML config.");
    }

    #[test]
    fn write_note_retries_id_collision() {
        let root = temp_dir("collision");
        let parent = root.join("inbox/notes/2026/05/16");
        fs::create_dir_all(&parent).expect("create parent");
        fs::write(parent.join("note-id.md"), "existing").expect("precreate note");
        let mut ids = ["note-id", "note-id-2"].into_iter();
        let options = AtomicWriteOptions {
            fsync: FsyncPolicy::Never,
            max_attempts: 5,
            skip_parent_fsync: false,
        };

        let result = write_note_with_id_generator(&root, &input(), &options, || {
            ids.next().expect("id").to_owned()
        })
        .expect("write note");

        assert_eq!(result.id, "note-id-2");
        assert!(result.path.ends_with("note-id-2.md"));
    }
}
