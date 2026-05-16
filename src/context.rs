//! Context assembly for agent prompts and hook adapters.
//!
//! Context output is not canonical memory. It is a carefully labeled view over
//! canonical notes, designed to be safe to inject into an agent as data. The
//! data-boundary blocks and escaping here are part of that trust boundary.

use crate::index::IndexEntry;
use crate::{note, visibility};
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
    /// Active agent identity for agent-private audience filtering.
    pub agent_id: Option<&'a str>,
    /// Active project identity. When present, project-scoped notes must match it.
    pub project_id: Option<&'a str>,
    /// Optional active path hint for the context header.
    pub path_hint: Option<&'a str>,
    /// Token-ish budget using the v1 byte/4 approximation.
    pub max_tokens: usize,
}

/// Assembled context output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextOutput {
    /// Markdown suitable for injection into an agent prompt or adapter include.
    pub markdown: String,
    /// Rendered sections included after filtering and budgeting.
    pub sections: Vec<ContextSection>,
    /// Approximate token count for the rendered Markdown.
    pub estimated_tokens: usize,
}

/// One rendered memory section in context output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSection {
    /// Memory id.
    pub id: String,
    /// Store-relative source path.
    pub note_path: String,
    /// Trust label exposed in the data-boundary block.
    pub trust: TrustLevel,
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
    fn as_str(self) -> &'static str {
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
        }
    }
}

impl Error for ContextError {}

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
    let mut estimated_tokens = estimate_tokens(&markdown);

    if curated_source_allowed(input.sources) {
        for curated in curated_candidates(input.store_root, input.project_id)? {
            if !curated_scope_allowed(&curated, input.scopes) {
                continue;
            }
            let block = render_curated_memory_block(input.store_name, &curated);
            let block_tokens = estimate_tokens(&block);
            if estimated_tokens + block_tokens > input.max_tokens {
                continue;
            }

            markdown.push_str(&block);
            estimated_tokens += block_tokens;
            sections.push(ContextSection {
                id: curated.id,
                note_path: curated.relative_path,
                trust: TrustLevel::Curated,
                estimated_tokens: block_tokens,
            });
        }
    }

    for entry in sorted_candidates(input.entries) {
        if !source_allowed(entry, input.sources, input.include_inbox) {
            continue;
        }
        if !scope_allowed(entry, input.scopes) {
            continue;
        }
        if !project_allowed(entry, input.project_id) {
            continue;
        }
        if !visibility::audience_allows(entry, input.agent_id) {
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
        let trust = trust_for(entry.entry_kind);
        let block = render_memory_block(input.store_name, entry, trust, &parsed.body);
        let block_tokens = estimate_tokens(&block);
        if estimated_tokens + block_tokens > input.max_tokens {
            continue;
        }

        markdown.push_str(&block);
        estimated_tokens += block_tokens;
        sections.push(ContextSection {
            id: entry.id.clone(),
            note_path: entry.note_path.clone(),
            trust,
            estimated_tokens: block_tokens,
        });
    }

    Ok(ContextOutput {
        markdown,
        sections,
        estimated_tokens,
    })
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

struct CuratedCandidate {
    id: String,
    relative_path: String,
    scope: String,
    body: String,
}

fn curated_source_allowed(sources: &[String]) -> bool {
    sources
        .iter()
        .any(|source| source == "curated" || source == "all")
}

fn curated_candidates(
    store_root: &Path,
    project_id: Option<&str>,
) -> Result<Vec<CuratedCandidate>, ContextError> {
    let mut files = Vec::new();
    collect_curated_tree(store_root, Path::new("rules"), "global", &mut files)?;
    collect_curated_tree(store_root, Path::new("people"), "global", &mut files)?;
    collect_curated_tree(
        store_root,
        Path::new("memories/global"),
        "global",
        &mut files,
    )?;
    if let Some(project_id) = project_id {
        collect_curated_tree(
            store_root,
            &Path::new("memories/projects").join(project_id),
            "project",
            &mut files,
        )?;
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

fn collect_curated_tree(
    store_root: &Path,
    relative_root: &Path,
    scope: &str,
    files: &mut Vec<CuratedCandidate>,
) -> Result<(), ContextError> {
    let root = store_root.join(relative_root);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(ContextError::ReadNote {
                path: root,
                message: err.to_string(),
            });
        }
    };

    for entry in entries {
        let entry = entry.map_err(|err| ContextError::ReadNote {
            path: root.clone(),
            message: err.to_string(),
        })?;
        let path = entry.path();
        // Use the directory entry type so prompt assembly does not follow
        // symlinked curated files/directories outside the store. Symlink drift
        // belongs in doctor diagnostics, not in the context hot path.
        let file_type = entry.file_type().map_err(|err| ContextError::ReadNote {
            path: path.clone(),
            message: err.to_string(),
        })?;
        if file_type.is_dir() {
            let relative = path.strip_prefix(store_root).unwrap_or(&path);
            collect_curated_tree(store_root, relative, scope, files)?;
        } else if file_type.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("md")
        {
            let body = fs::read_to_string(&path).map_err(|err| ContextError::ReadNote {
                path: path.clone(),
                message: err.to_string(),
            })?;
            let relative_path = path_string(path.strip_prefix(store_root).unwrap_or(&path));
            files.push(CuratedCandidate {
                id: format!("curated:{relative_path}"),
                relative_path,
                scope: scope.to_owned(),
                body,
            });
        }
    }

    Ok(())
}

fn curated_scope_allowed(candidate: &CuratedCandidate, scopes: &[String]) -> bool {
    scopes.is_empty() || scopes.iter().any(|scope| scope == &candidate.scope)
}

fn scope_allowed(entry: &IndexEntry, scopes: &[String]) -> bool {
    scopes.is_empty() || scopes.iter().any(|scope| scope == &entry.scope)
}

fn project_allowed(entry: &IndexEntry, project_id: Option<&str>) -> bool {
    if entry.scope != "project" {
        return true;
    }

    let Some(project_id) = project_id else {
        return false;
    };

    entry.project_id.as_deref() == Some(project_id)
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

fn render_curated_memory_block(store_name: &str, candidate: &CuratedCandidate) -> String {
    format!(
        "<memory id=\"{}\" agent=\"human\" store=\"{}\" scope=\"{}\" trust=\"{}\">\n{}\n</memory>\n\n",
        escape_attr(&candidate.id),
        escape_attr(store_name),
        escape_attr(&candidate.scope),
        TrustLevel::Curated.as_str(),
        escape_memory_body(&candidate.body)
    )
}

fn path_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
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
            agent_id: Some("codex"),
            project_id: None,
            path_hint: Some("/repo/src/main.rs"),
            max_tokens: 4000,
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
            agent_id: "co<dex".to_owned(),
            created_at: "2026-05-16T00:00:00Z".to_owned(),
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
}
