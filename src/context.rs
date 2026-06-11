//! Context assembly for agent prompts and hook adapters.
//!
//! Context output is not canonical memory. It is a carefully labeled view over
//! canonical notes, designed to be safe to inject into an agent as data. The
//! data-boundary blocks and escaping here are part of that trust boundary.

use crate::curated::CuratedFile;
use crate::index::IndexEntry;
use crate::inject::{self, ClassifyInput, IncidentMarkers, InjectClass};
use crate::{note, project, visibility};
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
    /// Session-start selection strategy. `Recency` (default) keeps legacy
    /// include-all behavior; `Relevance` withholds search-only candidates via
    /// the inject classifier.
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
/// Raw/remembered inbox filtering is driven by index metadata, but bodies are
/// read from canonical Markdown files so the index remains a disposable cache.
/// Curated files are read directly from `memories/`, `people/`, and `rules/`.
/// Each included body is wrapped as data and escaped so memory content cannot
/// forge or terminate the boundary block that tells agents how to treat it.
pub fn assemble_context(input: ContextInput<'_>) -> Result<ContextOutput, ContextError> {
    let header = render_header(&input);
    let mut markdown = header;
    let mut sections = Vec::new();
    let mut decisions = Vec::new();
    let mut estimated_tokens = estimate_tokens(&markdown);
    let project_ids = project_filter_ids(input.store_root, input.project_id)?;

    if curated_source_allowed(input.sources) {
        for curated in crate::curated::collect(input.store_root, input.project_id)? {
            if !curated_scope_allowed(&curated, input.scopes) {
                continue;
            }
            let body = escape_memory_body(&curated.body);
            let block = render_curated_memory_block(input.store_name, &curated);
            let block_tokens = estimate_tokens(&block);
            if estimated_tokens + block_tokens > input.max_tokens {
                continue;
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
    for entry in sorted_candidates(input.entries) {
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
        if !visibility::audience_allows(entry, input.agent_id) {
            push_decision(&mut decisions, &input, entry, "skipped", "audience");
            continue;
        }

        let note_path = input.store_root.join(&entry.note_path);
        let contents = fs::read_to_string(&note_path).map_err(|err| ContextError::ReadNote {
            path: note_path.clone(),
            message: err.to_string(),
        })?;
        let parsed = note::parse_note(&contents).map_err(|err| ContextError::ParseNote {
            path: note_path,
            message: err.to_string(),
        })?;
        // Under Relevance, withhold candidates the classifier marks search-only
        // (operational logs, raw notes) so startup context stays focused. The
        // body must be read first because the operational signal is content.
        if input.inject_strategy == inject::Strategy::Relevance {
            let class = inject::classify(
                ClassifyInput {
                    scope: &entry.scope,
                    entry_kind: entry.entry_kind,
                    kind: entry.kind,
                    body: &parsed.body,
                },
                &markers,
            );
            if class == InjectClass::SearchOnly && !input.include_search_only {
                push_decision(&mut decisions, &input, entry, "skipped", "search-only");
                continue;
            }
        }
        let trust = trust_for(entry.entry_kind);
        let body = escape_memory_body(&parsed.body);
        let block = render_memory_block(input.store_name, entry, trust, &parsed.body);
        let block_tokens = estimate_tokens(&block);
        if estimated_tokens + block_tokens > input.max_tokens {
            push_decision(&mut decisions, &input, entry, "skipped", "budget");
            if !input.explain {
                break;
            }
            continue;
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
    })
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
        source_rank(left.entry_kind)
            .cmp(&source_rank(right.entry_kind))
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

    match entry.entry_kind {
        note::EntryKind::Remember => {
            sources.is_empty() || sources.iter().any(|source| source == "remembered")
        }
        note::EntryKind::Note => include_inbox || sources.iter().any(|source| source == "inbox"),
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

fn trust_for(entry_kind: note::EntryKind) -> TrustLevel {
    match entry_kind {
        note::EntryKind::Remember => TrustLevel::Remembered,
        note::EntryKind::Note => TrustLevel::Raw,
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
            if line.starts_with("---")
                || line.starts_with("+++")
                || line.starts_with("<memory")
                || line.starts_with("</memory")
            {
                // Prefix with a backslash instead of dropping content. Agents
                // still see the literal data, but the line can no longer mimic
                // front matter/diff markers or the surrounding memory boundary.
                format!("\\{line}")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn source_rank(entry_kind: note::EntryKind) -> usize {
    match entry_kind {
        note::EntryKind::Remember => 0,
        note::EntryKind::Note => 1,
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
            tags: Vec::new(),
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
            kind: None,
            agent_id: "co<dex".to_owned(),
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
}
