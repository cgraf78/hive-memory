//! End-to-end validation that the product Tantivy backend ([`SearchIndex`])
//! reproduces the BM25 retrieval win on the public LongMemEval-S benchmark.
//!
//! The hand-rolled BM25 experiment (`plans/bm25-experiment-results.md`) showed
//! statistical ranking lifts session-ingest recall@5 from the lexical baseline
//! of 0.781 to ~0.904. This test confirms the real engine — the code that will
//! ship — clears the same bar, so promoting Tantivy to the product is backed by
//! the engine's own number, not just the reference ranker's.
//!
//! Ignored by default: it needs the external LongMemEval-S fixture. Run with:
//!
//! ```console
//! scripts/download-longmemeval-fixture
//! HIVE_MEMORY_LONGMEMEVAL_S_JSON=target/public-evals/longmemeval_s_cleaned.json \
//!   cargo test --test retrieval_longmemeval -- --ignored --nocapture
//! ```

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hive_memory::retrieval::{SearchDocument, SearchIndex};
use serde::Deserialize;

const DATASET_ENV: &str = "HIVE_MEMORY_LONGMEMEVAL_S_JSON";
const MAX_CASES_ENV: &str = "HIVE_MEMORY_LONGMEMEVAL_MAX_CASES";
const DEFAULT_MAX_CASES: usize = 100;
const RETRIEVAL_LIMIT: usize = 5;
/// Recall floor the engine must clear. Set comfortably below the observed 0.904
/// so the gate proves the win (well above the 0.781 lexical baseline) without
/// being brittle to fixture-subset variation.
const RECALL_FLOOR: f64 = 0.85;
/// p95 latency budget per case, matching the lexical scoreboard's budget.
const P95_BUDGET_MS: u128 = 500;

#[derive(Debug, Deserialize)]
struct LongMemEvalCase {
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

#[test]
#[ignore = "requires HIVE_MEMORY_LONGMEMEVAL_S_JSON pointing at downloaded LongMemEval-S JSON"]
fn tantivy_backend_clears_recall_floor_on_longmemeval() {
    let Some(dataset_path) = std::env::var_os(DATASET_ENV).map(PathBuf::from) else {
        eprintln!("{DATASET_ENV} is not set; run scripts/download-longmemeval-fixture first");
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
    assert!(!cases.is_empty(), "LongMemEval-S had no scored cases");

    let mut recall_sum = 0.0;
    let mut precision_sum = 0.0;
    let mut reciprocal_rank_sum = 0.0;
    let mut non_answer_hits = 0usize;
    let mut latencies = Vec::with_capacity(cases.len());

    for case in &cases {
        // Session ingest: one indexed document per haystack session, mirroring
        // the lexical scoreboard's best-performing decomposition.
        let documents = case
            .haystack_session_ids
            .iter()
            .zip(case.haystack_sessions.iter())
            .map(|(session_id, turns)| SearchDocument {
                id: session_id.clone(),
                subject: None,
                tags: Vec::new(),
                body: render_session(turns),
            })
            .collect::<Vec<_>>();

        let index = SearchIndex::in_memory().expect("create index");
        index.rebuild(&documents).expect("rebuild index");

        let start = Instant::now();
        let hits = index
            .query(&case.question, RETRIEVAL_LIMIT)
            .expect("query index");
        latencies.push(start.elapsed());

        let retrieved = hits.iter().map(|hit| hit.id.clone()).collect::<Vec<_>>();
        let expected = case
            .answer_session_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();

        let matched = retrieved
            .iter()
            .filter(|session_id| expected.contains(*session_id))
            .count();
        let unexpected = retrieved.len() - matched;
        non_answer_hits += unexpected;
        recall_sum += matched as f64 / expected.len() as f64;
        precision_sum += if retrieved.is_empty() {
            0.0
        } else {
            matched as f64 / retrieved.len() as f64
        };
        reciprocal_rank_sum += retrieved
            .iter()
            .position(|session_id| expected.contains(session_id))
            .map(|index| 1.0 / (index + 1) as f64)
            .unwrap_or(0.0);
    }

    let count = cases.len() as f64;
    let recall = recall_sum / count;
    let precision = precision_sum / count;
    let mrr = reciprocal_rank_sum / count;
    let p95_ms = p95(latencies).as_millis();

    eprintln!(
        "Tantivy LongMemEval-S cases={} recall@5={recall:.3} precision@5={precision:.3} mrr={mrr:.3} non_answer_hits={non_answer_hits} p95_ms={p95_ms}",
        cases.len()
    );

    assert!(
        recall >= RECALL_FLOOR,
        "Tantivy recall@5 {recall:.3} fell below the {RECALL_FLOOR:.3} floor (lexical baseline is 0.781)"
    );
    let budget = P95_BUDGET_MS * perf_budget_multiplier();
    assert!(
        p95_ms <= budget,
        "Tantivy retrieval p95 {p95_ms}ms exceeded {budget}ms"
    );
}

fn render_session(turns: &[Turn]) -> String {
    turns
        .iter()
        .map(|turn| format!("{}: {}", turn.role, turn.content))
        .collect::<Vec<_>>()
        .join("\n")
}

fn load_cases(path: &std::path::Path) -> Vec<LongMemEvalCase> {
    let text = std::fs::read_to_string(path).expect("read LongMemEval-S JSON");
    serde_json::from_str(&text).expect("parse LongMemEval-S JSON")
}

fn max_cases() -> usize {
    std::env::var(MAX_CASES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CASES)
}

fn perf_budget_multiplier() -> u128 {
    std::env::var("HIVE_MEMORY_PERF_BUDGET_MULTIPLIER")
        .ok()
        .and_then(|value| value.parse::<u128>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1)
}

fn p95(mut values: Vec<Duration>) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    values.sort();
    values[((values.len() * 95).div_ceil(100)).saturating_sub(1)]
}
