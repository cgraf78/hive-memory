//! Retrieval-only adapter for the public LongMemEval-S benchmark.
//!
//! The test is ignored by default because the dataset is external and larger
//! than normal fixtures. It deliberately stops at retrieval metrics: answering
//! questions would add reader-model variance before we know whether Hive Memory
//! is finding the right evidence.

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

const DATASET_ENV: &str = "HIVE_MEMORY_LONGMEMEVAL_S_JSON";
const MAX_CASES_ENV: &str = "HIVE_MEMORY_LONGMEMEVAL_MAX_CASES";
const DEFAULT_MAX_CASES: usize = 100;
const RETRIEVAL_LIMIT: usize = 5;
const P95_BUDGET_MS: u128 = 500;

#[derive(Debug, Deserialize)]
struct LongMemEvalCase {
    question_id: String,
    question_type: String,
    question: String,
    answer_session_ids: Vec<String>,
    haystack_session_ids: Vec<String>,
    haystack_sessions: Vec<Vec<Turn>>,
}

#[derive(Debug, Clone, Deserialize)]
struct Turn {
    role: String,
    content: String,
}

#[derive(Debug)]
struct Materialized {
    root: PathBuf,
    entries_by_session_id: BTreeMap<String, IndexEntry>,
}

#[derive(Debug, Default)]
struct Score {
    cases: usize,
    recall_sum: f64,
    precision_sum: f64,
    reciprocal_rank_sum: f64,
    non_answer_hits: usize,
    latencies: Vec<Duration>,
}

impl Score {
    fn add(&mut self, expected: &BTreeSet<String>, actual: &[String], elapsed: Duration) {
        self.cases += 1;
        self.latencies.push(elapsed);

        let actual_set = actual.iter().collect::<BTreeSet<_>>();
        let matched = expected
            .iter()
            .filter(|session_id| actual_set.contains(*session_id))
            .count();
        let unexpected = actual
            .iter()
            .filter(|session_id| !expected.contains(session_id.as_str()))
            .count();
        self.non_answer_hits += unexpected;
        self.recall_sum += if expected.is_empty() {
            1.0
        } else {
            matched as f64 / expected.len() as f64
        };
        self.precision_sum += if actual.is_empty() {
            if expected.is_empty() { 1.0 } else { 0.0 }
        } else {
            (actual.len() - unexpected) as f64 / actual.len() as f64
        };
        self.reciprocal_rank_sum += actual
            .iter()
            .position(|session_id| expected.contains(session_id))
            .map(|index| 1.0 / (index + 1) as f64)
            .unwrap_or(0.0);
    }

    fn recall_at_5(&self) -> f64 {
        average(self.recall_sum, self.cases)
    }

    fn precision_at_5(&self) -> f64 {
        average(self.precision_sum, self.cases)
    }

    fn mrr(&self) -> f64 {
        average(self.reciprocal_rank_sum, self.cases)
    }

    fn p95_ms(&self) -> u128 {
        p95(self.latencies.clone()).as_millis()
    }
}

#[test]
#[ignore = "requires HIVE_MEMORY_LONGMEMEVAL_S_JSON pointing at downloaded LongMemEval-S JSON"]
fn longmemeval_s_retrieval_scoreboard() {
    let Some(dataset_path) = std::env::var_os(DATASET_ENV).map(PathBuf::from) else {
        eprintln!(
            "{DATASET_ENV} is not set; run scripts/download-longmemeval-fixture and set the env var"
        );
        return;
    };
    if !dataset_path.is_file() {
        eprintln!("{} does not exist; skipping", dataset_path.display());
        return;
    }

    let cases = load_cases(&dataset_path);
    let max_cases = max_cases();
    let cases = cases
        .into_iter()
        .filter(|case| !case.answer_session_ids.is_empty())
        .take(if max_cases == 0 {
            usize::MAX
        } else {
            max_cases
        })
        .collect::<Vec<_>>();
    assert!(
        !cases.is_empty(),
        "LongMemEval-S dataset had no scored cases"
    );

    let materialized = materialize(&cases);
    let mut overall = Score::default();
    let mut by_type = BTreeMap::<String, Score>::new();

    for case in &cases {
        let haystack_entries = case
            .haystack_session_ids
            .iter()
            .filter_map(|session_id| materialized.entries_by_session_id.get(session_id).cloned())
            .collect::<Vec<_>>();
        assert_eq!(
            haystack_entries.len(),
            case.haystack_session_ids.len(),
            "case {} missing haystack entries",
            case.question_id
        );

        let expected = case
            .answer_session_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let start = Instant::now();
        let hits = search(SearchInput {
            store_root: &materialized.root,
            entries: &haystack_entries,
            query: &case.question,
            scopes: &["global".to_owned()],
            sources: &["remembered".to_owned()],
            include_inbox: false,
            agent_id: Some("codex"),
            project_id: None,
            limit: RETRIEVAL_LIMIT,
        })
        .unwrap_or_else(|err| panic!("case {} search failed: {err}", case.question_id));
        let elapsed = start.elapsed();
        let actual = hits
            .iter()
            .filter_map(|hit| hit.entry.subject.clone())
            .collect::<Vec<_>>();

        overall.add(&expected, &actual, elapsed);
        by_type
            .entry(case.question_type.clone())
            .or_default()
            .add(&expected, &actual, elapsed);
    }

    eprintln!(
        "LongMemEval-S overall cases={} recall@5={:.3} precision@5={:.3} mrr={:.3} non_answer_hits={} p95_ms={}",
        overall.cases,
        overall.recall_at_5(),
        overall.precision_at_5(),
        overall.mrr(),
        overall.non_answer_hits,
        overall.p95_ms()
    );
    for (question_type, score) in by_type {
        eprintln!(
            "LongMemEval-S {question_type} cases={} recall@5={:.3} precision@5={:.3} mrr={:.3} non_answer_hits={} p95_ms={}",
            score.cases,
            score.recall_at_5(),
            score.precision_at_5(),
            score.mrr(),
            score.non_answer_hits,
            score.p95_ms()
        );
    }

    assert!(
        overall.p95_ms() <= perf_budget_ms(),
        "LongMemEval-S p95 {}ms exceeded {}ms",
        overall.p95_ms(),
        perf_budget_ms()
    );
}

fn load_cases(path: &Path) -> Vec<LongMemEvalCase> {
    let text = fs::read_to_string(path).expect("read LongMemEval-S JSON");
    serde_json::from_str(&text).expect("parse LongMemEval-S JSON")
}

fn materialize(cases: &[LongMemEvalCase]) -> Materialized {
    let root = temp_dir("public-longmemeval").join("personal");
    fs::create_dir_all(&root).expect("create store root");
    let manifest = StoreManifest::with_identity(
        "personal",
        Some("LongMemEval-S public eval memory".to_owned()),
        Sensitivity::Private,
        "018f5f57-bd9b-7d33-9e21-1f44f0c5a013".to_owned(),
        "2026-05-16T00:00:00Z".to_owned(),
    );
    let options = AtomicWriteOptions {
        fsync: FsyncPolicy::Never,
        ..AtomicWriteOptions::default()
    };
    let mut sessions = BTreeMap::<String, Vec<Turn>>::new();
    for case in cases {
        assert_eq!(
            case.haystack_session_ids.len(),
            case.haystack_sessions.len(),
            "case {} haystack ids/sessions length mismatch",
            case.question_id
        );
        for (session_id, session) in case
            .haystack_session_ids
            .iter()
            .zip(case.haystack_sessions.iter())
        {
            sessions
                .entry(session_id.clone())
                .or_insert_with(|| session.clone());
        }
    }

    for (index, (session_id, session)) in sessions.iter().enumerate() {
        let created_at =
            OffsetDateTime::from_unix_timestamp(1_778_946_153 + i64::try_from(index).unwrap())
                .expect("timestamp");
        memory::write_record(WriteRecordInput {
            root: &root,
            manifest: &manifest,
            entry_kind: EntryKind::Remember,
            created_at,
            agent_id: "eval".to_owned(),
            host_id: "ci".to_owned(),
            user_id: "default".to_owned(),
            session_id: None,
            scope: "global".to_owned(),
            confidence: Confidence::High,
            body: render_session(session),
            project_id: None,
            subject: Some(session_id.clone()),
            kind: Some(MemoryKind::Reference),
            tags: vec!["longmemeval-s".to_owned()],
            audience: Vec::new(),
            source_kind: Some("public-eval".to_owned()),
            source_ref: Some(session_id.clone()),
            write_event: true,
            options: options.clone(),
        })
        .expect("write LongMemEval session");
    }

    let cache = temp_dir("public-longmemeval-cache");
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
    let entries_by_session_id = report
        .entries
        .into_iter()
        .filter_map(|entry| entry.subject.clone().map(|subject| (subject, entry)))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        entries_by_session_id.len(),
        sessions.len(),
        "indexed session count mismatch"
    );

    Materialized {
        root,
        entries_by_session_id,
    }
}

fn render_session(session: &[Turn]) -> String {
    session
        .iter()
        .map(|turn| format!("{}: {}", turn.role, turn.content))
        .collect::<Vec<_>>()
        .join("\n")
}

fn max_cases() -> usize {
    std::env::var(MAX_CASES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CASES)
}

fn perf_budget_ms() -> u128 {
    P95_BUDGET_MS
        * std::env::var("HIVE_MEMORY_PERF_BUDGET_MULTIPLIER")
            .ok()
            .and_then(|value| value.parse::<u128>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(1)
}

fn average(sum: f64, count: usize) -> f64 {
    if count == 0 { 0.0 } else { sum / count as f64 }
}

fn p95(mut values: Vec<Duration>) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    values.sort();
    values[((values.len() * 95).div_ceil(100)).saturating_sub(1)]
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
