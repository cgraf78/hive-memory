//! Deferred feature evals for memory mechanisms that should prove their value
//! before entering the hot hook path.
//!
//! This file is a scoreboard, not a product feature. The baseline candidate is
//! the current deterministic lexical/project-scoped implementation. Follow-up
//! branches can add candidate runners and compare them against these labels
//! without relying on anecdotes.

use hive_memory::config::Sensitivity;
use hive_memory::index::{self, IndexEntry, RebuildIndexInput};
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

#[derive(Debug, Deserialize)]
struct Corpus {
    record: Vec<Record>,
    retrieval_case: Vec<RetrievalCase>,
    supersession_case: Vec<SupersessionCase>,
    extraction_case: Vec<ExtractionCase>,
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
struct RetrievalCase {
    name: String,
    feature: String,
    query: String,
    project_id: Option<String>,
    expected: Vec<String>,
    forbidden: Vec<String>,
    target_recall_at_5: f64,
    target_precision_at_5: f64,
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SupersessionCase {
    name: String,
    project_id: Option<String>,
    old: String,
    new: String,
    expected_active: Vec<String>,
    expected_suppressed: Vec<String>,
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExtractionCase {
    name: String,
    project_id: Option<String>,
    input: String,
    expected_facts: Vec<String>,
    reject_terms: Vec<String>,
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug)]
struct Materialized {
    root: PathBuf,
    entries: Vec<IndexEntry>,
}

#[derive(Debug, Clone, PartialEq)]
struct RetrievalMetrics {
    feature: String,
    cases: usize,
    recall_at_5: f64,
    precision_at_5: f64,
    mrr: f64,
    forbidden_hits: usize,
    p95_ms: u128,
}

#[test]
fn deferred_retrieval_scoreboard_measures_candidate_quality() {
    let corpus = load_corpus();
    let materialized = materialize(&corpus);
    let metrics = score_retrieval(&corpus, &materialized, "lexical-baseline");
    for metric in &metrics {
        eprintln!(
            "deferred retrieval {} cases={} recall@5={:.3} precision@5={:.3} mrr={:.3} forbidden_hits={} p95_ms={}",
            metric.feature,
            metric.cases,
            metric.recall_at_5,
            metric.precision_at_5,
            metric.mrr,
            metric.forbidden_hits,
            metric.p95_ms
        );
    }

    let semantic = feature_metric(&metrics, "semantic");
    assert!(
        semantic.recall_at_5 >= 1.0 && semantic.precision_at_5 >= 1.0,
        "semantic candidate must clear the labeled paraphrase gate: {semantic:?}"
    );
    assert_eq!(
        semantic.forbidden_hits, 0,
        "semantic candidate must not add forbidden hits"
    );
    assert_eq!(
        feature_metric(&metrics, "scope").forbidden_hits,
        0,
        "baseline scope guard must keep inactive project hits out"
    );
    assert!(
        metrics
            .iter()
            .all(|metric| metric.p95_ms < u128::from(search_budget_ms())),
        "baseline search eval must stay fast: {metrics:?}"
    );
}

#[test]
fn deferred_feature_targets_are_actionable() {
    let corpus = load_corpus();
    assert!(
        corpus
            .retrieval_case
            .iter()
            .any(|case| case.feature == "semantic" && !case.expected.is_empty()),
        "semantic cases must name concrete expected memories"
    );
    assert!(
        corpus
            .retrieval_case
            .iter()
            .any(|case| case.feature == "entity" && !case.forbidden.is_empty()),
        "entity cases must include forbidden hits so scoping regressions are visible"
    );
    assert!(
        corpus.supersession_case.iter().all(|case| {
            !case.old.is_empty()
                && !case.new.is_empty()
                && !case.expected_active.is_empty()
                && !case.expected_suppressed.is_empty()
        }),
        "supersession cases must label both winners and suppressed memories"
    );
    assert!(
        corpus.extraction_case.iter().all(|case| {
            !case.input.is_empty()
                && !case.expected_facts.is_empty()
                && !case.reject_terms.is_empty()
        }),
        "extraction cases must label accepted facts and rejected noise"
    );
}

#[test]
fn deferred_supersession_eval_suppresses_stale_replacements() {
    let corpus = load_corpus();
    let materialized = materialize(&corpus);
    let subjects = subjects_by_name(&corpus);

    for case in &corpus.supersession_case {
        let hits = search(SearchInput {
            store_root: &materialized.root,
            entries: &materialized.entries,
            query: "before committing",
            scopes: &["global".to_owned(), "project".to_owned()],
            sources: &["remembered".to_owned()],
            include_inbox: false,
            agent_id: Some("codex"),
            project_id: case.project_id.as_deref(),
            limit: 20,
        })
        .unwrap_or_else(|err| panic!("supersession case {} search failed: {err}", case.name));
        let hit_subjects = hit_subjects(&hits);
        for expected in &case.expected_active {
            let subject = subjects
                .get(expected)
                .unwrap_or_else(|| panic!("unknown active subject {expected}"));
            assert!(
                hit_subjects.contains(subject),
                "supersession case {} missing active subject {expected}; hits={hit_subjects:?}",
                case.name
            );
        }
        for suppressed in &case.expected_suppressed {
            let subject = subjects
                .get(suppressed)
                .unwrap_or_else(|| panic!("unknown suppressed subject {suppressed}"));
            assert!(
                !hit_subjects.contains(subject),
                "supersession case {} included suppressed subject {suppressed}; hits={hit_subjects:?}",
                case.name
            );
        }
    }
}

#[test]
fn deferred_extraction_eval_documents_required_filters() {
    let corpus = load_corpus();
    for case in &corpus.extraction_case {
        assert!(
            case.project_id
                .as_deref()
                .is_some_and(|value| !value.is_empty()),
            "extraction case {} must name its project scope",
            case.name
        );
        let lower = case.input.to_ascii_lowercase();
        for fact in &case.expected_facts {
            assert!(
                lower.contains(&fact.to_ascii_lowercase()),
                "extraction case {} expected fact is not present in source input: {fact}",
                case.name
            );
        }
        for rejected in &case.reject_terms {
            assert!(
                lower.contains(&rejected.to_ascii_lowercase()),
                "extraction case {} reject term is not present in source input: {rejected}",
                case.name
            );
        }
    }
}

fn score_retrieval(
    corpus: &Corpus,
    materialized: &Materialized,
    candidate_name: &str,
) -> Vec<RetrievalMetrics> {
    let mut by_feature = BTreeMap::<String, Vec<CaseResult>>::new();
    for case in &corpus.retrieval_case {
        let start = Instant::now();
        let hits = search(SearchInput {
            store_root: &materialized.root,
            entries: &materialized.entries,
            query: &case.query,
            scopes: &["global".to_owned(), "project".to_owned()],
            sources: &["remembered".to_owned()],
            include_inbox: false,
            agent_id: Some("codex"),
            project_id: case.project_id.as_deref(),
            limit: 5,
        })
        .unwrap_or_else(|err| panic!("retrieval case {} failed: {err}", case.name));
        let elapsed = start.elapsed();
        let actual = hits
            .iter()
            .filter_map(|hit| hit.entry.subject.clone())
            .collect::<Vec<_>>();
        let result = CaseResult::score(case, &actual, elapsed);
        assert!(
            (0.0..=1.0).contains(&case.target_recall_at_5),
            "retrieval case {} has invalid recall target {}",
            case.name,
            case.target_recall_at_5
        );
        assert!(
            result.precision_at_5 <= case.target_precision_at_5 || case.feature == "semantic",
            "{candidate_name} unexpectedly exceeds precision target bookkeeping for {}",
            case.name
        );
        by_feature
            .entry(case.feature.clone())
            .or_default()
            .push(result);
    }

    by_feature
        .into_iter()
        .map(|(feature, results)| RetrievalMetrics {
            feature,
            cases: results.len(),
            recall_at_5: average(results.iter().map(|result| result.recall_at_5)),
            precision_at_5: average(results.iter().map(|result| result.precision_at_5)),
            mrr: average(results.iter().map(|result| result.reciprocal_rank)),
            forbidden_hits: results.iter().map(|result| result.forbidden_hits).sum(),
            p95_ms: p95(results
                .iter()
                .map(|result| result.elapsed)
                .collect::<Vec<_>>())
            .as_millis(),
        })
        .collect()
}

#[derive(Debug)]
struct CaseResult {
    recall_at_5: f64,
    precision_at_5: f64,
    reciprocal_rank: f64,
    forbidden_hits: usize,
    elapsed: Duration,
}

impl CaseResult {
    fn score(case: &RetrievalCase, actual: &[String], elapsed: Duration) -> Self {
        let expected = case.expected.iter().collect::<BTreeSet<_>>();
        let forbidden = case.forbidden.iter().collect::<BTreeSet<_>>();
        let actual_set = actual.iter().collect::<BTreeSet<_>>();
        let matched = expected
            .iter()
            .filter(|subject| actual_set.contains(**subject))
            .count();
        let unexpected = actual
            .iter()
            .filter(|subject| !expected.contains(subject))
            .count();
        let forbidden_hits = forbidden
            .iter()
            .filter(|subject| actual_set.contains(**subject))
            .count();
        let reciprocal_rank = actual
            .iter()
            .position(|subject| expected.contains(subject))
            .map(|index| 1.0 / (index + 1) as f64)
            .unwrap_or(0.0);

        Self {
            recall_at_5: if expected.is_empty() {
                1.0
            } else {
                matched as f64 / expected.len() as f64
            },
            precision_at_5: if actual.is_empty() {
                if expected.is_empty() { 1.0 } else { 0.0 }
            } else {
                (actual.len() - unexpected) as f64 / actual.len() as f64
            },
            reciprocal_rank,
            forbidden_hits,
            elapsed,
        }
    }
}

fn load_corpus() -> Corpus {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/deferred_feature_eval_corpus.toml");
    let text = fs::read_to_string(path).expect("read deferred feature corpus");
    toml::from_str(&text).expect("parse deferred feature corpus")
}

fn materialize(corpus: &Corpus) -> Materialized {
    let root = temp_dir("deferred-feature-eval").join("personal");
    fs::create_dir_all(&root).expect("create store root");
    let manifest = StoreManifest::with_identity(
        "personal",
        Some("Deferred feature eval memory".to_owned()),
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
            tags: vec!["deferred-feature-eval".to_owned()],
            audience: Vec::new(),
            source_kind: Some("fixture".to_owned()),
            source_ref: Some(record.subject.clone()),
            write_event: true,
            options: options.clone(),
        })
        .expect("write eval record");
    }

    let cache = temp_dir("deferred-feature-eval-cache");
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

fn subjects_by_name(corpus: &Corpus) -> BTreeMap<String, String> {
    corpus
        .record
        .iter()
        .map(|record| (record.subject.clone(), record.subject.clone()))
        .collect()
}

fn hit_subjects(hits: &[hive_memory::search::SearchHit]) -> BTreeSet<String> {
    hits.iter()
        .filter_map(|hit| hit.entry.subject.clone())
        .collect()
}

fn feature_metric<'a>(metrics: &'a [RetrievalMetrics], feature: &str) -> &'a RetrievalMetrics {
    metrics
        .iter()
        .find(|metric| metric.feature == feature)
        .unwrap_or_else(|| panic!("missing feature metric {feature}"))
}

fn average(values: impl Iterator<Item = f64>) -> f64 {
    let values = values.collect::<Vec<_>>();
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

fn p95(mut values: Vec<Duration>) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    values.sort();
    values[((values.len() * 95).div_ceil(100)).saturating_sub(1)]
}

fn search_budget_ms() -> u64 {
    150 * std::env::var("HIVE_MEMORY_PERF_BUDGET_MULTIPLIER")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1)
}

fn entry_kind(value: &str) -> EntryKind {
    match value {
        "remember" => EntryKind::Remember,
        "note" => EntryKind::Note,
        other => panic!("unknown entry_kind {other}"),
    }
}

fn confidence(value: &str) -> Confidence {
    match value {
        "low" => Confidence::Low,
        "medium" => Confidence::Medium,
        "high" => Confidence::High,
        other => panic!("unknown confidence {other}"),
    }
}

fn memory_kind(value: &str) -> MemoryKind {
    match value {
        "preference" => MemoryKind::Preference,
        "project-fact" => MemoryKind::ProjectFact,
        "incident" => MemoryKind::Incident,
        "reference" => MemoryKind::Reference,
        other => panic!("unknown memory kind {other}"),
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
