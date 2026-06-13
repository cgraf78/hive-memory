//! Deterministic text search over curated files and the local triage index.
//!
//! Curated files are read directly because they are small human-maintained
//! Markdown documents. Inbox records use the fresh local index for metadata and
//! body matching; older cache entries without a body fall back to canonical
//! Markdown so stale indexes remain recoverable.

use crate::curated::CuratedFile;
use crate::index::IndexEntry;
use crate::{note, project, visibility};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// Search request over one store's index entries.
#[derive(Debug, Clone)]
pub struct SearchInput<'a> {
    /// Store root containing canonical note and curated files.
    pub store_root: &'a Path,
    /// Candidate metadata entries.
    pub entries: &'a [IndexEntry],
    /// Case-insensitive text query. Exact substring matches rank highest; when
    /// no exact phrase match exists, every query term must be present.
    pub query: &'a str,
    /// Optional scope filter. Empty means all scopes allowed by source policy.
    pub scopes: &'a [String],
    /// Selected source classes: `curated`, `remembered`, `inbox`, and `all`.
    ///
    /// Empty means remembered indexed entries only. CLI callers normally pass
    /// config defaults so curated memory participates by default.
    pub sources: &'a [String],
    /// Whether lower-confidence raw `hm note` entries are included.
    pub include_inbox: bool,
    /// Active agent identity for agent-private audience filtering.
    pub agent_id: Option<&'a str>,
    /// Active project identity. Project-scoped records must match it.
    pub project_id: Option<&'a str>,
    /// Maximum hits to return.
    pub limit: usize,
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Matched metadata.
    ///
    /// Curated hits use a synthetic entry so existing callers can keep one
    /// output shape while the canonical text remains the curated Markdown file.
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
    /// Curated file discovery failed.
    Curated {
        /// Path that failed.
        path: PathBuf,
        /// Original error rendered for diagnostics.
        message: String,
    },
    /// Project alias metadata could not be read.
    ProjectAlias {
        /// Alias file or directory path that failed.
        path: PathBuf,
        /// Original error rendered for diagnostics.
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
            Self::Curated { path, message } => {
                write!(
                    f,
                    "failed to read curated memory {}: {message}",
                    path.display()
                )
            }
            Self::ProjectAlias { path, message } => {
                write!(
                    f,
                    "failed to read project aliases {}: {message}",
                    path.display()
                )
            }
        }
    }
}

impl Error for SearchError {}

impl From<crate::curated::CuratedError> for SearchError {
    fn from(value: crate::curated::CuratedError) -> Self {
        match value {
            crate::curated::CuratedError::ReadFile { path, message } => {
                Self::Curated { path, message }
            }
            crate::curated::CuratedError::ProjectAlias { path, message } => {
                Self::ProjectAlias { path, message }
            }
        }
    }
}

/// Search curated files and indexed notes by deterministic text matching.
///
/// Ranking is intentionally simple and stable for v1: exact phrase occurrences
/// outrank all-term matches, then newer timestamps, then lexical note path.
/// This keeps behavior easy to debug before any future scoring/index backend is
/// introduced while still supporting the loose keyword queries agents naturally
/// issue during recall. Curated files score only body text; indexed entries also
/// score subject/tags so metadata searches do not need to parse every note
/// twice.
pub fn search(input: SearchInput<'_>) -> Result<Vec<SearchHit>, SearchError> {
    let query = SearchQuery::parse(input.query)?;
    let project_ids = project_filter_ids(input.store_root, input.project_id)?;

    let mut hits = Vec::new();
    if curated_source_allowed(input.sources) {
        for curated in crate::curated::collect(input.store_root, input.project_id)? {
            if !curated_scope_allowed(&curated, input.scopes) {
                continue;
            }
            let score = score_text(&curated.body, &query);
            if score == 0 {
                continue;
            }
            hits.push(SearchHit {
                entry: curated_entry(&curated, input.project_id),
                score,
                snippet: curated_snippet(&curated.body, &query),
            });
        }
    }

    for entry in input.entries {
        if !source_allowed(entry, input.sources, input.include_inbox) {
            continue;
        }
        if !scope_allowed(entry, input.scopes) {
            continue;
        }
        if !project_allowed(entry, project_ids.as_ref()) {
            continue;
        }
        if !visibility::audience_allows(entry, input.agent_id) {
            continue;
        }

        let body = indexed_body(input.store_root, entry)?;
        let score = score_entry(entry, &body, &query);
        if score == 0 {
            continue;
        }
        hits.push(SearchHit {
            entry: entry.clone(),
            score,
            snippet: snippet(entry, &body, &query),
        });
    }

    hits.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                confidence_rank(right.entry.confidence).cmp(&confidence_rank(left.entry.confidence))
            })
            .then_with(|| {
                timestamp_rank(&right.entry.created_at).cmp(&timestamp_rank(&left.entry.created_at))
            })
            .then_with(|| left.entry.note_path.cmp(&right.entry.note_path))
    });
    collapse_duplicate_hits(&mut hits);
    hits.truncate(input.limit);
    Ok(hits)
}

fn collapse_duplicate_hits(hits: &mut Vec<SearchHit>) {
    let mut seen = BTreeSet::new();
    hits.retain(|hit| {
        let key = duplicate_key(&hit.entry, &hit.snippet);
        seen.insert(key)
    });
}

fn duplicate_key(entry: &IndexEntry, fallback: &str) -> String {
    let body = if entry.body.is_empty() {
        fallback
    } else {
        entry.body.as_str()
    };
    normalize_duplicate_text(body)
}

fn normalize_duplicate_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn indexed_body<'a>(store_root: &Path, entry: &'a IndexEntry) -> Result<Cow<'a, str>, SearchError> {
    if !entry.body.is_empty() {
        return Ok(Cow::Borrowed(entry.body.as_str()));
    }

    let note_path = store_root.join(&entry.note_path);
    let contents = fs::read_to_string(&note_path).map_err(|err| SearchError::ReadNote {
        path: note_path.clone(),
        message: err.to_string(),
    })?;
    let note = note::parse_note(&contents).map_err(|err| SearchError::ParseNote {
        path: note_path,
        message: err.to_string(),
    })?;
    Ok(Cow::Owned(note.body))
}

fn curated_source_allowed(sources: &[String]) -> bool {
    sources
        .iter()
        .any(|source| source == "curated" || source == "all")
}

fn source_allowed(entry: &IndexEntry, sources: &[String], include_inbox: bool) -> bool {
    if sources.iter().any(|source| source == "all") {
        return true;
    }

    match entry.entry_kind {
        note::EntryKind::Remember => {
            sources.is_empty() || sources.iter().any(|source| source == "remembered")
        }
        note::EntryKind::Note => include_inbox || sources.iter().any(|source| source == "inbox"),
    }
}

fn scope_allowed(entry: &IndexEntry, scopes: &[String]) -> bool {
    scopes.is_empty() || scopes.iter().any(|scope| scope == &entry.scope)
}

fn curated_scope_allowed(candidate: &CuratedFile, scopes: &[String]) -> bool {
    scopes.is_empty() || scopes.iter().any(|scope| scope == &candidate.scope)
}

fn project_filter_ids(
    store_root: &Path,
    project_id: Option<&str>,
) -> Result<Option<BTreeSet<String>>, SearchError> {
    // Search needs the same project-identity continuity as prompt context:
    // indexed notes keep their original project_id, while aliases declare that
    // old and current ids should be queried as one logical project.
    project_id
        .map(|project_id| {
            project::related_project_ids(store_root, project_id).map_err(project_alias_error)
        })
        .transpose()
}

fn project_alias_error(err: project::ProjectError) -> SearchError {
    match err {
        project::ProjectError::Alias { path, message } => {
            SearchError::ProjectAlias { path, message }
        }
        other => SearchError::ProjectAlias {
            path: PathBuf::new(),
            message: other.to_string(),
        },
    }
}

fn project_allowed(entry: &IndexEntry, project_ids: Option<&BTreeSet<String>>) -> bool {
    if entry.scope != "project" {
        return true;
    }

    let Some(project_ids) = project_ids else {
        return false;
    };

    entry
        .project_id
        .as_deref()
        .is_some_and(|project_id| project_ids.contains(project_id))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchQuery {
    phrase: String,
    terms: Vec<QueryTerm>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueryTerm {
    value: String,
    aliases: Vec<&'static str>,
}

impl SearchQuery {
    fn parse(input: &str) -> Result<Self, SearchError> {
        let phrase = input.trim().to_ascii_lowercase();
        if phrase.is_empty() {
            return Err(SearchError::EmptyQuery);
        }

        let raw_terms = phrase
            .split_whitespace()
            .filter_map(normalize_query_token)
            .collect::<Vec<_>>();
        if raw_terms.is_empty() {
            return Err(SearchError::EmptyQuery);
        }
        let meaningful_terms = raw_terms
            .iter()
            .filter(|term| !is_query_stopword(term))
            .cloned()
            .collect::<Vec<_>>();
        let terms = meaningful_terms
            .into_iter()
            .filter(|term| !term.is_empty())
            .map(|term| QueryTerm {
                aliases: concept_aliases(&term),
                value: term,
            })
            .collect::<Vec<_>>();
        let terms = if terms.is_empty() {
            raw_terms
                .into_iter()
                .map(|term| QueryTerm {
                    aliases: concept_aliases(&term),
                    value: term,
                })
                .collect::<Vec<_>>()
        } else {
            terms
        };

        Ok(Self { phrase, terms })
    }
}

fn score_text(body: &str, query: &SearchQuery) -> usize {
    let lower = body.to_ascii_lowercase();
    let exact = lower.matches(&query.phrase).count();
    if exact > 0 {
        // Keep phrase hits clearly above keyword-only hits without making the
        // scoring model clever. Exact recall should remain the most predictable
        // path for humans, while agents can still use natural token queries.
        return exact * 10;
    }

    if query.terms.iter().all(|term| term_matches(&lower, term)) {
        return query
            .terms
            .iter()
            .map(|term| term_score(&lower, term))
            .sum::<usize>()
            .max(1);
    }

    0
}

fn normalize_query_token(token: &str) -> Option<String> {
    let normalized = token
        .trim_matches(|ch: char| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/' | '#'))
        })
        .to_ascii_lowercase();
    (!normalized.is_empty()).then_some(normalized)
}

fn is_query_stopword(term: &str) -> bool {
    matches!(
        term,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "before"
            | "for"
            | "from"
            | "how"
            | "i"
            | "in"
            | "is"
            | "it"
            | "of"
            | "on"
            | "or"
            | "should"
            | "the"
            | "to"
            | "what"
            | "when"
            | "where"
            | "which"
            | "with"
    )
}

fn concept_aliases(term: &str) -> Vec<&'static str> {
    match term {
        "agent" | "agents" | "coding" => vec!["agents.md", "agent instructions"],
        "documented" | "documentation" | "docs" => vec!["agents.md", "instructions"],
        "rules" | "guidelines" | "policy" | "policies" => vec!["instructions", "agents.md"],
        "ship" | "shipping" | "commit" | "commits" | "committing" => vec!["checkrun", "lint"],
        "validate" | "validates" | "validation" | "verify" | "verifies" => {
            vec!["checkrun", "lint", "format"]
        }
        _ => Vec::new(),
    }
}

fn term_matches(lower: &str, term: &QueryTerm) -> bool {
    lower.contains(&term.value) || term.aliases.iter().any(|alias| lower.contains(alias))
}

fn term_score(lower: &str, term: &QueryTerm) -> usize {
    lower.matches(&term.value).count()
        + term
            .aliases
            .iter()
            .map(|alias| lower.matches(alias).count())
            .sum::<usize>()
}

fn score_entry(entry: &IndexEntry, body: &str, query: &SearchQuery) -> usize {
    let metadata = entry
        .subject
        .as_deref()
        .into_iter()
        .chain(entry.tags.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let body_score = score_text(body, query);
    let metadata_score = score_text(&metadata, query);
    // Natural recall queries can span metadata and body text, for example a
    // subject naming the workflow and a body naming the concrete preference.
    // Only use the combined view as a fallback so ordinary body/metadata hits
    // are not double-counted.
    let combined_score = if body_score == 0 && metadata_score == 0 {
        score_text(&format!("{metadata} {body}"), query)
    } else {
        0
    };
    body_score + metadata_score + combined_score
}

fn curated_entry(curated: &CuratedFile, project_id: Option<&str>) -> IndexEntry {
    IndexEntry {
        id: curated.id.clone(),
        store_id: String::new(),
        entry_kind: note::EntryKind::Remember,
        scope: curated.scope.clone(),
        project_id: (curated.scope == "project").then(|| project_id.unwrap_or_default().to_owned()),
        audience: Vec::new(),
        tags: Vec::new(),
        subject: None,
        confidence: note::Confidence::High,
        kind: None,
        classified: None,
        agent_id: "human".to_owned(),
        host_id: String::new(),
        created_at: String::new(),
        body: curated.body.clone(),
        note_path: curated.relative_path.clone(),
        event_path: None,
    }
}

fn snippet(entry: &IndexEntry, body: &str, query: &SearchQuery) -> String {
    if let Some(line) = matching_body_line(body, query) {
        return line.chars().take(160).collect();
    }

    if let Some(subject) = entry
        .subject
        .as_deref()
        .filter(|subject| score_text(subject, query) > 0)
    {
        return format!("subject: {subject}");
    }

    let tags = entry.tags.join(",");
    if score_text(&tags, query) > 0 {
        return format!("tags: {tags}");
    }

    body.trim().chars().take(160).collect()
}

fn curated_snippet(body: &str, query: &SearchQuery) -> String {
    matching_body_line(body, query)
        .unwrap_or_else(|| body.trim())
        .chars()
        .take(160)
        .collect()
}

fn matching_body_line<'a>(body: &'a str, query: &SearchQuery) -> Option<&'a str> {
    body.lines()
        .find(|line| line.to_ascii_lowercase().contains(&query.phrase))
        .or_else(|| {
            body.lines().find(|line| {
                let lower = line.to_ascii_lowercase();
                query.terms.iter().all(|term| term_matches(&lower, term))
            })
        })
        .map(str::trim)
}

fn timestamp_rank(value: &str) -> i128 {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map(|timestamp| timestamp.unix_timestamp_nanos())
        .unwrap_or_default()
}

fn confidence_rank(confidence: note::Confidence) -> u8 {
    match confidence {
        note::Confidence::High => 3,
        note::Confidence::Medium => 2,
        note::Confidence::Low => 1,
    }
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
            kind: None,
            tags: record.tags,
            audience: record.audience,
            source_kind: None,
            source_ref: None,
            write_event: true,
            options: options(),
        })
        .expect("write memory");
    }

    fn write_project_record(root: &Path, body: &str, project_id: &str) {
        memory::write_record(memory::WriteRecordInput {
            root,
            manifest: &manifest(),
            entry_kind: note::EntryKind::Remember,
            created_at: timestamp(1_778_946_153),
            agent_id: "codex".to_owned(),
            host_id: "taylor".to_owned(),
            user_id: "chris".to_owned(),
            session_id: None,
            scope: "project".to_owned(),
            confidence: note::Confidence::High,
            body: body.to_owned(),
            project_id: Some(project_id.to_owned()),
            subject: None,
            kind: None,
            tags: Vec::new(),
            audience: Vec::new(),
            source_kind: None,
            source_ref: None,
            write_event: true,
            options: options(),
        })
        .expect("write project memory");
    }

    fn entries(root: &Path, cache: &Path) -> Vec<IndexEntry> {
        index::rebuild_index(index::RebuildIndexInput {
            store_name: "personal",
            store_root: root,
            cache_dir: cache,
            options: options(),
            path_case: crate::path::PathCase::Sensitive,
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
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 1);
        assert!(hits[0].score > 0);
        assert_eq!(hits[0].snippet, "TOML config is preferred.");
    }

    #[test]
    fn search_uses_indexed_body_without_reopening_note() {
        let dir = temp_dir("indexed-body");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "Indexed bodies keep warm search fast.",
            timestamp(1_778_946_153),
            Vec::new(),
        );
        let entries = entries(&root, &cache);
        fs::remove_dir_all(root.join("inbox")).expect("remove canonical inbox");

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "warm search",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "Indexed bodies keep warm search fast.");
    }

    #[test]
    fn search_matches_all_query_terms_without_exact_phrase() {
        let dir = temp_dir("keyword-terms");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "For Hive Memory agent ergonomics, plain `hm` commands should work as the normal path.",
            timestamp(1_778_946_153),
            Vec::new(),
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "agent ergonomics plain hm",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].snippet,
            "For Hive Memory agent ergonomics, plain `hm` commands should work as the normal path."
        );
    }

    #[test]
    fn search_keyword_fallback_requires_all_terms() {
        let dir = temp_dir("keyword-all-terms");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "Plain `hm` commands are the normal path for agent ergonomics.",
            timestamp(1_778_946_153),
            Vec::new(),
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "agent ergonomics missing",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");

        assert!(hits.is_empty());
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
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");
        let tag_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "toml",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
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
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");
        let inbox_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "toml",
            scopes: &[],
            sources: &[],
            include_inbox: true,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");

        assert!(default_hits.is_empty());
        assert_eq!(inbox_hits.len(), 1);
    }

    #[test]
    fn search_includes_curated_files_when_source_allows_them() {
        let dir = temp_dir("curated");
        let root = dir.join("store");
        let cache = dir.join("cache");
        fs::create_dir_all(root.join("rules")).expect("rules dir");
        fs::write(
            root.join("rules/preferences.md"),
            "Use TOML for human-editable configuration.\n",
        )
        .expect("curated file");
        let sources = vec!["curated".to_owned()];

        let default_hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "toml",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");
        let curated_hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "toml",
            scopes: &[],
            sources: &sources,
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");

        assert!(default_hits.is_empty());
        assert_eq!(curated_hits.len(), 1);
        assert_eq!(curated_hits[0].entry.id, "curated:rules/preferences.md");
        assert_eq!(curated_hits[0].entry.scope, "global");
        assert_eq!(
            curated_hits[0].snippet,
            "Use TOML for human-editable configuration."
        );
    }

    #[test]
    fn search_filters_project_curated_files_to_active_project() {
        let dir = temp_dir("curated-project");
        let root = dir.join("store");
        let cache = dir.join("cache");
        fs::create_dir_all(root.join("memories/projects/proj-a")).expect("project a dir");
        fs::create_dir_all(root.join("memories/projects/proj-b")).expect("project b dir");
        fs::write(
            root.join("memories/projects/proj-a/MEMORY.md"),
            "Project A release checklist uses TOML.\n",
        )
        .expect("project a memory");
        fs::write(
            root.join("memories/projects/proj-b/MEMORY.md"),
            "Project B release checklist uses TOML.\n",
        )
        .expect("project b memory");
        let sources = vec!["curated".to_owned()];
        let scopes = vec!["project".to_owned()];

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "toml",
            scopes: &scopes,
            sources: &sources,
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-a"),
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].entry.note_path,
            "memories/projects/proj-a/MEMORY.md"
        );
        assert_eq!(hits[0].entry.project_id.as_deref(), Some("proj-a"));
    }

    #[test]
    fn search_follows_project_aliases_for_curated_files() {
        let dir = temp_dir("curated-project-alias");
        let root = dir.join("store");
        let cache = dir.join("cache");
        fs::create_dir_all(root.join("memories/projects/proj-current")).expect("current dir");
        fs::create_dir_all(root.join("memories/projects/proj-old")).expect("old dir");
        fs::write(
            root.join("memories/projects/proj-current/MEMORY.md"),
            "Current project TOML memory.\n",
        )
        .expect("current memory");
        fs::write(
            root.join("memories/projects/proj-old/MEMORY.md"),
            "Old project TOML memory.\n",
        )
        .expect("old memory");
        fs::write(
            root.join("memories/projects/proj-current/aliases.toml"),
            "schema_version = 1\nproject_id = \"proj-current\"\naliases = [\"proj-old\"]\n",
        )
        .expect("aliases");
        let sources = vec!["curated".to_owned()];
        let scopes = vec!["project".to_owned()];

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "toml",
            scopes: &scopes,
            sources: &sources,
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-current"),
            limit: 20,
        })
        .expect("search");

        let paths = hits
            .iter()
            .map(|hit| hit.entry.note_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                "memories/projects/proj-current/MEMORY.md",
                "memories/projects/proj-old/MEMORY.md"
            ]
        );
    }

    #[test]
    fn search_follows_project_aliases_for_indexed_notes() {
        let dir = temp_dir("indexed-project-alias");
        let root = dir.join("store");
        let cache = dir.join("cache");
        fs::create_dir_all(root.join("memories/projects/proj-current")).expect("current dir");
        fs::write(
            root.join("memories/projects/proj-current/aliases.toml"),
            "schema_version = 1\nproject_id = \"proj-current\"\naliases = [\"proj-old\"]\n",
        )
        .expect("aliases");
        write_project_record(&root, "Old project remembered TOML.", "proj-old");
        let sources = vec!["remembered".to_owned()];
        let scopes = vec!["project".to_owned()];

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "toml",
            scopes: &scopes,
            sources: &sources,
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-current"),
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.project_id.as_deref(), Some("proj-old"));
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
            sources: &[],
            include_inbox: false,
            agent_id: Some("codex"),
            project_id: None,
            limit: 20,
        })
        .expect("search");
        let claude_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "toml",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: Some("claude"),
            project_id: None,
            limit: 20,
        })
        .expect("search");

        assert_eq!(codex_hits.len(), 1);
        assert!(claude_hits.is_empty());
    }

    #[test]
    fn search_expands_common_agent_workflow_concepts() {
        let dir = temp_dir("concepts");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_project_record(
            &root,
            "Project alpha keeps coding agent instructions in AGENTS.md.",
            "proj-alpha",
        );
        write_project_record(
            &root,
            "Before committing, run checkrun format and checkrun lint to validate local changes.",
            "proj-alpha",
        );
        let entries = entries(&root, &cache);
        let scopes = vec!["project".to_owned()];
        let sources = vec!["remembered".to_owned()];

        let rule_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "where are coding agent rules documented",
            scopes: &scopes,
            sources: &sources,
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-alpha"),
            limit: 20,
        })
        .expect("search");
        let validation_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "what validates changes before shipping",
            scopes: &scopes,
            sources: &sources,
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-alpha"),
            limit: 20,
        })
        .expect("search");

        assert_eq!(rule_hits.len(), 1);
        assert!(rule_hits[0].entry.body.contains("AGENTS.md"));
        assert_eq!(validation_hits.len(), 1);
        assert!(validation_hits[0].entry.body.contains("checkrun format"));
    }

    #[test]
    fn search_rejects_punctuation_only_queries_after_normalization() {
        let dir = temp_dir("punctuation-query");
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

        let result = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "?!",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        });

        assert_eq!(result, Err(SearchError::EmptyQuery));
    }
}
