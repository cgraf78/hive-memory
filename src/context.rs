//! Context assembly for agent prompts and hook adapters.
//!
//! Context output is not canonical memory. It is a carefully labeled view over
//! canonical notes, designed to be safe to inject into an agent as data. The
//! data-boundary blocks and escaping here are part of that trust boundary.

use crate::curated::CuratedFile;
use crate::index::IndexEntry;
use crate::inject::{self, ClassifyInput, IncidentMarkers, InjectClass};
use crate::{note, project, supersession, validity, visibility};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// Request for assembling context from one store's indexed notes.
#[derive(Debug, Clone)]
pub struct ContextInput<'a> {
    /// Human/config alias for the selected store.
    pub store_name: &'a str,
    /// Store root containing canonical note files.
    pub store_root: &'a Path,
    /// Candidate metadata entries from the local index.
    pub entries: &'a [IndexEntry],
    /// Selected scopes. Empty means all indexed scopes are eligible.
    pub scopes: &'a [String],
    /// Selected source classes: `curated`, `remembered`, `inbox`, and `all`.
    /// Empty means remembered memory only; raw inbox still requires an explicit
    /// source or `include_inbox`.
    pub sources: &'a [String],
    /// Whether raw `hm note` entries are explicitly included.
    pub include_inbox: bool,
    /// Whether records classified as search-only should still render.
    ///
    /// Interactive inspection can ask for raw/search-only material explicitly,
    /// but hook/session-start context keeps these records out so startup memory
    /// stays focused.
    pub include_search_only: bool,
    /// Active agent identity for agent-private audience filtering.
    pub agent_id: Option<&'a str>,
    /// Active project identity. When present, project-scoped notes must match it.
    pub project_id: Option<&'a str>,
    /// Optional active path hint for the context header.
    pub path_hint: Option<&'a str>,
    /// Token-ish budget using the v1 byte/4 approximation.
    pub max_tokens: usize,
    /// Session-start selection strategy. `Adaptive` (default) withholds only
    /// records explicitly tagged as non-startup and never drops untagged
    /// content; `Recency` includes everything in scope; `Relevance` applies the
    /// full inject classifier (may withhold untagged ambiguous globals).
    pub inject_strategy: inject::Strategy,
    /// Capture candidate-level include/skip decisions for debugging selection.
    pub explain: bool,
}

/// Assembled context output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextOutput {
    /// Markdown suitable for injection into an agent prompt or adapter include.
    pub markdown: String,
    /// Rendered sections included after filtering and budgeting.
    pub sections: Vec<ContextSection>,
    /// Candidate decisions captured when `ContextInput::explain` is true.
    pub decisions: Vec<ContextDecision>,
    /// Approximate token count for the rendered Markdown.
    pub estimated_tokens: usize,
    /// Per-record degradations (e.g. an unreadable canonical note that was
    /// skipped). Callers should surface these; they are not failures because
    /// one mid-sync record must not strip all memory from a session.
    pub warnings: Vec<ContextWarning>,
}

/// One non-fatal degradation encountered while assembling context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextWarning {
    /// Store-relative path of the record that was skipped.
    pub source_path: String,
    /// Human-readable cause.
    pub message: String,
}

/// One explain record for an indexed context candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextDecision {
    /// Memory id.
    pub id: String,
    /// Store-relative source path.
    pub source_path: String,
    /// `included` or `skipped`.
    pub action: &'static str,
    /// Stable reason key.
    pub reason: &'static str,
}

/// One rendered memory section in context output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSection {
    /// Memory id.
    pub id: String,
    /// Store alias that supplied this section.
    pub store: String,
    /// Memory scope used for filtering and rendered trust boundaries.
    pub scope: String,
    /// Trust label exposed in the data-boundary block.
    pub trust: TrustLevel,
    /// Explicit agent audience for agent-private data.
    ///
    /// Curated files and non-private notes leave this empty. Keeping it on the
    /// section contract lets JSON callers preserve visibility metadata without
    /// reparsing front matter or indexes.
    pub audience: Vec<String>,
    /// Store-relative source path.
    pub source_path: String,
    /// Section body exposed to context consumers.
    ///
    /// This is the escaped body as rendered inside the Markdown memory block.
    /// The raw canonical note remains on disk; context output is intentionally
    /// safe-to-inject data.
    pub body: String,
    /// Approximate tokens consumed by this section.
    pub estimated_tokens: usize,
}

/// Trust label for rendered memory data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    /// Human-reviewed or explicitly promoted curated Markdown.
    Curated,
    /// Explicit durable memory written by `hm remember`.
    Remembered,
    /// Lower-confidence raw inbox note written by `hm note`.
    Raw,
}

impl TrustLevel {
    /// Return the stable lowercase label used in context Markdown and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Curated => "curated",
            Self::Remembered => "remembered",
            Self::Raw => "raw",
        }
    }
}

/// Context assembly failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextError {
    /// Candidate note could not be read.
    ReadNote {
        /// Note path that failed.
        path: PathBuf,
        /// Original error rendered for diagnostics.
        message: String,
    },
    /// Candidate note could not be parsed.
    ParseNote {
        /// Note path that failed.
        path: PathBuf,
        /// Parse error.
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

impl Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadNote { path, message } => {
                write!(f, "failed to read note {}: {message}", path.display())
            }
            Self::ParseNote { path, message } => {
                write!(f, "failed to parse note {}: {message}", path.display())
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

impl Error for ContextError {}

impl From<crate::curated::CuratedError> for ContextError {
    fn from(value: crate::curated::CuratedError) -> Self {
        match value {
            crate::curated::CuratedError::ReadFile { path, message } => {
                Self::ReadNote { path, message }
            }
            crate::curated::CuratedError::ProjectAlias { path, message } => {
                Self::ProjectAlias { path, message }
            }
        }
    }
}

/// Assemble bounded Markdown context from canonical memory.
///
/// Raw/remembered inbox filtering is driven by index metadata, and modern index
/// entries carry the parsed body so hook paths do not need to reopen canonical
/// notes. Body-less legacy index entries fall back to canonical Markdown reads
/// and skip only that record if the note is unavailable. Curated files are read
/// directly from `memories/`, `people/`, and `rules/`. Each included body is
/// wrapped as data and escaped so memory content cannot forge or terminate the
/// boundary block that tells agents how to treat it.
pub fn assemble_context(input: ContextInput<'_>) -> Result<ContextOutput, ContextError> {
    assemble_context_with_mode(input, true)
}

/// Assemble context from an already-selected candidate set.
///
/// Prompt-specific recall first searches across configured sources and then
/// asks context rendering to inject only those selected hits. Reusing the normal
/// renderer keeps trust labels, escaping, budgets, and search-only filtering in
/// one place, while skipping broad curated collection prevents one curated hit
/// from pulling every curated file in scope into a prompt-specific recall block.
pub fn assemble_selected_context(input: ContextInput<'_>) -> Result<ContextOutput, ContextError> {
    assemble_context_with_mode(input, false)
}

fn assemble_context_with_mode(
    input: ContextInput<'_>,
    collect_curated: bool,
) -> Result<ContextOutput, ContextError> {
    let header = render_header(&input);
    let mut markdown = header;
    let mut sections = Vec::new();
    let mut decisions = Vec::new();
    let mut warnings = Vec::new();
    let mut estimated_tokens = estimate_tokens(&markdown);
    let project_ids = project_filter_ids(input.store_root, input.project_id)?;
    let mut seen_bodies = BTreeSet::new();

    if collect_curated && curated_source_allowed(input.sources) {
        for curated in crate::curated::collect(input.store_root, input.project_id)? {
            if !curated_scope_allowed(&curated, input.scopes) {
                continue;
            }
            let body = escape_memory_body(&curated.body);
            let block = render_curated_memory_block(input.store_name, &curated);
            let block_tokens = estimate_tokens(&block);
            if estimated_tokens + block_tokens > input.max_tokens {
                // First-fit-then-stop, matching the indexed-entry budget loop:
                // once a curated block overflows the budget, stop packing so a
                // later, lower-priority curated file (curated files are visited
                // in stable priority order) cannot jump ahead of the skipped
                // higher-priority one. The fits-in-budget case is unaffected.
                break;
            }

            markdown.push_str(&block);
            estimated_tokens += block_tokens;
            sections.push(ContextSection {
                id: curated.id,
                store: input.store_name.to_owned(),
                scope: curated.scope,
                trust: TrustLevel::Curated,
                audience: Vec::new(),
                source_path: curated.relative_path,
                body,
                estimated_tokens: block_tokens,
            });
        }
    }

    // Built once and reused; the Relevance strategy consults it per candidate.
    let markers = IncidentMarkers::default();
    let candidates = sorted_candidates(input.entries);

    // Supersession resolves over TWO distinct sets, kept separate on purpose:
    //
    //   suppressors = audience-allowed ∧ valid (NOT scope/project filtered)
    //   rendered    = all filters (source/scope/project/audience/validity)
    //
    // explicit links cross scope, heuristic same-scope.
    //
    // Suppressors must NOT be scope/project filtered: per the frozen contract,
    // an explicit `supersedes` correction is authoritative ACROSS scope, so a
    // global (or other-project) correction must still retire its target even for
    // a viewer who narrowed `--scope` and would never render the corrector. The
    // NL heuristic stays conservative because it independently requires the same
    // scope+project (`same_scope`), so widening the suppressor input can never
    // make it fire across scope.
    //
    // Suppressors MUST still be audience-allowed: a record the viewer cannot see
    // (another agent's `agent-private`) must never suppress one it can, or the
    // viewer would lose both the hidden corrector and the suppressed target.
    // Audience filtering therefore always wins over `superseded`.
    //
    // Suppressors MUST be valid: an expired or not-yet-valid corrector is itself
    // dropped from rendering, so letting it suppress a live older fact would
    // leave the viewer with neither (Fix A). This mirrors `search.rs`, which
    // filters validity before feeding the resolver.
    //
    // Suppressors MUST be source-allowed: a raw inbox note is excluded from
    // context by default, so it must not retire a durable remembered/curated
    // fact the viewer can actually see. Scope is widened for cross-scope
    // explicit links; source is not — a low-confidence note overriding canonical
    // memory would be a footgun, not authority.
    //
    // Context has no query, so the historical-recall exception never fires and
    // superseded survivors are always hidden. This uses the same shared resolver
    // as search.
    let suppressors = candidates
        .iter()
        .copied()
        .filter(|entry| {
            source_allowed(entry, input.sources, input.include_inbox)
                && visibility::audience_allows(entry, input.agent_id)
                && validity::allows_current(entry)
        })
        .collect::<Vec<_>>();
    let superseded = supersession::suppressed_ids(&suppressors, None);

    for entry in candidates {
        if !source_allowed(entry, input.sources, input.include_inbox) {
            push_decision(&mut decisions, &input, entry, "skipped", "source");
            continue;
        }
        if !scope_allowed(entry, input.scopes) {
            push_decision(&mut decisions, &input, entry, "skipped", "scope");
            continue;
        }
        if !project_allowed(entry, project_ids.as_ref()) {
            push_decision(&mut decisions, &input, entry, "skipped", "project");
            continue;
        }
        // Audience precedes `superseded`: a record the viewer cannot see is
        // recorded under its own visibility reason, never as superseded.
        if !visibility::audience_allows(entry, input.agent_id) {
            push_decision(&mut decisions, &input, entry, "skipped", "audience");
            continue;
        }
        if superseded.contains(&entry.id) {
            push_decision(&mut decisions, &input, entry, "skipped", "superseded");
            continue;
        }
        if !validity::allows_current(entry) {
            push_decision(&mut decisions, &input, entry, "skipped", "validity");
            continue;
        }

        // Prefer the indexed body: on a cloud-synced store the canonical note
        // can be mid-sync or already gone, and the rebuildable index is the
        // local copy the latency-sensitive hook path can trust. Canonical
        // reads remain only as a fallback for body-less entries from an older
        // index schema, and a fallback failure degrades to a per-record skip —
        // one unreadable record must not strip all memory from a session.
        let record_body = if entry.body.is_empty() {
            let note_path = input.store_root.join(&entry.note_path);
            match read_note_body(&note_path) {
                Ok(body) => body,
                Err(message) => {
                    warnings.push(ContextWarning {
                        source_path: entry.note_path.clone(),
                        message,
                    });
                    push_decision(&mut decisions, &input, entry, "skipped", "unreadable");
                    continue;
                }
            }
        } else {
            entry.body.clone()
        };
        // Withhold candidates the active strategy marks search-only so startup
        // context stays focused. Recency keeps everything; Relevance applies the
        // full content classifier; Adaptive (default) only withholds explicitly
        // non-startup kinds and never guesses against untagged content. The body
        // must be resolved first because Relevance's signal is content.
        if input.inject_strategy != inject::Strategy::Recency {
            let class = inject::select(
                input.inject_strategy,
                ClassifyInput {
                    scope: &entry.scope,
                    project_id: entry.project_id.as_deref(),
                    entry_kind: entry.entry_kind,
                    kind: entry.kind,
                    body: &record_body,
                },
                &markers,
            );
            if class == InjectClass::SearchOnly && !input.include_search_only {
                push_decision(&mut decisions, &input, entry, "skipped", "search-only");
                continue;
            }
        }
        if !seen_bodies.insert(duplicate_key(&record_body)) {
            push_decision(&mut decisions, &input, entry, "skipped", "duplicate");
            continue;
        }
        let trust = trust_for(entry);
        let body = escape_memory_body(&record_body);
        let block = render_memory_block(input.store_name, entry, trust, &record_body);
        let block_tokens = estimate_tokens(&block);
        if estimated_tokens + block_tokens > input.max_tokens {
            // Budget policy is first-fit-then-stop and MUST be identical whether
            // or not `explain` is set: the debug flag records diagnostics, it
            // never changes which memories are injected. Record the skip, then
            // stop packing so a later smaller candidate cannot jump ahead of the
            // overflowing higher-priority one it follows in sorted order.
            push_decision(&mut decisions, &input, entry, "skipped", "budget");
            break;
        }

        markdown.push_str(&block);
        estimated_tokens += block_tokens;
        sections.push(ContextSection {
            id: entry.id.clone(),
            store: input.store_name.to_owned(),
            scope: entry.scope.clone(),
            trust,
            audience: entry.audience.clone(),
            source_path: entry.note_path.clone(),
            body,
            estimated_tokens: block_tokens,
        });
        push_decision(&mut decisions, &input, entry, "included", "included");
    }

    Ok(ContextOutput {
        markdown,
        sections,
        decisions,
        estimated_tokens,
        warnings,
    })
}

/// Read and parse one canonical note body for a body-less index entry.
///
/// Returns a message instead of a `ContextError` because callers degrade to a
/// per-record warning rather than failing the assembly.
fn read_note_body(note_path: &Path) -> Result<String, String> {
    let contents = fs::read_to_string(note_path)
        .map_err(|err| format!("read note {}: {err}", note_path.display()))?;
    note::parse_note(&contents)
        .map(|parsed| parsed.body)
        .map_err(|err| format!("parse note: {err}"))
}

fn duplicate_key(body: &str) -> String {
    body.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn push_decision(
    decisions: &mut Vec<ContextDecision>,
    input: &ContextInput<'_>,
    entry: &IndexEntry,
    action: &'static str,
    reason: &'static str,
) {
    if !input.explain {
        return;
    }
    decisions.push(ContextDecision {
        id: entry.id.clone(),
        source_path: entry.note_path.clone(),
        action,
        reason,
    });
}

/// Estimate tokens with the v1 byte/4 heuristic.
///
/// This deliberately stays cheap and deterministic because `hm context` runs
/// from agent startup hooks. A future tokenizer can replace the implementation
/// without changing callers that only depend on the approximate budget contract.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

fn render_header(input: &ContextInput<'_>) -> String {
    let scopes = if input.scopes.is_empty() {
        "all".to_owned()
    } else {
        input.scopes.join(",")
    };
    let sources = if input.sources.is_empty() {
        "remembered".to_owned()
    } else {
        input.sources.join(",")
    };
    let agent = sanitize_header_value(input.agent_id.unwrap_or("unknown"));
    let project = sanitize_header_value(input.project_id.unwrap_or("none"));
    let path_hint = sanitize_header_value(input.path_hint.unwrap_or("none"));

    format!(
        "Hive Memory Context\nstore: {}\nagent: {agent}\nproject: {project}\npath: {path_hint}\nscopes: {scopes}\nsources: {sources}\n\n",
        sanitize_header_value(input.store_name)
    )
}

fn sorted_candidates(entries: &[IndexEntry]) -> Vec<&IndexEntry> {
    let mut entries = entries.iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        source_rank(left)
            .cmp(&source_rank(right))
            .then_with(|| confidence_rank(right.confidence).cmp(&confidence_rank(left.confidence)))
            .then_with(|| timestamp_rank(&right.created_at).cmp(&timestamp_rank(&left.created_at)))
            .then_with(|| left.note_path.cmp(&right.note_path))
    });
    entries
}

fn source_allowed(entry: &IndexEntry, sources: &[String], include_inbox: bool) -> bool {
    if sources.iter().any(|source| source == "all") {
        return true;
    }

    match entry_source(entry) {
        EntrySource::Curated => sources.iter().any(|source| source == "curated"),
        EntrySource::Remembered => {
            sources.is_empty() || sources.iter().any(|source| source == "remembered")
        }
        EntrySource::Inbox => include_inbox || sources.iter().any(|source| source == "inbox"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntrySource {
    Curated,
    Remembered,
    Inbox,
}

fn entry_source(entry: &IndexEntry) -> EntrySource {
    if entry.id.starts_with("curated:") {
        return EntrySource::Curated;
    }

    match entry.entry_kind {
        note::EntryKind::Remember => EntrySource::Remembered,
        note::EntryKind::Note => EntrySource::Inbox,
    }
}

fn curated_source_allowed(sources: &[String]) -> bool {
    sources
        .iter()
        .any(|source| source == "curated" || source == "all")
}

fn curated_scope_allowed(candidate: &CuratedFile, scopes: &[String]) -> bool {
    scopes.is_empty() || scopes.iter().any(|scope| scope == &candidate.scope)
}

fn scope_allowed(entry: &IndexEntry, scopes: &[String]) -> bool {
    scopes.is_empty() || scopes.iter().any(|scope| scope == &entry.scope)
}

fn project_filter_ids(
    store_root: &Path,
    project_id: Option<&str>,
) -> Result<Option<BTreeSet<String>>, ContextError> {
    // Project-scoped remembered notes use the id that was active at write time.
    // Resolve the alias family once per assembly so old notes stay visible
    // after repo renames without making each filter path reread metadata.
    project_id
        .map(|project_id| {
            project::related_project_ids(store_root, project_id).map_err(project_alias_error)
        })
        .transpose()
}

fn project_alias_error(err: project::ProjectError) -> ContextError {
    match err {
        project::ProjectError::Alias { path, message } => {
            ContextError::ProjectAlias { path, message }
        }
        other => ContextError::ProjectAlias {
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

fn trust_for(entry: &IndexEntry) -> TrustLevel {
    match entry_source(entry) {
        EntrySource::Curated => TrustLevel::Curated,
        EntrySource::Remembered => TrustLevel::Remembered,
        EntrySource::Inbox => TrustLevel::Raw,
    }
}

fn render_memory_block(
    store_name: &str,
    entry: &IndexEntry,
    trust: TrustLevel,
    body: &str,
) -> String {
    format!(
        "<memory id=\"{}\" agent=\"{}\" store=\"{}\" scope=\"{}\" trust=\"{}\">\n{}\n</memory>\n\n",
        escape_attr(&entry.id),
        escape_attr(&entry.agent_id),
        escape_attr(store_name),
        escape_attr(&entry.scope),
        trust.as_str(),
        escape_memory_body(body)
    )
}

fn render_curated_memory_block(store_name: &str, candidate: &CuratedFile) -> String {
    format!(
        "<memory id=\"{}\" agent=\"human\" store=\"{}\" scope=\"{}\" trust=\"{}\">\n{}\n</memory>\n\n",
        escape_attr(&candidate.id),
        escape_attr(store_name),
        escape_attr(&candidate.scope),
        TrustLevel::Curated.as_str(),
        escape_memory_body(&candidate.body)
    )
}

fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '\r' | '\n' | '\t' => ' ',
            _ => ch,
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn escape_attr(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '"' => escaped.push_str("&quot;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn escape_memory_body(body: &str) -> String {
    body.lines()
        .map(|line| {
            // Match on the trimmed line so an indented `   </memory>` is escaped
            // too: leading whitespace must not let a marker lookalike slip past.
            let trimmed = line.trim_start();
            if trimmed.starts_with("---")
                || trimmed.starts_with("+++")
                || trimmed.starts_with("<memory")
                || trimmed.starts_with("</memory")
            {
                // Prefix the ORIGINAL line with a backslash instead of dropping
                // content. Agents still see the literal data (indentation
                // preserved), but the line can no longer mimic front
                // matter/diff markers or the surrounding memory boundary.
                format!("\\{line}")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn source_rank(entry: &IndexEntry) -> usize {
    match entry_source(entry) {
        EntrySource::Curated => 0,
        EntrySource::Remembered => 1,
        EntrySource::Inbox => 2,
    }
}

fn confidence_rank(confidence: note::Confidence) -> usize {
    match confidence {
        note::Confidence::High => 3,
        note::Confidence::Medium => 2,
        note::Confidence::Low => 1,
    }
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
            "hive-memory-context-{name}-{}-{nanos}",
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
        project_id: Option<&'a str>,
        audience: Vec<String>,
    }

    fn write_record(record: TestRecord<'_>) {
        write_record_full(record, Vec::new());
    }

    /// Write a record with an explicit `supersedes` list, returning the created
    /// id so supersession tests can wire links between records.
    fn write_record_full(record: TestRecord<'_>, supersedes: Vec<String>) -> String {
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
            project_id: record.project_id.map(str::to_owned),
            subject: None,
            kind: None,
            valid_from: None,
            valid_to: None,
            supersedes,
            tags: Vec::new(),
            audience: record.audience,
            source_kind: None,
            source_ref: None,
            write_event: true,
            options: options(),
        })
        .expect("write memory")
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

    fn input<'a>(
        root: &'a Path,
        entries: &'a [IndexEntry],
        scopes: &'a [String],
        sources: &'a [String],
    ) -> ContextInput<'a> {
        ContextInput {
            store_name: "personal",
            store_root: root,
            entries,
            scopes,
            sources,
            include_inbox: false,
            include_search_only: false,
            agent_id: Some("codex"),
            project_id: None,
            path_hint: Some("/repo/src/main.rs"),
            max_tokens: 4000,
            inject_strategy: inject::Strategy::Recency,
            explain: false,
        }
    }

    #[test]
    fn assembles_remembered_memory_with_header_and_boundary() {
        let dir = temp_dir("remembered");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "Chris prefers TOML config.",
            created_at: timestamp(1_778_946_153),
            project_id: None,
            audience: Vec::new(),
        });
        let entries = entries(&root, &cache);

        let output = assemble_context(input(&root, &entries, &[], &["remembered".to_owned()]))
            .expect("context");

        assert_eq!(output.sections.len(), 1);
        assert!(output.markdown.contains("Hive Memory Context"));
        assert!(output.markdown.contains("store: personal"));
        let has_memory_body = output
            .markdown
            .contains("trust=\"remembered\">\nChris prefers TOML config.");
        assert!(has_memory_body);
    }

    #[test]
    fn includes_curated_global_and_matching_project_memory() {
        let dir = temp_dir("curated");
        let root = dir.join("store");
        let cache = dir.join("cache");
        fs::create_dir_all(root.join("memories/global")).expect("global dir");
        fs::create_dir_all(root.join("memories/projects/proj-a")).expect("project dir");
        fs::create_dir_all(root.join("memories/projects/proj-b")).expect("other project dir");
        fs::write(
            root.join("memories/global/MEMORY.md"),
            "Global curated fact.\n",
        )
        .expect("global memory");
        fs::write(
            root.join("memories/projects/proj-a/MEMORY.md"),
            "Project A curated fact.\n",
        )
        .expect("project memory");
        fs::write(
            root.join("memories/projects/proj-b/MEMORY.md"),
            "Project B curated fact.\n",
        )
        .expect("other project memory");
        let entries = entries(&root, &cache);
        let sources = ["curated".to_owned()];
        let mut request = input(&root, &entries, &[], &sources);
        request.project_id = Some("proj-a");

        let output = assemble_context(request).expect("context");

        assert_eq!(output.sections.len(), 2);
        assert!(output.markdown.contains("trust=\"curated\""));
        assert!(output.markdown.contains("Global curated fact."));
        assert!(output.markdown.contains("Project A curated fact."));
        assert!(!output.markdown.contains("Project B curated fact."));
    }

    #[test]
    fn context_follows_project_aliases_for_curated_files() {
        let dir = temp_dir("curated-alias");
        let root = dir.join("store");
        let cache = dir.join("cache");
        fs::create_dir_all(root.join("memories/projects/proj-current")).expect("current dir");
        fs::create_dir_all(root.join("memories/projects/proj-old")).expect("old dir");
        fs::write(
            root.join("memories/projects/proj-current/MEMORY.md"),
            "Current project curated fact.\n",
        )
        .expect("current memory");
        fs::write(
            root.join("memories/projects/proj-old/MEMORY.md"),
            "Old project curated fact.\n",
        )
        .expect("old memory");
        fs::write(
            root.join("memories/projects/proj-current/aliases.toml"),
            "schema_version = 1\nproject_id = \"proj-current\"\naliases = [\"proj-old\"]\n",
        )
        .expect("aliases");
        let entries = entries(&root, &cache);
        let sources = ["curated".to_owned()];
        let mut request = input(&root, &entries, &[], &sources);
        request.project_id = Some("proj-old");

        let output = assemble_context(request).expect("context");

        assert!(output.markdown.contains("Current project curated fact."));
        assert!(output.markdown.contains("Old project curated fact."));
    }

    #[test]
    fn excludes_raw_notes_unless_inbox_is_requested() {
        let dir = temp_dir("inbox");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Note,
            scope: "global",
            body: "Raw note.",
            created_at: timestamp(1_778_946_153),
            project_id: None,
            audience: Vec::new(),
        });
        let entries = entries(&root, &cache);
        let sources = vec!["remembered".to_owned()];
        let mut request = input(&root, &entries, &[], &sources);

        let default_sections = assemble_context(request.clone()).expect("context").sections;
        assert!(default_sections.is_empty());
        request.include_inbox = true;
        assert_eq!(
            assemble_context(request).expect("context").sections[0].trust,
            TrustLevel::Raw
        );
    }

    #[test]
    fn filters_project_scope_to_active_project() {
        let dir = temp_dir("project");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "project",
            body: "Project-specific memory.",
            created_at: timestamp(1_778_946_153),
            project_id: Some("repo-a"),
            audience: Vec::new(),
        });
        let entries = entries(&root, &cache);
        let scopes = vec!["project".to_owned()];
        let sources = vec!["remembered".to_owned()];
        let mut request = input(&root, &entries, &scopes, &sources);

        request.project_id = Some("repo-b");
        let wrong_project_sections = assemble_context(request.clone()).expect("context").sections;
        assert!(wrong_project_sections.is_empty());
        request.project_id = Some("repo-a");
        assert_eq!(
            assemble_context(request).expect("context").sections.len(),
            1
        );
    }

    #[test]
    fn context_suppresses_expired_and_future_records() {
        let dir = temp_dir("validity");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "Expired memory.",
            created_at: timestamp(1_778_946_153),
            project_id: None,
            audience: Vec::new(),
        });
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "Current memory.",
            created_at: timestamp(1_778_946_154),
            project_id: None,
            audience: Vec::new(),
        });
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "Future memory.",
            created_at: timestamp(1_778_946_155),
            project_id: None,
            audience: Vec::new(),
        });
        let mut entries = entries(&root, &cache);
        for entry in &mut entries {
            match entry.body.as_str() {
                "Expired memory." => {
                    entry.valid_to = Some("2000-01-01T00:00:00Z".to_owned());
                }
                "Future memory." => {
                    entry.valid_from = Some("2999-01-01T00:00:00Z".to_owned());
                }
                _ => {}
            }
        }
        let sources = ["remembered".to_owned()];
        let mut request = input(&root, &entries, &[], &sources);
        request.explain = true;

        let output = assemble_context(request).expect("context");

        assert_eq!(output.sections.len(), 1);
        assert_eq!(output.sections[0].body, "Current memory.");
        assert!(output.markdown.contains("Current memory."));
        assert!(!output.markdown.contains("Expired memory."));
        assert!(!output.markdown.contains("Future memory."));
        assert_eq!(
            output
                .decisions
                .iter()
                .filter(|decision| decision.reason == "validity")
                .count(),
            2
        );
    }

    #[test]
    fn context_follows_project_aliases_for_indexed_notes() {
        let dir = temp_dir("indexed-project-alias");
        let root = dir.join("store");
        let cache = dir.join("cache");
        fs::create_dir_all(root.join("memories/projects/repo-current")).expect("project dir");
        fs::write(
            root.join("memories/projects/repo-current/aliases.toml"),
            "schema_version = 1\nproject_id = \"repo-current\"\naliases = [\"repo-old\"]\n",
        )
        .expect("aliases");
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "project",
            body: "Remembered before the repo rename.",
            created_at: timestamp(1_778_946_153),
            project_id: Some("repo-old"),
            audience: Vec::new(),
        });
        let entries = entries(&root, &cache);
        let scopes = vec!["project".to_owned()];
        let sources = vec!["remembered".to_owned()];
        let mut request = input(&root, &entries, &scopes, &sources);
        request.project_id = Some("repo-current");

        let output = assemble_context(request).expect("context");

        assert_eq!(output.sections.len(), 1);
        let contains_renamed_memory = output
            .markdown
            .contains("Remembered before the repo rename.");
        assert!(contains_renamed_memory);
    }

    #[test]
    fn escapes_lines_that_can_confuse_memory_boundaries() {
        let dir = temp_dir("escape");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "---\n+++ patch\n<memory fake>\n</memory>",
            created_at: timestamp(1_778_946_153),
            project_id: None,
            audience: Vec::new(),
        });
        let entries = entries(&root, &cache);

        let output = assemble_context(input(&root, &entries, &[], &["remembered".to_owned()]))
            .expect("context");

        assert!(output.markdown.contains("\\---"));
        assert!(output.markdown.contains("\\+++ patch"));
        assert!(output.markdown.contains("\\<memory fake>"));
        assert!(output.markdown.contains("\\</memory>"));
        assert_eq!(
            output.sections[0].body,
            "\\---\n\\+++ patch\n\\<memory fake>\n\\</memory>"
        );
    }

    #[test]
    fn escapes_indented_boundary_lookalikes() {
        // Defense-in-depth: leading whitespace must not let a marker lookalike
        // slip past the escaper. An indented `   </memory>` is escaped too, with
        // the original indentation preserved after the backslash.
        let escaped = escape_memory_body("   </memory>");
        assert_eq!(escaped, "\\   </memory>");

        let multi = escape_memory_body("safe line\n\t<memory fake>\n   --- diff");
        assert_eq!(multi, "safe line\n\\\t<memory fake>\n\\   --- diff");
    }

    #[test]
    fn sanitizes_header_values_and_boundary_attributes() {
        let entry = IndexEntry {
            id: "id\"<&>".to_owned(),
            store_id: "store-id".to_owned(),
            entry_kind: note::EntryKind::Remember,
            scope: "global\"".to_owned(),
            project_id: None,
            audience: Vec::new(),
            tags: Vec::new(),
            subject: None,
            confidence: note::Confidence::High,
            valid_from: None,
            valid_to: None,
            supersedes: Vec::new(),
            kind: None,
            entities: Vec::new(),
            classified: None,
            agent_id: "co<dex".to_owned(),
            host_id: "taylor".to_owned(),
            created_at: "2026-05-16T00:00:00Z".to_owned(),
            body: "body".to_owned(),
            note_path: "inbox/notes/2026-05-16/id.md".to_owned(),
            event_path: None,
        };
        let block = render_memory_block("per&sonal", &entry, TrustLevel::Remembered, "body");

        assert_eq!(sanitize_header_value("repo\npath\tname"), "repo path name");
        assert!(block.contains("id=\"id&quot;&lt;&amp;&gt;\""));
        assert!(block.contains("agent=\"co&lt;dex\""));
        assert!(block.contains("store=\"per&amp;sonal\""));
        assert!(block.contains("scope=\"global&quot;\""));
    }

    #[test]
    fn respects_token_budget() {
        let dir = temp_dir("budget");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "This memory is too large for a tiny budget.",
            created_at: timestamp(1_778_946_153),
            project_id: None,
            audience: Vec::new(),
        });
        let entries = entries(&root, &cache);
        let sources = vec!["remembered".to_owned()];
        let mut request = input(&root, &entries, &[], &sources);
        request.max_tokens = estimate_tokens("Hive Memory Context\n") + 1;

        let tiny_budget_sections = assemble_context(request).expect("context").sections;
        assert!(tiny_budget_sections.is_empty());
    }

    #[test]
    fn relevance_strategy_withholds_operational_records() {
        let dir = temp_dir("relevance");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "Prefer fd over find.",
            created_at: timestamp(1_778_946_153),
            project_id: None,
            audience: Vec::new(),
        });
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "2026-06-06 root cause: a cron job leaked daemons.",
            created_at: timestamp(1_778_946_200),
            project_id: None,
            audience: Vec::new(),
        });
        let entries = entries(&root, &cache);
        let sources = ["remembered".to_owned()];

        // Recency (default) includes both records.
        let recency = assemble_context(input(&root, &entries, &[], &sources)).expect("context");
        assert_eq!(recency.sections.len(), 2);

        // Relevance withholds the operational record but keeps the preference.
        let mut request = input(&root, &entries, &[], &sources);
        request.inject_strategy = inject::Strategy::Relevance;
        let relevance = assemble_context(request).expect("context");
        assert_eq!(relevance.sections.len(), 1);
        assert!(relevance.markdown.contains("Prefer fd over find."));
        assert!(!relevance.markdown.contains("root cause"));
    }

    #[test]
    fn relevance_strategy_withholds_ambiguous_global_records() {
        let dir = temp_dir("relevance-ambiguous-global");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "Project shdeps rebuilds source-checkout binaries when the recorded version no longer matches checkout HEAD.",
            created_at: timestamp(1_778_946_153),
            project_id: None,
            audience: Vec::new(),
        });
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "The maintainer prefers agent-agnostic tooling.",
            created_at: timestamp(1_778_946_200),
            project_id: None,
            audience: Vec::new(),
        });
        let entries = entries(&root, &cache);
        let sources = ["remembered".to_owned()];
        let mut request = input(&root, &entries, &[], &sources);
        request.inject_strategy = inject::Strategy::Relevance;
        request.explain = true;

        let relevance = assemble_context(request).expect("context");

        assert_eq!(relevance.sections.len(), 1);
        assert!(
            relevance
                .markdown
                .contains("The maintainer prefers agent-agnostic tooling.")
        );
        assert!(!relevance.markdown.contains("Project shdeps rebuilds"));
        assert!(
            relevance.decisions.iter().any(|decision| {
                decision.action == "skipped" && decision.reason == "search-only"
            })
        );
    }

    #[test]
    fn context_uses_indexed_body_without_reopening_note() {
        // On a cloud-synced store a canonical note can vanish between index
        // build and context read (partial sync, remote delete). The index body
        // must be enough to keep session-start context alive.
        let dir = temp_dir("indexed-body");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "Indexed bodies keep hook context alive.",
            created_at: timestamp(1_778_946_153),
            project_id: None,
            audience: Vec::new(),
        });
        let entries = entries(&root, &cache);
        fs::remove_dir_all(root.join("inbox")).expect("remove canonical inbox");

        let sources = ["remembered".to_owned()];
        let output = assemble_context(input(&root, &entries, &[], &sources)).expect("context");
        assert_eq!(output.sections.len(), 1);
        assert!(
            output
                .markdown
                .contains("Indexed bodies keep hook context alive.")
        );
        assert!(output.warnings.is_empty());
    }

    #[test]
    fn unreadable_note_is_skipped_with_warning() {
        // A legacy body-less index entry whose canonical note is unreadable
        // must degrade to a per-record skip with a surfaced warning. Failing
        // the whole assembly would strip ALL memory from the session because
        // one record was mid-sync.
        let dir = temp_dir("unreadable-note");
        let root = dir.join("store");
        let cache = dir.join("cache");
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "Survivor memory stays available.",
            created_at: timestamp(1_778_946_153),
            project_id: None,
            audience: Vec::new(),
        });
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "Mid-sync memory is missing on disk.",
            created_at: timestamp(1_778_946_200),
            project_id: None,
            audience: Vec::new(),
        });
        let mut entries = entries(&root, &cache);
        let victim = entries
            .iter_mut()
            .find(|entry| entry.body.contains("Mid-sync"))
            .expect("victim entry");
        // Simulate an index produced before bodies were cached, pointing at a
        // note that never finished syncing to this machine.
        victim.body = String::new();
        let victim_path = victim.note_path.clone();
        fs::remove_file(root.join(&victim_path)).expect("remove victim note");

        let sources = ["remembered".to_owned()];
        let mut request = input(&root, &entries, &[], &sources);
        request.explain = true;
        let output = assemble_context(request).expect("context");

        assert_eq!(output.sections.len(), 1);
        assert!(output.markdown.contains("Survivor memory stays available."));
        assert!(!output.markdown.contains("Mid-sync"));
        assert_eq!(output.warnings.len(), 1);
        assert_eq!(output.warnings[0].source_path, victim_path);
        assert!(
            output
                .decisions
                .iter()
                .any(|decision| decision.action == "skipped" && decision.reason == "unreadable")
        );
    }

    #[test]
    fn context_suppresses_explicitly_superseded_record() {
        // The critical regression: context is the primary agent read path, so an
        // explicitly superseded record must not be injected as live remembered
        // memory while search hides it. Broad context shows only current truth.
        let dir = temp_dir("supersede-explicit");
        let root = dir.join("store");
        let cache = dir.join("cache");
        let old_id = write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "global",
                body: "Deployment gate is launchctl.",
                created_at: timestamp(1_778_946_153),
                project_id: None,
                audience: Vec::new(),
            },
            Vec::new(),
        );
        write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "global",
                body: "Deployment gate is deployctl.",
                created_at: timestamp(1_778_946_154),
                project_id: None,
                audience: Vec::new(),
            },
            vec![old_id.clone()],
        );
        let entries = entries(&root, &cache);
        let sources = ["remembered".to_owned()];
        let mut request = input(&root, &entries, &[], &sources);
        request.explain = true;

        let output = assemble_context(request).expect("context");

        assert_eq!(output.sections.len(), 1);
        assert!(output.markdown.contains("Deployment gate is deployctl."));
        assert!(!output.markdown.contains("Deployment gate is launchctl."));
        assert!(output.decisions.iter().any(|decision| {
            decision.id == old_id && decision.action == "skipped" && decision.reason == "superseded"
        }));
    }

    #[test]
    fn context_suppresses_explicit_link_across_scope_and_kind() {
        // An explicit link is authoritative even when the records differ in
        // scope and entry kind, where the conservative NL heuristic would refuse.
        let dir = temp_dir("supersede-cross");
        let root = dir.join("store");
        let cache = dir.join("cache");
        // Older lives in project scope; written as remember.
        let old_id = write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "project",
                body: "Release uses cargo-dist for project repo-a.",
                created_at: timestamp(1_778_946_153),
                project_id: Some("repo-a"),
                audience: Vec::new(),
            },
            Vec::new(),
        );
        // Newer is a global remembered correction explicitly superseding it.
        write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "global",
                body: "Release uses cargo-release everywhere now.",
                created_at: timestamp(1_778_946_154),
                project_id: None,
                audience: Vec::new(),
            },
            vec![old_id.clone()],
        );
        let entries = entries(&root, &cache);
        let sources = ["remembered".to_owned()];
        // Project scope must be eligible for the older record so the only thing
        // that hides it is supersession, not the project filter.
        let scopes = ["global".to_owned(), "project".to_owned()];
        let mut request = input(&root, &entries, &scopes, &sources);
        request.project_id = Some("repo-a");

        let output = assemble_context(request).expect("context");

        assert!(output.markdown.contains("Release uses cargo-release"));
        assert!(!output.markdown.contains("cargo-dist"));
    }

    #[test]
    fn invisible_record_cannot_suppress_visible_one() {
        // Bug 1: supersession must be resolved over the VIEWER-VISIBLE set only.
        // A newer agent-private correction (audience=["claude"]) that the active
        // agent `codex` cannot see must NOT hide the older global record it
        // supersedes, or codex would see neither the new nor the old fact.
        let dir = temp_dir("supersede-invisible");
        let root = dir.join("store");
        let cache = dir.join("cache");
        let old_id = write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "global",
                body: "Deployment gate is launchctl.",
                created_at: timestamp(1_778_946_153),
                project_id: None,
                audience: Vec::new(),
            },
            Vec::new(),
        );
        // Newer correction is private to a different agent ("claude").
        // `agent-private` scope is required for the audience filter to engage.
        write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "agent-private",
                body: "Deployment gate is deployctl.",
                created_at: timestamp(1_778_946_154),
                project_id: None,
                audience: vec!["claude".to_owned()],
            },
            vec![old_id.clone()],
        );
        let entries = entries(&root, &cache);
        let sources = ["remembered".to_owned()];
        // Active agent is codex (set by `input`), so the claude-private newer
        // record is filtered out before suppression is computed.
        let mut request = input(&root, &entries, &[], &sources);
        request.explain = true;

        let output = assemble_context(request).expect("context");

        // codex still sees the older global fact; the invisible newer one neither
        // appears nor suppresses it.
        assert_eq!(output.sections.len(), 1);
        assert!(output.markdown.contains("Deployment gate is launchctl."));
        assert!(!output.markdown.contains("Deployment gate is deployctl."));
        // The private newer record is skipped for audience, not rendered.
        assert!(
            output
                .decisions
                .iter()
                .any(|decision| { decision.action == "skipped" && decision.reason == "audience" })
        );
        // The older record is NOT recorded as superseded.
        assert!(
            !output
                .decisions
                .iter()
                .any(|decision| { decision.id == old_id && decision.reason == "superseded" })
        );
    }

    #[test]
    fn validity_failing_corrector_does_not_suppress_live_older() {
        // Fix A: an expired (or not-yet-valid) newer record with an explicit
        // `supersedes` link must NOT suppress the live older fact, because it is
        // itself dropped for validity in the emit loop. If it could suppress, the
        // viewer would see NEITHER the corrector (expired) nor the target
        // (suppressed). Validity must gate suppressors, mirroring search.
        let dir = temp_dir("supersede-expired-corrector");
        let root = dir.join("store");
        let cache = dir.join("cache");
        let old_id = write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "global",
                body: "Deployment gate is launchctl.",
                created_at: timestamp(1_778_946_153),
                project_id: None,
                audience: Vec::new(),
            },
            Vec::new(),
        );
        write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "global",
                body: "Deployment gate is deployctl.",
                created_at: timestamp(1_778_946_154),
                project_id: None,
                audience: Vec::new(),
            },
            vec![old_id.clone()],
        );
        let mut entries = entries(&root, &cache);
        // Expire the newer corrector so it fails `validity::allows_current`.
        for entry in &mut entries {
            if entry.body == "Deployment gate is deployctl." {
                entry.valid_to = Some("2000-01-01T00:00:00Z".to_owned());
            }
        }
        let sources = ["remembered".to_owned()];
        let mut request = input(&root, &entries, &[], &sources);
        request.explain = true;

        let output = assemble_context(request).expect("context");

        // The live older fact survives; the expired corrector neither renders nor
        // suppresses.
        assert_eq!(output.sections.len(), 1);
        assert!(output.markdown.contains("Deployment gate is launchctl."));
        assert!(!output.markdown.contains("Deployment gate is deployctl."));
        // The older record is NOT recorded as superseded.
        assert!(
            !output
                .decisions
                .iter()
                .any(|decision| { decision.id == old_id && decision.reason == "superseded" })
        );
    }

    #[test]
    fn inbox_note_does_not_suppress_remembered_fact_when_inbox_excluded() {
        // Suppressors are source-filtered: a raw inbox note carrying an explicit
        // `supersedes` link must NOT retire a durable remembered fact in context
        // when inbox is excluded (the default). Otherwise a low-confidence triage
        // note that the viewer never sees would silently hide canonical memory.
        let dir = temp_dir("supersede-inbox-source");
        let root = dir.join("store");
        let cache = dir.join("cache");
        let remembered_id = write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "global",
                body: "The deploy gate is deployctl.",
                created_at: timestamp(1_778_946_153),
                project_id: None,
                audience: Vec::new(),
            },
            Vec::new(),
        );
        // A newer raw inbox NOTE that explicitly supersedes the remembered fact.
        write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Note,
                scope: "global",
                body: "scratch: maybe the deploy gate changed?",
                created_at: timestamp(1_778_946_154),
                project_id: None,
                audience: Vec::new(),
            },
            vec![remembered_id.clone()],
        );
        let entries = entries(&root, &cache);
        // Default context sources exclude inbox, so the note is not a suppressor.
        let sources = ["remembered".to_owned()];
        let request = input(&root, &entries, &[], &sources);

        let output = assemble_context(request).expect("context");

        assert_eq!(output.sections.len(), 1);
        assert!(output.markdown.contains("The deploy gate is deployctl."));
        assert!(
            !output
                .decisions
                .iter()
                .any(|decision| decision.id == remembered_id && decision.reason == "superseded"),
            "inbox note must not suppress a remembered fact when inbox is excluded"
        );
    }

    #[test]
    fn cross_scope_corrector_retires_target_when_scope_excludes_corrector() {
        // Fix E: an explicit `supersedes` correction in a scope the viewer did
        // NOT select must still retire its target. The corrector lives in
        // `global`; the target in `project`. The viewer narrows `--scope project`,
        // so the global corrector is never rendered, yet the explicit link is
        // authoritative across scope and must still hide the stale target. The
        // suppressor set is audience-allowed ∧ valid but NOT scope-filtered.
        let dir = temp_dir("supersede-cross-scope-excluded");
        let root = dir.join("store");
        let cache = dir.join("cache");
        let old_id = write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "project",
                body: "Release uses cargo-dist for project repo-a.",
                created_at: timestamp(1_778_946_153),
                project_id: Some("repo-a"),
                audience: Vec::new(),
            },
            Vec::new(),
        );
        write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "global",
                body: "Release uses cargo-release everywhere now.",
                created_at: timestamp(1_778_946_154),
                project_id: None,
                audience: Vec::new(),
            },
            vec![old_id.clone()],
        );
        let entries = entries(&root, &cache);
        let sources = ["remembered".to_owned()];
        // Viewer selects ONLY project scope, excluding the global corrector from
        // rendering. Round-2's filter-then-suppress would have dropped the global
        // corrector from the suppressor set, leaving the stale target visible.
        let scopes = ["project".to_owned()];
        let mut request = input(&root, &entries, &scopes, &sources);
        request.project_id = Some("repo-a");
        request.explain = true;

        let output = assemble_context(request).expect("context");

        // The stale project-scoped target is retired by the out-of-scope global
        // corrector; the corrector itself is not rendered (scope-filtered).
        assert!(!output.markdown.contains("cargo-dist"));
        assert!(!output.markdown.contains("cargo-release"));
        assert!(output.decisions.iter().any(|decision| {
            decision.id == old_id && decision.action == "skipped" && decision.reason == "superseded"
        }));
    }

    #[test]
    fn context_supersession_cycle_keeps_one_record() {
        // Reciprocal explicit links must not erase both records. Exactly one
        // survives (the deterministic winner: newer by timestamp).
        let dir = temp_dir("supersede-cycle");
        let root = dir.join("store");
        let cache = dir.join("cache");
        let first_id = write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "global",
                body: "Cycle fact older.",
                created_at: timestamp(1_778_946_153),
                project_id: None,
                audience: Vec::new(),
            },
            Vec::new(),
        );
        let second_id = write_record_full(
            TestRecord {
                root: &root,
                entry_kind: note::EntryKind::Remember,
                scope: "global",
                body: "Cycle fact newer.",
                created_at: timestamp(1_778_946_154),
                project_id: None,
                audience: Vec::new(),
            },
            vec![first_id.clone()],
        );
        // Build entries, then inject the reciprocal link on the older record to
        // simulate a hand-edit/import that created an A↔B cycle.
        let mut entries = entries(&root, &cache);
        for entry in &mut entries {
            if entry.id == first_id {
                entry.supersedes = vec![second_id.clone()];
            }
        }
        let sources = ["remembered".to_owned()];
        let output = assemble_context(input(&root, &entries, &[], &sources)).expect("context");

        // Exactly one record survives, and it is the newer winner.
        assert_eq!(output.sections.len(), 1);
        assert!(output.markdown.contains("Cycle fact newer."));
        assert!(!output.markdown.contains("Cycle fact older."));
    }

    #[test]
    fn explain_does_not_change_emitted_context() {
        // A high-priority candidate overflows the budget and a later one would
        // fit. The budget/packing policy must be identical regardless of the
        // `explain` flag, so the injected Markdown and sections are byte-equal.
        let dir = temp_dir("explain-parity");
        let root = dir.join("store");
        let cache = dir.join("cache");
        // High confidence + newest sorts first; it is large enough to overflow.
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "This first high-priority memory is intentionally long enough to overflow the tiny budget once the header is counted.",
            created_at: timestamp(1_778_946_200),
            project_id: None,
            audience: Vec::new(),
        });
        // A later, smaller candidate that would fit on its own.
        write_record(TestRecord {
            root: &root,
            entry_kind: note::EntryKind::Remember,
            scope: "global",
            body: "Short later memory.",
            created_at: timestamp(1_778_946_100),
            project_id: None,
            audience: Vec::new(),
        });
        let entries = entries(&root, &cache);
        let sources = ["remembered".to_owned()];
        let mut base = input(&root, &entries, &[], &sources);

        // Size the budget so the small later block WOULD fit on its own after the
        // header, but the large first-sorted block does not. This is exactly the
        // input that exposes the old explain-divergent packing: skip-and-continue
        // would slip the small block in only when explain is true.
        let header_tokens = estimate_tokens(&render_header(&base));
        let small_entry = base
            .entries
            .iter()
            .find(|entry| entry.body == "Short later memory.")
            .expect("small entry");
        let small_block = render_memory_block(
            base.store_name,
            small_entry,
            TrustLevel::Remembered,
            &small_entry.body,
        );
        base.max_tokens = header_tokens + estimate_tokens(&small_block);

        let mut with_explain = base.clone();
        with_explain.explain = true;
        let mut without_explain = base.clone();
        without_explain.explain = false;

        let explained = assemble_context(with_explain).expect("context");
        let plain = assemble_context(without_explain).expect("context");

        // First-fit-then-stop: the overflowing first block stops packing, so the
        // later smaller block is NOT injected under either flag value, and the
        // emitted Markdown/sections are byte-identical regardless of explain.
        assert_eq!(explained.markdown, plain.markdown);
        assert_eq!(explained.sections, plain.sections);
        assert!(!plain.markdown.contains("Short later memory."));
        // Diagnostics still differ: only the explain run records decisions.
        assert!(!explained.decisions.is_empty());
        assert!(plain.decisions.is_empty());
    }
}
