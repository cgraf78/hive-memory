//! JSON event sidecars for machine-readable memory operations.
//!
//! Markdown notes are the canonical human record. Events are the structured
//! view used for indexing, dedupe, audit, compaction input, and future machine
//! integrations. Keeping this module separate from `note` prevents JSON policy
//! from leaking into the prose format while still allowing paired records to
//! share one id.

use crate::note::Confidence;
use crate::path as memory_path;
use crate::write;
use crate::write::AtomicWriteOptions;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::{self, Display};
use std::path::{Component, Path, PathBuf};
use time::OffsetDateTime;

/// Event schema version supported by this build.
pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// Recommended v1 event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventType {
    /// A fact, preference, or context worth remembering.
    #[serde(rename = "memory.observation")]
    Observation,
    /// Corrects or supersedes an earlier memory record.
    #[serde(rename = "memory.correction")]
    Correction,
    /// Durable todo or follow-up.
    #[serde(rename = "memory.task")]
    Task,
    /// Explicit decision made by the user or project.
    #[serde(rename = "memory.decision")]
    Decision,
    /// Imported legacy memory entry.
    #[serde(rename = "memory.import")]
    Import,
    /// Raw inbox note promoted into curated memory.
    #[serde(rename = "memory.promotion")]
    Promotion,
    /// Summary, schema migration, or compaction metadata.
    #[serde(rename = "memory.compaction")]
    Compaction,
}

/// Optional source pointer for an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSource {
    /// Source category, such as `session`, `hook`, or `import`.
    pub kind: String,
    /// Optional source locator or opaque reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
}

/// Machine-readable event sidecar.
///
/// The shape intentionally mirrors note front matter where fields overlap so
/// search/context can prefer event metadata without translating between two
/// competing vocabularies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEvent {
    /// Event schema version.
    pub schema_version: u32,
    /// Event type discriminator.
    #[serde(rename = "type")]
    pub event_type: EventType,
    /// Stable write id, also used as the filename stem.
    pub id: String,
    /// Stable store manifest id at write time.
    pub store_id: String,
    /// Store alias/name at write time for readable browsing.
    pub store_name: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
    /// Agent identity that wrote the event.
    pub agent_id: String,
    /// Host identity that wrote the event.
    pub host_id: String,
    /// Optional user identity when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Optional agent session id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Memory scope, such as `global`, `project`, or `agent-private`.
    pub scope: String,
    /// Optional project identity for project-scoped events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Optional short subject for grouping/search.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Optional tags for lightweight filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Writer confidence used by later rendering and compaction.
    pub confidence: Confidence,
    /// Optional RFC3339 timestamp when this fact starts being valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    /// Optional RFC3339 timestamp when this fact stops being current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    /// Explicit records superseded by this event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    /// Optional explicit memory kind, mirrored from the note. The index prefers
    /// the event copy, so it must be carried here too, not only on the note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<crate::note::MemoryKind>,
    /// Optional provenance for the persisted `kind` verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classified: Option<crate::note::ClassifiedBy>,
    /// Explicit allowed agents for `agent-private` events.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audience: Vec<String>,
    /// Machine-readable copy of the note body for indexing and dedupe.
    pub body: String,
    /// Store-relative path to the paired Markdown note when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_path: Option<String>,
    /// Optional event source pointer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<EventSource>,
}

/// Result of writing a JSON event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventWriteResult {
    /// Stable write id used as the filename stem.
    pub id: String,
    /// Final path of the JSON event.
    pub path: PathBuf,
}

/// Event parse, validation, or write failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventError {
    /// JSON could not be parsed or serialized.
    InvalidJson(String),
    /// Required event field was present but empty.
    MissingRequiredField(&'static str),
    /// Event schema is newer or otherwise unsupported by this build.
    UnsupportedSchema(u32),
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
    /// `agent-private` events must declare which agents may read them.
    MissingAudienceForAgentPrivate,
    /// Store-relative path field was absolute or not normalized for v1.
    InvalidRelativePath {
        /// Path field name.
        field: &'static str,
        /// Invalid path value.
        value: String,
    },
    /// Filesystem publish failed.
    Write(String),
}

impl Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson(message) => write!(f, "invalid event JSON: {message}"),
            Self::MissingRequiredField(field) => {
                write!(f, "event field {field} is required")
            }
            Self::UnsupportedSchema(version) => {
                write!(f, "unsupported event schema_version: {version}")
            }
            Self::InvalidTimestamp { field, value } => {
                write!(f, "event field {field} is not RFC3339: {value}")
            }
            Self::InvalidValidityWindow {
                valid_from,
                valid_to,
            } => write!(
                f,
                "event valid_from must be earlier than valid_to: {valid_from} >= {valid_to}"
            ),
            Self::MissingAudienceForAgentPrivate => {
                write!(f, "agent-private events require an explicit audience")
            }
            Self::InvalidRelativePath { field, value } => {
                write!(
                    f,
                    "event field {field} must be a normalized relative path: {value}"
                )
            }
            Self::Write(message) => write!(f, "failed to write event: {message}"),
        }
    }
}

impl Error for EventError {}

impl MemoryEvent {
    /// Build a v1 memory observation event.
    ///
    /// This constructor covers the common paired note/event write path. It
    /// still returns a normal struct so importers and future commands can build
    /// other event types explicitly when they need more control.
    pub fn observation(input: EventObservationInput) -> Result<Self, EventError> {
        let audience = if input.scope == "agent-private" {
            input.audience
        } else {
            Vec::new()
        };
        let event = Self {
            schema_version: EVENT_SCHEMA_VERSION,
            event_type: EventType::Observation,
            id: input.id,
            store_id: input.store_id,
            store_name: input.store_name,
            created_at: rfc3339(input.created_at),
            agent_id: input.agent_id,
            host_id: input.host_id,
            user_id: input.user_id,
            session_id: input.session_id,
            scope: input.scope,
            project_id: input.project_id,
            subject: input.subject,
            tags: input.tags,
            confidence: input.confidence,
            valid_from: input.valid_from,
            valid_to: input.valid_to,
            supersedes: input.supersedes,
            kind: input.kind,
            classified: input.classified,
            audience,
            body: input.body,
            note_path: input.note_path.map(path_to_event_string),
            source: input.source,
        };
        validate_event(&event)?;
        Ok(event)
    }
}

/// Input for building a memory observation event.
///
/// The id is caller-supplied because paired notes and events must share one
/// logical record id. The note writer is normally responsible for generating it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventObservationInput {
    /// Stable write id shared with the paired Markdown note.
    pub id: String,
    /// Stable store manifest id at write time.
    pub store_id: String,
    /// Store alias/name at write time for readable browsing.
    pub store_name: String,
    /// Timestamp used for both JSON metadata and canonical day partition.
    pub created_at: OffsetDateTime,
    /// Agent identity that wrote the event.
    pub agent_id: String,
    /// Host identity that wrote the event.
    pub host_id: String,
    /// Optional user identity when known.
    pub user_id: Option<String>,
    /// Optional agent session id.
    pub session_id: Option<String>,
    /// Memory scope, such as `global`, `project`, or `agent-private`.
    pub scope: String,
    /// Optional project identity for project-scoped events.
    pub project_id: Option<String>,
    /// Optional short subject for grouping/search.
    pub subject: Option<String>,
    /// Optional tags for lightweight filtering.
    pub tags: Vec<String>,
    /// Writer confidence used by later rendering and compaction.
    pub confidence: Confidence,
    /// Optional RFC3339 timestamp when this fact starts being valid.
    pub valid_from: Option<String>,
    /// Optional RFC3339 timestamp when this fact stops being current.
    pub valid_to: Option<String>,
    /// Explicit records superseded by this event.
    pub supersedes: Vec<String>,
    /// Optional explicit memory kind, mirrored from the note.
    pub kind: Option<crate::note::MemoryKind>,
    /// Optional provenance for the persisted `kind` verdict.
    pub classified: Option<crate::note::ClassifiedBy>,
    /// Explicit allowed agents for `agent-private` events.
    pub audience: Vec<String>,
    /// Machine-readable copy of the note body for indexing and dedupe.
    pub body: String,
    /// Store-relative path to the paired Markdown note when present.
    pub note_path: Option<PathBuf>,
    /// Optional event source pointer.
    pub source: Option<EventSource>,
}

/// Render a JSON event with stable pretty formatting.
///
/// Rendering validates first so malformed events do not land on disk and later
/// force search/context code to carry repair branches.
pub fn render_event(event: &MemoryEvent) -> Result<String, EventError> {
    validate_event(event)?;
    serde_json::to_string_pretty(event)
        .map(|json| format!("{json}\n"))
        .map_err(|err| EventError::InvalidJson(err.to_string()))
}

/// Parse and validate a JSON event.
pub fn parse_event(input: &str) -> Result<MemoryEvent, EventError> {
    let mut event: MemoryEvent =
        serde_json::from_str(input).map_err(|err| EventError::InvalidJson(err.to_string()))?;
    // Mirror note parsing: a blank validity timestamp means absent, not an
    // RFC3339 value to validate.
    crate::note::normalize_blank_timestamp(&mut event.valid_from);
    crate::note::normalize_blank_timestamp(&mut event.valid_to);
    validate_event(&event)?;
    Ok(event)
}

/// Write an event into `<store-root>/inbox/events/YYYY/MM/DD/<id>.json`.
///
/// Event ids are caller-owned so paired notes and events can share an id. The
/// write path still uses create-if-absent publishing to avoid overwriting a
/// concurrent event with the same id.
pub fn write_event(
    store_root: &Path,
    event: &MemoryEvent,
    options: &AtomicWriteOptions,
) -> Result<EventWriteResult, EventError> {
    validate_event(event)?;
    let created_at = parse_rfc3339("created_at", &event.created_at)?;
    let path = store_root.join(event_relative_path(&event.id, created_at));
    let rendered = render_event(event)?;
    match write::write_atomic_create_new(&path, rendered.as_bytes(), options) {
        Ok(_) => Ok(EventWriteResult {
            id: event.id.clone(),
            path,
        }),
        Err(err) => Err(EventError::Write(err.to_string())),
    }
}

/// Return the store-relative canonical JSON event path.
pub fn event_relative_path(id: &str, created_at: OffsetDateTime) -> PathBuf {
    event_day_relative_dir(created_at).join(format!("{id}.json"))
}

fn validate_event(event: &MemoryEvent) -> Result<(), EventError> {
    if event.schema_version != EVENT_SCHEMA_VERSION {
        return Err(EventError::UnsupportedSchema(event.schema_version));
    }
    require_non_empty("id", &event.id)?;
    require_non_empty("store_id", &event.store_id)?;
    require_non_empty("store_name", &event.store_name)?;
    require_non_empty("created_at", &event.created_at)?;
    require_non_empty("agent_id", &event.agent_id)?;
    require_non_empty("host_id", &event.host_id)?;
    require_non_empty("scope", &event.scope)?;
    require_non_empty("body", &event.body)?;
    parse_rfc3339("created_at", &event.created_at)?;
    validate_validity_window(event.valid_from.as_deref(), event.valid_to.as_deref())?;
    if let Some(classified) = &event.classified {
        parse_rfc3339("classified.at", &classified.at)?;
    }
    if event.scope == "agent-private" && event.audience.is_empty() {
        return Err(EventError::MissingAudienceForAgentPrivate);
    }
    if let Some(note_path) = &event.note_path {
        validate_relative_path("note_path", note_path)?;
    }
    if let Some(source) = &event.source {
        require_non_empty("source.kind", &source.kind)?;
    }
    Ok(())
}

fn require_non_empty(field: &'static str, value: &str) -> Result<(), EventError> {
    if value.trim().is_empty() {
        Err(EventError::MissingRequiredField(field))
    } else {
        Ok(())
    }
}

fn parse_rfc3339(field: &'static str, value: &str) -> Result<OffsetDateTime, EventError> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).map_err(|_| {
        EventError::InvalidTimestamp {
            field,
            value: value.to_owned(),
        }
    })
}

fn validate_validity_window(
    valid_from: Option<&str>,
    valid_to: Option<&str>,
) -> Result<(), EventError> {
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
        return Err(EventError::InvalidValidityWindow {
            valid_from: valid_from_value.to_owned(),
            valid_to: valid_to_value.to_owned(),
        });
    }
    Ok(())
}

fn validate_relative_path(field: &'static str, value: &str) -> Result<(), EventError> {
    if value.trim().is_empty() || value.contains('\\') {
        return Err(invalid_relative_path(field, value));
    }

    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir
                    | Component::ParentDir
                    | Component::Prefix(_)
                    | Component::RootDir
            )
        })
    {
        return Err(invalid_relative_path(field, value));
    }
    // Event JSONL is the long-lived audit surface. Reject alternate serialized
    // spellings here so readers, search indexes, and generated context all see
    // one canonical path form instead of normalizing divergent history forever.
    if memory_path::relative_str(value, memory_path::PathCase::Sensitive) != value {
        return Err(invalid_relative_path(field, value));
    }
    Ok(())
}

fn invalid_relative_path(field: &'static str, value: &str) -> EventError {
    EventError::InvalidRelativePath {
        field,
        value: value.to_owned(),
    }
}

fn event_day_relative_dir(created_at: OffsetDateTime) -> PathBuf {
    PathBuf::from("inbox/events")
        .join(format!("{:04}", created_at.year()))
        .join(format!("{:02}", u8::from(created_at.month())))
        .join(format!("{:02}", created_at.day()))
}

fn path_to_event_string(path: PathBuf) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn rfc3339(timestamp: OffsetDateTime) -> String {
    timestamp
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting is infallible for UTC timestamps")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note;
    use crate::write::FsyncPolicy;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn timestamp() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_778_946_153)
            .expect("timestamp")
            .replace_nanosecond(184_921_000)
            .expect("nanos")
    }

    fn input() -> EventObservationInput {
        EventObservationInput {
            id: "event-id".to_owned(),
            store_id: "018f5f57-bd9b-7d33-9e21-1f44f0c5a013".to_owned(),
            store_name: "personal".to_owned(),
            created_at: timestamp(),
            agent_id: "codex".to_owned(),
            host_id: "taylor".to_owned(),
            user_id: Some("chris".to_owned()),
            session_id: Some("abc123".to_owned()),
            scope: "global".to_owned(),
            project_id: Some("github-com-cgraf78-hive-memory-018f5f57".to_owned()),
            subject: Some("workflow.preference".to_owned()),
            tags: vec!["preference".to_owned(), "workflow".to_owned()],
            confidence: Confidence::High,
            valid_from: None,
            valid_to: None,
            supersedes: Vec::new(),
            kind: None,
            classified: None,
            audience: Vec::new(),
            body: "Chris prefers concise summaries.".to_owned(),
            note_path: Some(note::note_relative_path("event-id", timestamp())),
            source: Some(EventSource {
                kind: "session".to_owned(),
                r#ref: Some("abc123".to_owned()),
            }),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hive-memory-event-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn event_round_trips_as_json() {
        let mut input = input();
        input.valid_from = Some("2026-06-01T00:00:00Z".to_owned());
        input.valid_to = Some("2026-07-01T00:00:00Z".to_owned());
        input.supersedes = vec!["old-event-id".to_owned()];
        let event = MemoryEvent::observation(input).expect("event");

        let rendered = render_event(&event).expect("render event");
        let parsed = parse_event(&rendered).expect("parse event");

        assert_eq!(parsed, event);
        assert_eq!(parsed.valid_to.as_deref(), Some("2026-07-01T00:00:00Z"));
        assert_eq!(parsed.supersedes, vec!["old-event-id".to_owned()]);
        assert!(rendered.contains("\"type\": \"memory.observation\""));
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn blank_validity_window_parses_as_absent() {
        let event = MemoryEvent::observation(input()).expect("event");
        let rendered = render_event(&event).expect("render event");
        // Inject blank validity timestamps, mirroring the note "blank means
        // absent" convention; these would otherwise fail RFC3339 validation.
        let with_blanks = rendered.replace(
            "\"body\":",
            "\"valid_from\": \"\",\n  \"valid_to\": \"   \",\n  \"body\":",
        );

        let parsed = parse_event(&with_blanks).expect("parse event with blank validity window");

        assert_eq!(parsed.valid_from, None);
        assert_eq!(parsed.valid_to, None);
    }

    #[test]
    fn rejects_inverted_validity_window() {
        let mut input = input();
        input.valid_from = Some("2030-01-01T00:00:00Z".to_owned());
        input.valid_to = Some("2020-01-01T00:00:00Z".to_owned());

        let err = MemoryEvent::observation(input).expect_err("event rejected");

        assert_eq!(
            err,
            EventError::InvalidValidityWindow {
                valid_from: "2030-01-01T00:00:00Z".to_owned(),
                valid_to: "2020-01-01T00:00:00Z".to_owned(),
            }
        );
    }

    #[test]
    fn classified_provenance_round_trips() {
        let mut input = input();
        input.classified = Some(note::ClassifiedBy {
            source: note::ClassifierSource::Llm,
            backend: Some("claude".to_owned()),
            at: "2026-06-12T00:00:00Z".to_owned(),
            verdict_version: 1,
            confidence: Some("high".to_owned()),
        });
        let event = MemoryEvent::observation(input).expect("event");

        let rendered = render_event(&event).expect("render event");
        let parsed = parse_event(&rendered).expect("parse event");

        let classified = parsed.classified.expect("classified");
        assert_eq!(classified.source, note::ClassifierSource::Llm);
        assert_eq!(classified.backend.as_deref(), Some("claude"));
        assert_eq!(classified.verdict_version, 1);
    }

    #[test]
    fn non_private_event_omits_accidental_audience() {
        let mut input = input();
        input.audience = vec!["codex".to_owned()];

        let event = MemoryEvent::observation(input).expect("event");

        assert!(event.audience.is_empty());
    }

    #[test]
    fn rejects_agent_private_event_without_audience() {
        let mut input = input();
        input.scope = "agent-private".to_owned();

        let err = MemoryEvent::observation(input).expect_err("event rejected");

        assert_eq!(err, EventError::MissingAudienceForAgentPrivate);
    }

    #[test]
    fn rejects_absolute_note_path() {
        let mut input = input();
        input.note_path = Some(PathBuf::from("/tmp/note.md"));

        let err = MemoryEvent::observation(input).expect_err("event rejected");

        assert_eq!(
            err,
            EventError::InvalidRelativePath {
                field: "note_path",
                value: "/tmp/note.md".to_owned()
            }
        );
    }

    #[test]
    fn rejects_unnormalized_note_path() {
        let mut input = input();
        input.note_path = Some(PathBuf::from("inbox/notes/../bad.md"));

        let err = MemoryEvent::observation(input).expect_err("event rejected");

        assert_eq!(
            err,
            EventError::InvalidRelativePath {
                field: "note_path",
                value: "inbox/notes/../bad.md".to_owned()
            }
        );
    }

    #[test]
    fn rejects_non_nfc_note_path() {
        let mut input = input();
        input.note_path = Some(PathBuf::from("inbox/notes/Cafe\u{301}.md"));

        let err = MemoryEvent::observation(input).expect_err("event rejected");

        assert_eq!(
            err,
            EventError::InvalidRelativePath {
                field: "note_path",
                value: "inbox/notes/Cafe\u{301}.md".to_owned()
            }
        );
    }

    #[test]
    fn writes_event_to_canonical_day_path() {
        let root = temp_dir("write");
        let event = MemoryEvent::observation(input()).expect("event");
        let options = AtomicWriteOptions {
            fsync: FsyncPolicy::Never,
            ..AtomicWriteOptions::default()
        };

        let result = write_event(&root, &event, &options).expect("write event");

        assert_eq!(
            result.path,
            root.join("inbox/events/2026/05/16/event-id.json")
        );
        let parsed = parse_event(&fs::read_to_string(&result.path).expect("read event"))
            .expect("parse event");
        assert_eq!(parsed.id, "event-id");
        assert_eq!(
            parsed.note_path.as_deref(),
            Some("inbox/notes/2026/05/16/event-id.md")
        );
    }

    #[test]
    fn create_new_write_refuses_event_collision() {
        let root = temp_dir("collision");
        let parent = root.join("inbox/events/2026/05/16");
        fs::create_dir_all(&parent).expect("create parent");
        fs::write(parent.join("event-id.json"), "{}").expect("precreate event");
        let event = MemoryEvent::observation(input()).expect("event");
        let options = AtomicWriteOptions {
            fsync: FsyncPolicy::Never,
            ..AtomicWriteOptions::default()
        };

        let err = write_event(&root, &event, &options).expect_err("write rejected");

        assert!(
            matches!(err, EventError::Write(message) if message.contains("final path already exists"))
        );
    }
}
