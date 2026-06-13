//! Retrieval quality evals for the Mem0-inspired recall work.
//!
//! These tests intentionally start as a baseline over today's local index,
//! search, and context paths. Later retrieval changes should improve the
//! corpus without making existing lexical/project/audience behavior worse.

use hive_memory::config::Sensitivity;
use hive_memory::context::{ContextInput, assemble_context};
use hive_memory::index::{self, IndexEntry, RebuildIndexInput};
use hive_memory::inject::Strategy as InjectStrategy;
use hive_memory::memory::{self, WriteRecordInput};
use hive_memory::note::{Confidence, EntryKind, MemoryKind};
use hive_memory::path::PathCase;
use hive_memory::search::{SearchInput, search};
use hive_memory::store::StoreManifest;
use hive_memory::write::{AtomicWriteOptions, FsyncPolicy};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

#[derive(Debug, Deserialize)]
struct Corpus {
    record: Vec<Record>,
    search_case: Vec<SearchCase>,
    context_case: Vec<ContextCase>,
}

#[derive(Debug, Deserialize)]
struct Record {
    subject: String,
    entry_kind: String,
    scope: String,
    project_id: Option<String>,
    confidence: String,
    kind: Option<String>,
    body: String,
}

#[derive(Debug, Deserialize)]
struct SearchCase {
    name: String,
    query: String,
    project_id: Option<String>,
    include: Vec<String>,
    exclude: Vec<String>,
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContextCase {
    name: String,
    project_id: Option<String>,
    include: Vec<String>,
    exclude: Vec<String>,
}

struct Materialized {
    root: PathBuf,
    entries: Vec<IndexEntry>,
}

#[test]
fn retrieval_corpus_search_baseline() {
    let corpus = load_corpus();
    let materialized = materialize(&corpus);

    for case in &corpus.search_case {
        let hits = search(SearchInput {
            store_root: &materialized.root,
            entries: &materialized.entries,
            query: &case.query,
            scopes: &["global".to_owned(), "project".to_owned()],
            sources: &["remembered".to_owned()],
            include_inbox: false,
            agent_id: Some("codex"),
            project_id: case.project_id.as_deref(),
            limit: 20,
        })
        .unwrap_or_else(|err| panic!("search case {} failed: {err}", case.name));
        let subjects = hit_subjects(&hits);
        assert_includes(&case.name, &subjects, &case.include);
        assert_excludes(&case.name, &subjects, &case.exclude);
    }
}

#[test]
fn retrieval_corpus_context_baseline() {
    let corpus = load_corpus();
    let materialized = materialize(&corpus);
    let bodies_by_subject = bodies_by_subject(&corpus);

    for case in &corpus.context_case {
        let output = assemble_context(ContextInput {
            store_name: "personal",
            store_root: &materialized.root,
            entries: &materialized.entries,
            scopes: &["global".to_owned(), "project".to_owned()],
            sources: &["remembered".to_owned(), "curated".to_owned()],
            include_inbox: false,
            include_search_only: false,
            agent_id: Some("codex"),
            project_id: case.project_id.as_deref(),
            path_hint: None,
            max_tokens: 4_000,
            inject_strategy: InjectStrategy::Relevance,
            explain: true,
        })
        .unwrap_or_else(|err| panic!("context case {} failed: {err}", case.name));
        let body = output.markdown;
        for expected in &case.include {
            let expected_body = bodies_by_subject
                .get(expected)
                .unwrap_or_else(|| panic!("unknown expected subject {expected}"));
            assert!(
                body.contains(expected_body.as_str()),
                "context case {} missing expected subject {expected}; body:\n{body}",
                case.name
            );
        }
        for forbidden in &case.exclude {
            let forbidden_body = bodies_by_subject
                .get(forbidden)
                .unwrap_or_else(|| panic!("unknown forbidden subject {forbidden}"));
            assert!(
                !body.contains(forbidden_body.as_str()),
                "context case {} included forbidden subject {forbidden}; body:\n{body}",
                case.name
            );
        }
        if case.name == "active project startup context" {
            let active_body = bodies_by_subject
                .get("active-agents-checkrun")
                .expect("active body");
            assert_eq!(
                body.matches(active_body).count(),
                1,
                "context case {} should emit the duplicate project fact once; body:\n{body}",
                case.name
            );
        }
    }
}

fn load_corpus() -> Corpus {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/retrieval_corpus.toml");
    let text = fs::read_to_string(path).expect("read retrieval corpus");
    toml::from_str(&text).expect("parse retrieval corpus")
}

fn materialize(corpus: &Corpus) -> Materialized {
    let root = temp_dir("retrieval-eval").join("personal");
    fs::create_dir_all(&root).expect("create store root");
    let manifest = StoreManifest::with_identity(
        "personal",
        Some("Retrieval eval memory".to_owned()),
        Sensitivity::Private,
        "018f5f57-bd9b-7d33-9e21-1f44f0c5a013".to_owned(),
        "2026-05-16T00:00:00Z".to_owned(),
    );
    let options = AtomicWriteOptions {
        fsync: FsyncPolicy::Never,
        ..AtomicWriteOptions::default()
    };

    for (index, record) in corpus.record.iter().enumerate() {
        let created_at =
            OffsetDateTime::from_unix_timestamp(1_778_946_153 + i64::try_from(index).unwrap())
                .expect("timestamp");
        memory::write_record(WriteRecordInput {
            root: &root,
            manifest: &manifest,
            entry_kind: entry_kind(&record.entry_kind),
            created_at,
            agent_id: "eval".to_owned(),
            host_id: "ci".to_owned(),
            user_id: "default".to_owned(),
            session_id: None,
            scope: record.scope.clone(),
            confidence: confidence(&record.confidence),
            body: record.body.clone(),
            project_id: record.project_id.clone(),
            subject: Some(record.subject.clone()),
            kind: record.kind.as_deref().map(memory_kind),
            valid_from: None,
            valid_to: None,
            supersedes: Vec::new(),
            tags: vec!["retrieval-eval".to_owned()],
            audience: Vec::new(),
            source_kind: Some("fixture".to_owned()),
            source_ref: Some(record.subject.clone()),
            write_event: true,
            options: options.clone(),
        })
        .expect("write eval record");
    }

    let cache = temp_dir("retrieval-eval-cache");
    let report = index::rebuild_index(RebuildIndexInput {
        store_name: "personal",
        store_root: &root,
        cache_dir: &cache,
        options,
        path_case: PathCase::Sensitive,
    })
    .expect("rebuild index");
    assert!(
        report.warnings.is_empty(),
        "index warnings: {:?}",
        report.warnings
    );

    Materialized {
        root,
        entries: report.entries,
    }
}

fn hit_subjects(hits: &[hive_memory::search::SearchHit]) -> BTreeSet<String> {
    hits.iter()
        .filter_map(|hit| hit.entry.subject.clone())
        .collect()
}

fn bodies_by_subject(corpus: &Corpus) -> BTreeMap<String, String> {
    corpus
        .record
        .iter()
        .map(|record| (record.subject.clone(), record.body.clone()))
        .collect()
}

fn assert_includes(case: &str, subjects: &BTreeSet<String>, expected: &[String]) {
    for subject in expected {
        assert!(
            subjects.contains(subject),
            "case {case} missing expected subject {subject}; hits: {subjects:?}"
        );
    }
}

fn assert_excludes(case: &str, subjects: &BTreeSet<String>, forbidden: &[String]) {
    for subject in forbidden {
        assert!(
            !subjects.contains(subject),
            "case {case} included forbidden subject {subject}; hits: {subjects:?}"
        );
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("hive-memory-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn entry_kind(value: &str) -> EntryKind {
    match value {
        "remember" => EntryKind::Remember,
        "note" => EntryKind::Note,
        other => panic!("unknown entry_kind in corpus: {other}"),
    }
}

fn confidence(value: &str) -> Confidence {
    match value {
        "high" => Confidence::High,
        "medium" => Confidence::Medium,
        "low" => Confidence::Low,
        other => panic!("unknown confidence in corpus: {other}"),
    }
}

fn memory_kind(value: &str) -> MemoryKind {
    match value {
        "preference" => MemoryKind::Preference,
        "project-fact" => MemoryKind::ProjectFact,
        "incident" => MemoryKind::Incident,
        "reference" => MemoryKind::Reference,
        other => panic!("unknown kind in corpus: {other}"),
    }
}
