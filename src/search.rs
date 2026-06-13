//! Deterministic text search over curated files and the local triage index.
//!
//! Curated files are read directly because they are small human-maintained
//! Markdown documents. Inbox records use the fresh local index for metadata and
//! body matching; older cache entries without a body fall back to canonical
//! Markdown so stale indexes remain recoverable.

use crate::curated::CuratedFile;
use crate::index::IndexEntry;
use crate::{entity, note, project, supersession, visibility};
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
    /// Structured scoring components used to produce `score`.
    pub trace: SearchScoreTrace,
    /// First matching line, trimmed for display.
    pub snippet: String,
}

/// Structured scoring components for one search hit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchScoreTrace {
    /// Phrase match score from canonical body text.
    pub body_phrase: usize,
    /// Term/alias match score from canonical body text.
    pub body_terms: usize,
    /// Phrase match score from subject/tags metadata.
    pub metadata_phrase: usize,
    /// Term/alias match score from subject/tags metadata.
    pub metadata_terms: usize,
    /// Phrase match score from the combined metadata/body fallback.
    pub combined_phrase: usize,
    /// Term/alias match score from the combined metadata/body fallback.
    pub combined_terms: usize,
    /// Entity overlap boost.
    pub entity: usize,
}

impl SearchScoreTrace {
    /// Total deterministic score used for ranking.
    #[must_use]
    pub fn total(&self) -> usize {
        self.body_phrase
            + self.body_terms
            + self.metadata_phrase
            + self.metadata_terms
            + self.combined_phrase
            + self.combined_terms
            + self.entity
    }
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
    /// Entity registry could not be loaded.
    EntityRegistry(String),
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
            Self::EntityRegistry(message) => write!(f, "{message}"),
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
    let registry = entity::EntityRegistry::load_for_store(input.store_root)
        .map_err(|err| SearchError::EntityRegistry(err.to_string()))?;
    let query = SearchQuery::parse(input.query, &registry)?;
    let project_ids = project_filter_ids(input.store_root, input.project_id)?;

    let mut hits = Vec::new();
    if curated_source_allowed(input.sources) {
        for curated in crate::curated::collect(input.store_root, input.project_id)? {
            if !curated_scope_allowed(&curated, input.scopes) {
                continue;
            }
            let text_score = score_text_trace(&curated.body, &query);
            let trace = SearchScoreTrace {
                body_phrase: text_score.phrase,
                body_terms: text_score.terms,
                ..SearchScoreTrace::default()
            };
            let score = trace.total();
            if score == 0 {
                continue;
            }
            hits.push(SearchHit {
                entry: curated_entry(&curated, input.project_id, &registry),
                score,
                trace,
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
        if !validity_allows(entry, &query) {
            continue;
        }

        let body = indexed_body(input.store_root, entry)?;
        let trace = score_entry(entry, &body, &query);
        let score = trace.total();
        if score == 0 {
            continue;
        }
        hits.push(SearchHit {
            entry: entry.clone(),
            score,
            trace,
            snippet: snippet(entry, &body, &query),
        });
    }

    let temporal_intent = query.temporal_intent();
    hits.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                confidence_rank(right.entry.confidence).cmp(&confidence_rank(left.entry.confidence))
            })
            .then_with(|| {
                temporal_rank(temporal_intent, &right.entry.created_at)
                    .cmp(&temporal_rank(temporal_intent, &left.entry.created_at))
            })
            .then_with(|| {
                timestamp_rank(&right.entry.created_at).cmp(&timestamp_rank(&left.entry.created_at))
            })
            .then_with(|| left.entry.note_path.cmp(&right.entry.note_path))
    });
    suppress_superseded_hits(&mut hits, &query, input.limit);
    collapse_duplicate_hits(&mut hits);
    hits.truncate(input.limit);
    Ok(hits)
}

fn suppress_superseded_hits(hits: &mut Vec<SearchHit>, query: &SearchQuery, limit: usize) {
    let scan_len = hits.len().min(limit.saturating_mul(4).clamp(16, 128));
    let mut suppressed = BTreeSet::new();
    for older in hits.iter().take(scan_len) {
        for newer in hits.iter().take(scan_len) {
            if supersession::should_suppress_older(&older.entry, &newer.entry, Some(&query.phrase))
            {
                suppressed.insert(older.entry.id.clone());
                break;
            }
        }
    }
    hits.retain(|hit| !suppressed.contains(&hit.entry.id));
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

fn validity_allows(entry: &IndexEntry, query: &SearchQuery) -> bool {
    if query.temporal_intent() == Some(TemporalIntent::Oldest) {
        return true;
    }
    let now = OffsetDateTime::now_utc();
    if let Some(valid_from) = entry.valid_from.as_deref()
        && let Some(valid_from) = parse_time(valid_from)
        && valid_from > now
    {
        return false;
    }
    if let Some(valid_to) = entry.valid_to.as_deref()
        && let Some(valid_to) = parse_time(valid_to)
        && valid_to <= now
    {
        return false;
    }
    true
}

fn parse_time(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchQuery {
    phrase: String,
    terms: Vec<QueryTerm>,
    entities: Vec<entity::EntityId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueryTerm {
    value: String,
    alternatives: Vec<String>,
    required: bool,
}

impl SearchQuery {
    fn parse(input: &str, registry: &entity::EntityRegistry) -> Result<Self, SearchError> {
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
            .map(QueryTerm::new)
            .collect::<Vec<_>>();
        let terms = if terms.is_empty() {
            raw_terms
                .into_iter()
                .map(QueryTerm::new)
                .collect::<Vec<_>>()
        } else {
            terms
        };

        let entities = entity::extract_with_registry(input, registry);

        Ok(Self {
            phrase,
            terms,
            entities,
        })
    }

    fn min_matching_terms(&self) -> usize {
        match self.terms.len() {
            0 => 0,
            1..=3 => self.terms.len(),
            len => ((len * 3).div_ceil(5)).max(3),
        }
    }

    fn temporal_intent(&self) -> Option<TemporalIntent> {
        if contains_temporal_indicator(&self.phrase, NEWEST_TERMS) {
            return Some(TemporalIntent::Newest);
        }
        if contains_temporal_indicator(&self.phrase, OLDEST_TERMS) {
            return Some(TemporalIntent::Oldest);
        }
        None
    }

    fn has_required_terms(&self) -> bool {
        self.terms.iter().any(|term| term.required)
    }

    fn has_entities(&self) -> bool {
        !self.entities.is_empty()
    }
}

impl QueryTerm {
    fn new(value: String) -> Self {
        let mut alternatives = Vec::new();
        alternatives.extend(concept_aliases(&value).into_iter().map(str::to_owned));
        alternatives.extend(morphological_alternatives(&value));
        alternatives.sort();
        alternatives.dedup();
        alternatives.retain(|alternative| alternative != &value);
        Self {
            required: is_required_query_term(&value) || is_required_intent_term(&value),
            value,
            alternatives,
        }
    }
}

fn score_text(body: &str, query: &SearchQuery) -> usize {
    score_text_trace(body, query).total()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TextScoreTrace {
    phrase: usize,
    terms: usize,
}

impl TextScoreTrace {
    fn total(&self) -> usize {
        self.phrase + self.terms
    }
}

fn score_text_trace(body: &str, query: &SearchQuery) -> TextScoreTrace {
    let lower = body.to_ascii_lowercase();
    let exact = count_pattern_matches(&lower, &query.phrase, query.has_required_terms());
    if exact > 0 {
        // Keep phrase hits clearly above keyword-only hits without making the
        // scoring model clever. Exact recall should remain the most predictable
        // path for humans, while agents can still use natural token queries.
        return TextScoreTrace {
            phrase: exact * (query.terms.len() * 20 + 100),
            terms: 0,
        };
    }

    let coverage = term_coverage(&lower, query);
    if coverage.required_matched == coverage.required_total
        && coverage.matched >= query.min_matching_terms()
    {
        return TextScoreTrace {
            phrase: 0,
            terms: coverage.matched * 10 + coverage.score,
        };
    }

    TextScoreTrace::default()
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
        "a" | "am"
            | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "before"
            | "can"
            | "could"
            | "did"
            | "do"
            | "does"
            | "for"
            | "from"
            | "had"
            | "has"
            | "have"
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
            | "after"
            | "current"
            | "currently"
            | "earliest"
            | "first"
            | "former"
            | "formerly"
            | "initial"
            | "last"
            | "latest"
            | "later"
            | "newest"
            | "now"
            | "oldest"
            | "original"
            | "previous"
            | "previously"
            | "recent"
            | "recently"
            | "was"
            | "were"
            | "which"
            | "with"
            | "would"
    )
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TemporalIntent {
    Newest,
    Oldest,
}

const NEWEST_TERMS: &[&str] = &[
    "current",
    "currently",
    "last",
    "latest",
    "later",
    "newest",
    "now",
    "recent",
    "recently",
];
const OLDEST_TERMS: &[&str] = &[
    "earliest",
    "first",
    "former",
    "formerly",
    "initial",
    "old",
    "oldest",
    "original",
    "previous",
    "previously",
];

fn contains_temporal_indicator(phrase: &str, terms: &[&str]) -> bool {
    phrase
        .split_whitespace()
        .filter_map(normalize_query_token)
        .any(|term| terms.iter().any(|candidate| *candidate == term))
}

fn is_required_query_term(term: &str) -> bool {
    matches!(
        term,
        "not" | "no" | "never" | "without" | "except" | "exclude"
    )
}

fn is_required_intent_term(term: &str) -> bool {
    matches!(
        term,
        "buy"
            | "bought"
            | "favorite"
            | "favourite"
            | "order"
            | "ordered"
            | "prefer"
            | "preferred"
            | "prefers"
            | "preference"
            | "preferences"
            | "purchase"
            | "purchased"
            | "recommend"
            | "recommendation"
            | "recommendations"
            | "recommended"
            | "suggest"
            | "suggested"
            | "suggestion"
            | "suggestions"
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
        "buy" | "bought" | "order" | "ordered" | "purchase" | "purchased" => {
            vec!["shopping", "payment"]
        }
        "favorite" | "favourite" | "prefer" | "preferred" | "prefers" | "preference"
        | "preferences" => vec![
            "likes",
            "like",
            "favorite",
            "prefer",
            "prefers",
            "preferred",
            "preference",
            "preferences",
        ],
        "recommend" | "recommendation" | "recommendations" | "recommended" | "suggest"
        | "suggested" | "suggestion" | "suggestions" => {
            vec!["recommend", "suggest", "advice"]
        }
        "tell" | "told" | "said" | "say" => vec!["mentioned", "said"],
        _ => Vec::new(),
    }
}

fn term_score(lower: &str, term: &QueryTerm) -> usize {
    let score = count_pattern_matches(lower, &term.value, term.required)
        + term
            .alternatives
            .iter()
            .map(|alternative| count_pattern_matches(lower, alternative, term.required))
            .sum::<usize>();
    score.min(3)
}

#[derive(Debug, Clone, Copy)]
struct TermCoverage {
    matched: usize,
    required_matched: usize,
    required_total: usize,
    score: usize,
}

fn term_coverage(lower: &str, query: &SearchQuery) -> TermCoverage {
    query.terms.iter().fold(
        TermCoverage {
            matched: 0,
            required_matched: 0,
            required_total: 0,
            score: 0,
        },
        |coverage, term| {
            let score = term_score(lower, term);
            let required_total = coverage.required_total + usize::from(term.required);
            if score == 0 {
                TermCoverage {
                    required_total,
                    ..coverage
                }
            } else {
                TermCoverage {
                    matched: coverage.matched + 1,
                    required_matched: coverage.required_matched + usize::from(term.required),
                    required_total,
                    score: coverage.score + score,
                }
            }
        },
    )
}

fn morphological_alternatives(term: &str) -> Vec<String> {
    let mut alternatives = Vec::new();
    if term.len() >= 4 {
        alternatives.push(format!("{term}s"));
        alternatives.push(format!("{term}ed"));
        alternatives.push(format!("{term}ing"));
        if let Some(stem) = term.strip_suffix('y').filter(|stem| !stem.is_empty()) {
            alternatives.push(format!("{stem}ies"));
        } else {
            alternatives.push(format!("{term}es"));
        }
        if let Some(stem) = term.strip_suffix('e').filter(|stem| stem.len() >= 3) {
            alternatives.push(format!("{stem}ed"));
            alternatives.push(format!("{stem}ing"));
        }
    }
    if let Some(stem) = term.strip_suffix("ies").filter(|stem| stem.len() >= 3) {
        alternatives.push(format!("{stem}y"));
    }
    if let Some(stem) = term.strip_suffix("es").filter(|stem| stem.len() >= 4) {
        alternatives.push(stem.to_owned());
    }
    if let Some(stem) = term.strip_suffix('s').filter(|stem| stem.len() >= 4) {
        alternatives.push(stem.to_owned());
    }
    if let Some(stem) = term.strip_suffix("ed").filter(|stem| stem.len() >= 4) {
        alternatives.push(stem.to_owned());
        alternatives.push(format!("{stem}e"));
    }
    if let Some(stem) = term.strip_suffix("ing").filter(|stem| stem.len() >= 4) {
        alternatives.push(stem.to_owned());
        alternatives.push(format!("{stem}e"));
    }
    alternatives
}

fn count_pattern_matches(lower: &str, pattern: &str, require_right_boundary: bool) -> usize {
    if pattern.is_empty() {
        return 0;
    }

    let mut count = 0usize;
    let mut offset = 0usize;
    while let Some(relative_index) = lower[offset..].find(pattern) {
        let index = offset + relative_index;
        let end = index + pattern.len();
        if pattern_boundary_allows(lower.as_bytes(), index, end, require_right_boundary) {
            count += 1;
        }
        offset = end;
    }
    count
}

fn pattern_boundary_allows(
    bytes: &[u8],
    start: usize,
    end: usize,
    require_right_boundary: bool,
) -> bool {
    let left_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
    let right_ok =
        !require_right_boundary || end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
    left_ok && right_ok
}

fn score_entry(entry: &IndexEntry, body: &str, query: &SearchQuery) -> SearchScoreTrace {
    let metadata = entry
        .subject
        .as_deref()
        .into_iter()
        .chain(entry.tags.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let body_score = score_text_trace(body, query);
    let metadata_score = score_text_trace(&metadata, query);
    // Natural recall queries can span metadata and body text, for example a
    // subject naming the workflow and a body naming the concrete preference.
    // Only use the combined view as a fallback so ordinary body/metadata hits
    // are not double-counted.
    let combined_score =
        if body_score.total() == 0 && metadata_score.total() == 0 && !query.has_required_terms() {
            score_text_trace(&format!("{metadata} {body}"), query)
        } else {
            TextScoreTrace::default()
        };
    SearchScoreTrace {
        body_phrase: body_score.phrase,
        body_terms: body_score.terms,
        metadata_phrase: metadata_score.phrase,
        metadata_terms: metadata_score.terms,
        combined_phrase: combined_score.phrase,
        combined_terms: combined_score.terms,
        entity: score_entities(entry, query),
    }
}

fn score_entities(entry: &IndexEntry, query: &SearchQuery) -> usize {
    if !query.has_entities() || entry.entities.is_empty() || query.has_required_terms() {
        return 0;
    }

    let overlap = query
        .entities
        .iter()
        .filter(|entity| entry.entities.contains(*entity))
        .count();
    if overlap == 0 {
        return 0;
    }

    // Entity overlap is strong enough to recall an alias-only hit, but still
    // below exact phrase matches. This lets "pre landing verification" recover
    // a `sley ready` memory without allowing one broad entity to dominate
    // obvious textual matches.
    overlap * 45
}

fn curated_entry(
    curated: &CuratedFile,
    project_id: Option<&str>,
    registry: &entity::EntityRegistry,
) -> IndexEntry {
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
        valid_from: None,
        valid_to: None,
        supersedes: Vec::new(),
        kind: None,
        entities: entity::extract_with_registry(&curated.body, registry),
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
                term_coverage(&lower, query).matched >= query.min_matching_terms()
            })
        })
        .map(str::trim)
}

fn timestamp_rank(value: &str) -> i128 {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map(|timestamp| timestamp.unix_timestamp_nanos())
        .unwrap_or_default()
}

fn temporal_rank(intent: Option<TemporalIntent>, created_at: &str) -> i128 {
    match intent {
        Some(TemporalIntent::Newest) => timestamp_rank(created_at),
        Some(TemporalIntent::Oldest) => -timestamp_rank(created_at),
        None => 0,
    }
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
            valid_from: None,
            valid_to: None,
            supersedes: Vec::new(),
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
        write_project_record_at(root, body, project_id, timestamp(1_778_946_153));
    }

    fn write_project_record_at(
        root: &Path,
        body: &str,
        project_id: &str,
        created_at: OffsetDateTime,
    ) {
        memory::write_record(memory::WriteRecordInput {
            root,
            manifest: &manifest(),
            entry_kind: note::EntryKind::Remember,
            created_at,
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
            valid_from: None,
            valid_to: None,
            supersedes: Vec::new(),
            tags: Vec::new(),
            audience: Vec::new(),
            source_kind: None,
            source_ref: None,
            write_event: true,
            options: options(),
        })
        .expect("write project memory");
    }

    fn write_project_record_with_validity(
        root: &Path,
        body: &str,
        project_id: &str,
        valid_to: Option<&str>,
        supersedes: Vec<String>,
    ) -> String {
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
            valid_from: None,
            valid_to: valid_to.map(str::to_owned),
            supersedes,
            tags: Vec::new(),
            audience: Vec::new(),
            source_kind: None,
            source_ref: None,
            write_event: true,
            options: options(),
        })
        .expect("write project memory")
        .id
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

    fn hit_bodies(hits: &[SearchHit]) -> Vec<String> {
        hits.iter().map(|hit| hit.entry.body.clone()).collect()
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
    fn search_long_natural_queries_require_core_term_coverage() {
        let dir = temp_dir("keyword-coverage");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "Chris prefers dark roast coffee for morning brewing.",
            timestamp(1_778_946_153),
            Vec::new(),
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "what kind of coffee does chris usually prefer in the morning",
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
            "Chris prefers dark roast coffee for morning brewing."
        );
    }

    #[test]
    fn search_long_queries_reject_insufficient_term_coverage() {
        let dir = temp_dir("keyword-insufficient-coverage");
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
            query: "agent ergonomics missing unrelated banana",
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
    fn search_long_query_threshold_scales_past_eight_terms() {
        let query = SearchQuery::parse(
            "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november",
            &entity::EntityRegistry::builtin(),
        )
        .expect("query");

        assert_eq!(query.terms.len(), 14);
        assert_eq!(query.min_matching_terms(), 9);
    }

    #[test]
    fn search_caps_repeated_term_score_contribution() {
        let term = QueryTerm::new("repeat".to_owned());

        assert_eq!(
            term_score(
                "repeat repeat repeat repeat repeat repeat repeat repeat",
                &term
            ),
            3
        );
    }

    #[test]
    fn search_long_queries_require_negation_terms() {
        let dir = temp_dir("keyword-negation");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "Chris prefers Delta airline for regional flights.",
            timestamp(1_778_946_153),
            Vec::new(),
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "which airline does chris not prefer",
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
    fn search_negation_terms_require_full_token_boundaries() {
        let dir = temp_dir("keyword-negation-boundary");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "Note: Chris prefers Delta airline for regional flights.",
            timestamp(1_778_946_153),
            Vec::new(),
        );
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "Normal preference record: Chris prefers United airline for regional flights.",
            timestamp(1_778_946_154),
            Vec::new(),
        );

        let not_hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "which airline does chris not prefer",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");
        let no_hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "which airline does chris no prefer",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");

        assert!(not_hits.is_empty());
        assert!(no_hits.is_empty());
    }

    #[test]
    fn search_does_not_match_positive_preference_inside_dislike() {
        let dir = temp_dir("keyword-dislike");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "Chris dislikes Delta airline for regional flights.",
            timestamp(1_778_946_153),
            Vec::new(),
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "which airline was preferred for regional flights",
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
    fn search_matches_inflections_without_substring_fallback() {
        let dir = temp_dir("keyword-inflections");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "Chris watched several documentaries and enjoyed replaying the archived talks.",
            timestamp(1_778_946_153),
            Vec::new(),
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "which documentary did chris replay archive talk",
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
            "Chris watched several documentaries and enjoyed replaying the archived talks."
        );
    }

    #[test]
    fn search_required_terms_cannot_be_satisfied_across_metadata_and_body() {
        let dir = temp_dir("keyword-metadata-required");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record_with_metadata(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "Chris dislikes Delta airline for regional flights.",
            created_at: timestamp(1_778_946_153),
            audience: Vec::new(),
            subject: Some("travel.preference"),
            tags: Vec::new(),
        });

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "which airline was preferred for regional flights",
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
    fn search_prefers_broader_term_coverage_over_repetition() {
        let dir = temp_dir("keyword-coverage-ranking");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "alpha bravo charlie delta alpha bravo charlie delta alpha bravo charlie delta",
            timestamp(1_778_946_153),
            Vec::new(),
        );
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "alpha bravo charlie delta echo foxtrot",
            timestamp(1_778_946_154),
            Vec::new(),
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "alpha bravo charlie delta echo foxtrot",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].snippet, "alpha bravo charlie delta echo foxtrot");
    }

    #[test]
    fn search_preserves_will_as_a_name() {
        let dir = temp_dir("keyword-will-name");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "Ada has a payment status update for the group.",
            timestamp(1_778_946_153),
            Vec::new(),
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "what did Will order for Ada",
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
    fn search_expands_preference_and_recommendation_terms() {
        let dir = temp_dir("keyword-expansion");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "The user likes Delta for regional flights and asked for advice on window seats.",
            timestamp(1_778_946_153),
            Vec::new(),
        );

        let preference_hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "which airline was preferred for regional flights",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");
        let recommendation_hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "seat recommendation regional flights",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");

        assert_eq!(preference_hits.len(), 1);
        assert_eq!(recommendation_hits.len(), 1);
    }

    #[test]
    fn search_ranks_newest_hits_for_current_temporal_queries() {
        let dir = temp_dir("temporal-newest");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_project_record_at(
            &root,
            "Project alpha status update says deploys use manual checks.",
            "proj-alpha",
            timestamp(1_778_946_153),
        );
        write_project_record_at(
            &root,
            "Project alpha status update says deploys use checkrun gates.",
            "proj-alpha",
            timestamp(1_778_946_200),
        );
        let entries = entries(&root, &cache);
        let scopes = vec!["project".to_owned()];
        let sources = vec!["remembered".to_owned()];

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "latest status update",
            scopes: &scopes,
            sources: &sources,
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-alpha"),
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 2);
        assert!(hits[0].entry.body.contains("checkrun gates"));
    }

    #[test]
    fn search_ranks_oldest_hits_for_initial_temporal_queries() {
        let dir = temp_dir("temporal-oldest");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_project_record_at(
            &root,
            "Project alpha status update says deploys use manual checks.",
            "proj-alpha",
            timestamp(1_778_946_153),
        );
        write_project_record_at(
            &root,
            "Project alpha status update says deploys use checkrun gates.",
            "proj-alpha",
            timestamp(1_778_946_200),
        );
        let entries = entries(&root, &cache);
        let scopes = vec!["project".to_owned()];
        let sources = vec!["remembered".to_owned()];

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "first status update",
            scopes: &scopes,
            sources: &sources,
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-alpha"),
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 2);
        assert!(hits[0].entry.body.contains("manual checks"));
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
    fn search_uses_entity_links_for_alias_only_recall() {
        let dir = temp_dir("entity-links");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_project_record(
            &root,
            "Project alpha requires `sley ready` before landing PRs.",
            "proj-alpha",
        );
        write_project_record(
            &root,
            "Project beta requires `sley ready` before release reviews.",
            "proj-beta",
        );
        let entries = entries(&root, &cache);
        assert!(
            entries
                .iter()
                .any(|entry| entry.entities.contains(&"tool:sley".to_owned())),
            "index should carry extracted sley entity links: {entries:?}"
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "pre landing verification gate",
            scopes: &["project".to_owned()],
            sources: &["remembered".to_owned()],
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-alpha"),
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 1);
        assert!(hits[0].entry.body.contains("sley ready"));
        assert_eq!(hits[0].entry.project_id.as_deref(), Some("proj-alpha"));
        assert_eq!(hits[0].score, hits[0].trace.total());
        assert!(
            hits[0].trace.entity > 0,
            "alias-only recall should expose the entity boost in the score trace: {:?}",
            hits[0].trace
        );
    }

    #[test]
    fn search_score_trace_explains_body_components() {
        let dir = temp_dir("score-trace");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(
            &root,
            note::EntryKind::Remember,
            "global",
            "Use concise summaries for release notes.",
            timestamp(1_778_946_153),
            Vec::new(),
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "release notes",
            scopes: &[],
            sources: &[],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].score, hits[0].trace.total());
        assert!(
            hits[0].trace.body_terms > 0 || hits[0].trace.body_phrase > 0,
            "body match should be visible in trace: {:?}",
            hits[0].trace
        );
        assert_eq!(hits[0].trace.entity, 0);
    }

    #[test]
    fn search_uses_store_entity_registry_aliases() {
        let dir = temp_dir("entity-registry");
        let root = dir.join("store");
        let cache = dir.join("cache");
        fs::create_dir_all(&root).expect("create store root");
        fs::write(
            root.join("entities.toml"),
            r#"
schema_version = 1

[[entity]]
id = "tool:deployctl"
aliases = ["deployctl", "release promotion gate"]
"#,
        )
        .expect("write entity registry");
        write_project_record(
            &root,
            "Project alpha uses `deployctl approve` before production release.",
            "proj-alpha",
        );

        let entries = entries(&root, &cache);
        assert!(
            entries
                .iter()
                .any(|entry| entry.entities.contains(&"tool:deployctl".to_owned())),
            "index should carry store-registry entity links: {entries:?}"
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "release promotion gate",
            scopes: &["project".to_owned()],
            sources: &["remembered".to_owned()],
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-alpha"),
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 1);
        assert!(hits[0].entry.body.contains("deployctl approve"));
        assert_eq!(hits[0].score, hits[0].trace.total());
        assert!(
            hits[0].trace.entity > 0,
            "registry alias should be visible in entity score trace: {:?}",
            hits[0].trace
        );
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

    #[test]
    fn search_suppresses_superseded_records_for_broad_recall() {
        let dir = temp_dir("superseded-broad");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_project_record_at(
            &root,
            "Project alpha used to run cargo fmt before committing.",
            "proj-alpha",
            timestamp(1_778_946_153),
        );
        write_project_record_at(
            &root,
            "Project alpha now uses checkrun format and checkrun lint before committing.",
            "proj-alpha",
            timestamp(1_778_946_154),
        );
        let entries = entries(&root, &cache);
        let scopes = vec!["project".to_owned()];
        let sources = vec!["remembered".to_owned()];

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "before committing",
            scopes: &scopes,
            sources: &sources,
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-alpha"),
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 1);
        assert!(hits[0].entry.body.contains("now uses checkrun"));
    }

    #[test]
    fn search_suppresses_expired_records_unless_query_is_historical() {
        let dir = temp_dir("validity");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_project_record_with_validity(
            &root,
            "Project alpha deploys with the old launch gate.",
            "proj-alpha",
            Some("2000-01-01T00:00:00Z"),
            Vec::new(),
        );
        write_project_record_with_validity(
            &root,
            "Project alpha deploys with the current launch gate.",
            "proj-alpha",
            None,
            Vec::new(),
        );
        let entries = entries(&root, &cache);
        let scopes = vec!["project".to_owned()];
        let sources = vec!["remembered".to_owned()];

        let current_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "launch gate",
            scopes: &scopes,
            sources: &sources,
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-alpha"),
            limit: 20,
        })
        .expect("search");
        let historical_hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "old launch gate",
            scopes: &scopes,
            sources: &sources,
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-alpha"),
            limit: 20,
        })
        .expect("search");

        assert_eq!(
            hit_bodies(&current_hits),
            vec!["Project alpha deploys with the current launch gate."]
        );
        assert!(
            hit_bodies(&historical_hits)
                .contains(&"Project alpha deploys with the old launch gate.".to_owned())
        );
    }

    #[test]
    fn search_uses_explicit_supersedes_metadata() {
        let dir = temp_dir("explicit-supersedes");
        let root = dir.join("store");
        let cache = dir.join("cache");
        let old_id = write_project_record_with_validity(
            &root,
            "Project alpha deployment gate is launchctl.",
            "proj-alpha",
            None,
            Vec::new(),
        );
        write_project_record_with_validity(
            &root,
            "Project alpha deployment gate is deployctl.",
            "proj-alpha",
            None,
            vec![old_id],
        );

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries(&root, &cache),
            query: "deployment gate",
            scopes: &["project".to_owned()],
            sources: &["remembered".to_owned()],
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-alpha"),
            limit: 20,
        })
        .expect("search");

        assert_eq!(
            hit_bodies(&hits),
            vec!["Project alpha deployment gate is deployctl."]
        );
    }

    #[test]
    fn search_keeps_superseded_records_for_explicit_historical_query() {
        let dir = temp_dir("superseded-explicit");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_project_record_at(
            &root,
            "Project alpha used to run cargo fmt before committing.",
            "proj-alpha",
            timestamp(1_778_946_153),
        );
        write_project_record_at(
            &root,
            "Project alpha now uses checkrun format and checkrun lint before committing.",
            "proj-alpha",
            timestamp(1_778_946_154),
        );
        let entries = entries(&root, &cache);
        let scopes = vec!["project".to_owned()];
        let sources = vec!["remembered".to_owned()];

        let hits = search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: "cargo fmt",
            scopes: &scopes,
            sources: &sources,
            include_inbox: false,
            agent_id: None,
            project_id: Some("proj-alpha"),
            limit: 20,
        })
        .expect("search");

        assert_eq!(hits.len(), 1);
        assert!(hits[0].entry.body.contains("used to run cargo fmt"));
    }
}
