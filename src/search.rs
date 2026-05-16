//! Deterministic text search over the local triage index.
//!
//! The index narrows candidates by metadata. Search still reads the canonical
//! Markdown files for body matching so the cache never becomes a second source
//! of truth for memory text.

use crate::index::IndexEntry;
use crate::note;
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// Search request over one store's index entries.
#[derive(Debug, Clone)]
pub struct SearchInput<'a> {
    /// Store root containing canonical note files.
    pub store_root: &'a Path,
    /// Candidate metadata entries.
    pub entries: &'a [IndexEntry],
    /// Case-insensitive substring query.
    pub query: &'a str,
    /// Optional scope filter. Empty means all scopes allowed by source policy.
    pub scopes: &'a [String],
    /// Whether lower-confidence raw `hm note` entries are included.
    pub include_inbox: bool,
    /// Active agent identity for agent-private audience filtering.
    pub agent_id: Option<&'a str>,
    /// Maximum hits to return.
    pub limit: usize,
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Matched index metadata.
    pub entry: IndexEntry,
    /// Simple occurrence count used for deterministic ranking.
    pub score: usize,
    /// First matching line, trimmed for display.
    pub snippet: String,
}

/// Search failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// Query was empty after trimming.
    EmptyQuery,
    /// Candidate note could not be read.
    ReadNote {
        /// Note path that failed.
        path: PathBuf,
        /// Original error rendered for CLI diagnostics.
        message: String,
    },
    /// Candidate note could not be parsed.
    ParseNote {
        /// Note path that failed.
        path: PathBuf,
        /// Parse error.
        message: String,
    },
}

impl Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyQuery => write!(f, "search query must not be empty"),
            Self::ReadNote { path, message } => {
                write!(f, "failed to read note {}: {message}", path.display())
            }
            Self::ParseNote { path, message } => {
                write!(f, "failed to parse note {}: {message}", path.display())
            }
        }
    }
}

impl Error for SearchError {}

/// Search indexed notes by case-insensitive substring.
///
/// Ranking is intentionally simple and stable for v1: more occurrences first,
/// then newer timestamps, then lexical note path. This keeps behavior easy to
/// debug before any future scoring/index backend is introduced. The body is
/// read from canonical Markdown, while metadata terms come from the rebuilt
/// index so subject/tag searches do not need to parse every note twice.
pub fn search(input: SearchInput<'_>) -> Result<Vec<SearchHit>, SearchError> {
    let query = input.query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return Err(SearchError::EmptyQuery);
    }

    let mut hits = Vec::new();
    for entry in input.entries {
        if !source_allowed(entry, input.include_inbox) {
            continue;
        }
        if !scope_allowed(entry, input.scopes) {
            continue;
        }
        if !audience_allowed(entry, input.agent_id) {
            continue;
        }

        let note_path = input.store_root.join(&entry.note_path);
        let contents = fs::read_to_string(&note_path).map_err(|err| SearchError::ReadNote {
            path: note_path.clone(),
            message: err.to_string(),
        })?;
        let note = note::parse_note(&contents).map_err(|err| SearchError::ParseNote {
            path: note_path,
            message: err.to_string(),
        })?;
        let score = score_entry(entry, &note.body, &query);
        if score == 0 {
            continue;
        }
        hits.push(SearchHit {
            entry: entry.clone(),
            score,
            snippet: snippet(entry, &note.body, &query),
        });
    }

    hits.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                timestamp_rank(&right.entry.created_at).cmp(&timestamp_rank(&left.entry.created_at))
            })
            .then_with(|| left.entry.note_path.cmp(&right.entry.note_path))
    });
    hits.truncate(input.limit);
    Ok(hits)
}

fn source_allowed(entry: &IndexEntry, include_inbox: bool) -> bool {
    include_inbox || entry.entry_kind == note::EntryKind::Remember
}

fn scope_allowed(entry: &IndexEntry, scopes: &[String]) -> bool {
    scopes.is_empty() || scopes.iter().any(|scope| scope == &entry.scope)
}

fn audience_allowed(entry: &IndexEntry, agent_id: Option<&str>) -> bool {
    if entry.scope != "agent-private" {
        return true;
    }

    let Some(agent_id) = agent_id else {
        return false;
    };

    if entry.audience.is_empty() {
        return entry.agent_id == agent_id;
    }

    entry.audience.iter().any(|audience| audience == agent_id)
}

fn occurrence_count(body: &str, query: &str) -> usize {
    body.to_ascii_lowercase().matches(query).count()
}

fn score_entry(entry: &IndexEntry, body: &str, query: &str) -> usize {
    let metadata_score = entry
        .subject
        .as_deref()
        .map(|subject| occurrence_count(subject, query))
        .unwrap_or_default()
        + entry
            .tags
            .iter()
            .map(|tag| occurrence_count(tag, query))
            .sum::<usize>();
    occurrence_count(body, query) + metadata_score
}

fn snippet(entry: &IndexEntry, body: &str, query: &str) -> String {
    if let Some(line) = matching_body_line(body, query) {
        return line.chars().take(160).collect();
    }

    if let Some(subject) = entry
        .subject
        .as_deref()
        .filter(|subject| occurrence_count(subject, query) > 0)
    {
        return format!("subject: {subject}");
    }

    let tags = entry.tags.join(",");
    if occurrence_count(&tags, query) > 0 {
        return format!("tags: {tags}");
    }

    body.trim().chars().take(160).collect()
}

fn matching_body_line<'a>(body: &'a str, query: &str) -> Option<&'a str> {
    body.lines()
        .find(|line| line.to_ascii_lowercase().contains(query))
        .map(str::trim)
}

fn timestamp_rank(value: &str) -> i128 {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map(|timestamp| timestamp.unix_timestamp_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Sensitivity;
    use crate::index;
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
            "hive-memory-search-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
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

    fn timestamp(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds)
            .expect("timestamp")
            .replace_nanosecond(184_921_000)
            .expect("nanos")
    }

    fn options() -> AtomicWriteOptions {
        AtomicWriteOptions {
            fsync: FsyncPolicy::Never,
            ..AtomicWriteOptions::default()
        }
    }

    struct TestRecord<'a> {
        root: &'a Path,
        entry_kind: note::EntryKind,
        scope: &'a str,
        body: &'a str,
        created_at: OffsetDateTime,
        audience: Vec<String>,
        subject: Option<&'a str>,
        tags: Vec<String>,
    }

    fn write_record(
        root: &Path,
        entry_kind: note::EntryKind,
        scope: &str,
        body: &str,
        created_at: OffsetDateTime,
        audience: Vec<String>,
    ) {
        write_record_with_metadata(TestRecord {
            root,
            entry_kind,
            scope,
            body,
            created_at,
            audience,
            subject: None,
            tags: Vec::new(),
        });
    }

    fn write_record_with_metadata(record: TestRecord<'_>) {
        memory::write_record(memory::WriteRecordInput {
            root: record.root,
            manifest: &manifest(),
            entry_kind: record.entry_kind,
            created_at: record.created_at,
            agent_id: "codex".to_owned(),
            host_id: "taylor".to_owned(),
            user_id: "chris".to_owned(),
            session_id: None,
            scope: record.scope.to_owned(),
            confidence: note::Confidence::High,
            body: record.body.to_owned(),
            project_id: None,
            subject: record.subject.map(str::to_owned),
            tags: record.tags,
            audience: record.audience,
            source_kind: None,
            source_ref: None,
            write_event: true,
            options: options(),
        })
        .expect("write memory");
    }

    fn entries(root: &Path, cache: &Path) -> Vec<IndexEntry> {
        index::rebuild_index(index::RebuildIndexInput {
            store_name: "personal",
            store_root: root,
            cache_dir: cache,
            options: options(),
        })
        .expect("rebuild index")
        .entries
    }

    #[test]
    fn search_finds_remembered_notes_by_default() {
        let dir = temp_dir("remembered");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "TOML config is preferred.",
            timestamp(1_778_946_153),
            Vec::new(),
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "toml",
            scopes: &[],
            include_inbox: false,
            agent_id: None,
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].score, 1);
        assert_eq!(hits[0].snippet, "TOML config is preferred.");
    }

    #[test]
    fn search_matches_indexed_subject_and_tags() {
        let dir = temp_dir("metadata");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record_with_metadata(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "Body intentionally does not contain the query.",
            created_at: timestamp(1_778_946_153),
            audience: Vec::new(),
            subject: Some("workflow.preference"),
            tags: vec!["config".to_owned(), "toml".to_owned()],
        });
        let entries = entries(&root, &cache);

        let subject_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "workflow",
            scopes: &[],
            include_inbox: false,
            agent_id: None,
            limit: 20,
        })
        .expect("search");
        let tag_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "toml",
            scopes: &[],
            include_inbox: false,
            agent_id: None,
            limit: 20,
        })
        .expect("search");

        assert_eq!(subject_hits.len(), 1);
        assert_eq!(subject_hits[0].snippet, "subject: workflow.preference");
        assert_eq!(tag_hits.len(), 1);
        assert_eq!(tag_hits[0].snippet, "tags: config,toml");
    }

    #[test]
    fn search_excludes_raw_notes_unless_requested() {
        let dir = temp_dir("include-inbox");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Note,
            "global",
            "Raw note mentions TOML.",
            timestamp(1_778_946_153),
            Vec::new(),
        );
        let entries = entries(&root, &cache);

        let default_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "toml",
            scopes: &[],
            include_inbox: false,
            agent_id: None,
            limit: 20,
        })
        .expect("search");
        let inbox_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "toml",
            scopes: &[],
            include_inbox: true,
            agent_id: None,
            limit: 20,
        })
        .expect("search");

        assert!(default_hits.is_empty());
        assert_eq!(inbox_hits.len(), 1);
    }

    #[test]
    fn search_filters_agent_private_audience() {
        let dir = temp_dir("audience");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "agent-private",
            "Private TOML note.",
            timestamp(1_778_946_153),
            vec!["codex".to_owned()],
        );
        let entries = entries(&root, &cache);

        let codex_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "toml",
            scopes: &[],
            include_inbox: false,
            agent_id: Some("codex"),
            limit: 20,
        })
        .expect("search");
        let claude_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "toml",
            scopes: &[],
            include_inbox: false,
            agent_id: Some("claude"),
            limit: 20,
        })
        .expect("search");

        assert_eq!(codex_hits.len(), 1);
        assert!(claude_hits.is_empty());
    }
}
