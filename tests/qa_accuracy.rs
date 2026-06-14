//! End-to-end QA-accuracy grader for LongMemEval-S.
//!
//! The cheap deterministic evals score *retrieval* (did we surface the answer
//! session). This grader closes the loop the project's metric philosophy depends
//! on: it actually answers each question from the retrieved context with a real
//! LLM, judges the answer against the gold answer with an LLM judge, and reports
//! whether retrieving the answer session predicts a correct answer. That
//! correlation is what justifies trusting `recall@5` as the fast proxy for the
//! end-to-end accuracy gate (see `plans/memory-improvements-roadmap.md`).
//!
//! Ignored by default: it needs the external fixture AND a real model backend,
//! so it is nondeterministic and not part of the normal suite. Run with:
//!
//! ```console
//! HIVE_MEMORY_LONGMEMEVAL_S_JSON=target/public-evals/longmemeval_s_cleaned.json \
//!   HIVE_MEMORY_QA_BACKEND="claude -p" \
//!   cargo test --test qa_accuracy -- --ignored --nocapture
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hive_memory::config::Sensitivity;
use hive_memory::index::{self, RebuildIndexInput};
use hive_memory::llm::{self, Backend};
use hive_memory::memory::{self, WriteRecordInput};
use hive_memory::note::{Confidence, EntryKind};
use hive_memory::path::PathCase;
use hive_memory::search::{self, SearchInput};
use hive_memory::store::StoreManifest;
use hive_memory::write::{AtomicWriteOptions, FsyncPolicy};
use serde::Deserialize;
use time::OffsetDateTime;

const DATASET_ENV: &str = "HIVE_MEMORY_LONGMEMEVAL_S_JSON";
const BACKEND_ENV: &str = "HIVE_MEMORY_QA_BACKEND";
const MAX_CASES_ENV: &str = "HIVE_MEMORY_QA_MAX_CASES";
const DEFAULT_MAX_CASES: usize = 15;
const RETRIEVAL_LIMIT: usize = 5;
const CONTEXT_CHAR_BUDGET: usize = 24000;

#[derive(Debug, Deserialize)]
struct LongMemEvalCase {
    question: String,
    // Gold answers are usually strings but some are bare numbers/booleans in the
    // fixture, so accept any JSON scalar and stringify it (see `answer_text`).
    answer: serde_json::Value,
    answer_session_ids: Vec<String>,
    haystack_session_ids: Vec<String>,
    haystack_sessions: Vec<Vec<Turn>>,
}

/// Render a gold answer as text regardless of its JSON scalar type.
fn answer_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Turn {
    role: String,
    content: String,
}

/// Prompt the reader model to answer strictly from the retrieved context.
fn answer_prompt(context: &str, question: &str) -> String {
    format!(
        "Answer the question using ONLY the context below. If the context does \
         not contain the answer, reply exactly UNKNOWN.\n\n\
         Context:\n{context}\n\nQuestion: {question}\nAnswer:"
    )
}

/// Prompt the judge model to compare a candidate answer to the gold answer.
fn judge_prompt(gold: &str, candidate: &str) -> String {
    format!(
        "You are grading an answer. Reply with a single word, YES or NO.\n\
         Does the candidate answer convey the same factual information as the \
         gold answer?\n\nGold answer: {gold}\nCandidate answer: {candidate}\n\
         Same factual answer (YES/NO):"
    )
}

/// Interpret a judge response as correct/incorrect. Looks for the first YES/NO
/// token so a chatty judge ("YES, because...") still grades cleanly; anything
/// without a clear YES is treated as incorrect.
fn judged_correct(judge_output: &str) -> bool {
    for token in judge_output.split(|ch: char| !ch.is_ascii_alphabetic()) {
        match token.to_ascii_lowercase().as_str() {
            "yes" => return true,
            "no" => return false,
            _ => {}
        }
    }
    false
}

fn render_session(turns: &[Turn]) -> String {
    turns
        .iter()
        .map(|turn| format!("{}: {}", turn.role, turn.content))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn answer_prompt_includes_context_and_question() {
    let prompt = answer_prompt("ctx text", "what?");
    assert!(prompt.contains("ctx text"));
    assert!(prompt.contains("what?"));
    assert!(prompt.contains("UNKNOWN"));
}

#[test]
fn judged_correct_reads_first_decision_token() {
    assert!(judged_correct("YES"));
    assert!(judged_correct("yes, the answers match"));
    assert!(!judged_correct("NO"));
    assert!(!judged_correct("no — different city"));
    assert!(!judged_correct("the model rambled without deciding"));
    // The first decisive token wins even with leading prose.
    assert!(judged_correct("Comparing the two... YES they match"));
}

#[derive(Debug, Default)]
struct Tally {
    cases: usize,
    correct: usize,
    hit_cases: usize,
    hit_correct: usize,
    miss_cases: usize,
    miss_correct: usize,
}

#[test]
#[ignore = "requires HIVE_MEMORY_LONGMEMEVAL_S_JSON and a real model backend on PATH"]
fn qa_accuracy_correlates_with_retrieval() {
    let Some(dataset_path) = std::env::var_os(DATASET_ENV).map(PathBuf::from) else {
        eprintln!("{DATASET_ENV} is not set; run scripts/download-longmemeval-fixture first");
        return;
    };
    let Some(backend) = resolve_backend() else {
        eprintln!("no usable model backend ({BACKEND_ENV} unset and none detected); skipping");
        return;
    };
    if !dataset_path.is_file() {
        eprintln!("{} does not exist; skipping", dataset_path.display());
        return;
    }

    let cases = load_cases(&dataset_path)
        .into_iter()
        .filter(|case| {
            !case.answer_session_ids.is_empty() && !answer_text(&case.answer).trim().is_empty()
        })
        .take(max_cases())
        .collect::<Vec<_>>();
    assert!(!cases.is_empty(), "no scored LongMemEval cases to grade");

    let timeout = Duration::from_secs(180);
    let mut tally = Tally::default();

    for case in &cases {
        let (retrieved, recall_hit) = retrieve(case);
        let context = build_context(case, &retrieved);

        let answer =
            match llm::invoke_raw(&backend, &answer_prompt(&context, &case.question), timeout) {
                Ok(text) => text,
                Err(err) => {
                    eprintln!("answer backend failed: {err}; skipping case");
                    continue;
                }
            };
        let judge_raw = match llm::invoke_raw(
            &backend,
            &judge_prompt(&answer_text(&case.answer), &answer),
            timeout,
        ) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("judge backend failed: {err}; skipping case");
                continue;
            }
        };
        let judged = judged_correct(&judge_raw);
        if std::env::var("HIVE_MEMORY_QA_DEBUG").is_ok() {
            eprintln!(
                "DEBUG ctx_len={} recall_hit={recall_hit}\n  gold={:?}\n  answer={:?}\n  judge={:?}",
                context.len(),
                answer_text(&case.answer),
                answer.trim(),
                judge_raw.trim(),
            );
        }

        tally.cases += 1;
        tally.correct += usize::from(judged);
        if recall_hit {
            tally.hit_cases += 1;
            tally.hit_correct += usize::from(judged);
        } else {
            tally.miss_cases += 1;
            tally.miss_correct += usize::from(judged);
        }
    }

    assert!(tally.cases > 0, "every graded case failed at the backend");
    let pct = |num: usize, den: usize| {
        if den == 0 {
            0.0
        } else {
            num as f64 / den as f64
        }
    };
    eprintln!(
        "QA accuracy: overall {:.3} ({}/{}); when answer session retrieved {:.3} ({}/{}); when missed {:.3} ({}/{})",
        pct(tally.correct, tally.cases),
        tally.correct,
        tally.cases,
        pct(tally.hit_correct, tally.hit_cases),
        tally.hit_correct,
        tally.hit_cases,
        pct(tally.miss_correct, tally.miss_cases),
        tally.miss_correct,
        tally.miss_cases,
    );
    eprintln!(
        "Proxy check: retrieval recall {} predict correctness (accuracy|hit {:.3} vs accuracy|miss {:.3}).",
        if pct(tally.hit_correct, tally.hit_cases) > pct(tally.miss_correct, tally.miss_cases) {
            "DOES"
        } else {
            "does NOT"
        },
        pct(tally.hit_correct, tally.hit_cases),
        pct(tally.miss_correct, tally.miss_cases),
    );
}

/// Lexically retrieve the top sessions for a case from a fresh per-case store.
/// Returns the retrieved session ids and whether any answer session was hit.
fn retrieve(case: &LongMemEvalCase) -> (Vec<String>, bool) {
    let temp = temp_dir();
    let root = temp.join("store");
    let cache = temp.join("cache");
    let manifest = StoreManifest::with_identity(
        "personal",
        Some("QA grader memory".to_owned()),
        Sensitivity::Private,
        "018f5f57-bd9b-7d33-9e21-1f44f0c5a013".to_owned(),
        "2026-05-16T00:00:00Z".to_owned(),
    );
    let options = AtomicWriteOptions {
        fsync: FsyncPolicy::Never,
        ..AtomicWriteOptions::default()
    };

    for (offset, (session_id, turns)) in case
        .haystack_session_ids
        .iter()
        .zip(case.haystack_sessions.iter())
        .enumerate()
    {
        let created_at = OffsetDateTime::from_unix_timestamp(1_780_000_000 + offset as i64)
            .expect("valid synthetic timestamp");
        memory::write_record(WriteRecordInput {
            root: &root,
            manifest: &manifest,
            entry_kind: EntryKind::Remember,
            created_at,
            agent_id: "qa".to_owned(),
            host_id: "ci".to_owned(),
            user_id: "default".to_owned(),
            session_id: Some(session_id.clone()),
            scope: "global".to_owned(),
            confidence: Confidence::High,
            body: render_session(turns),
            project_id: None,
            subject: Some(session_id.clone()),
            kind: None,
            valid_from: None,
            valid_to: None,
            supersedes: Vec::new(),
            tags: Vec::new(),
            audience: Vec::new(),
            source_kind: None,
            source_ref: None,
            write_event: false,
            options: options.clone(),
        })
        .expect("write session record");
    }

    let report = index::rebuild_index(RebuildIndexInput {
        store_name: "personal",
        store_root: &root,
        cache_dir: &cache,
        options,
        path_case: PathCase::Sensitive,
    })
    .expect("rebuild index");

    let hits = search::search(SearchInput {
        store_root: &root,
        entries: &report.entries,
        query: &case.question,
        scopes: &["global".to_owned()],
        sources: &["remembered".to_owned()],
        include_inbox: false,
        agent_id: Some("qa"),
        project_id: None,
        limit: RETRIEVAL_LIMIT,
    })
    .expect("search");

    let retrieved: Vec<String> = hits
        .iter()
        .filter_map(|hit| hit.entry.subject.clone())
        .collect();
    let answers: BTreeSet<&str> = case.answer_session_ids.iter().map(String::as_str).collect();
    let recall_hit = retrieved.iter().any(|id| answers.contains(id.as_str()));

    let _ = std::fs::remove_dir_all(&temp);
    (retrieved, recall_hit)
}

/// Build a bounded context string from the retrieved sessions' transcripts.
fn build_context(case: &LongMemEvalCase, retrieved: &[String]) -> String {
    let by_id: std::collections::HashMap<&str, &Vec<Turn>> = case
        .haystack_session_ids
        .iter()
        .map(String::as_str)
        .zip(case.haystack_sessions.iter())
        .collect();

    let mut context = String::new();
    for session_id in retrieved {
        if context.len() >= CONTEXT_CHAR_BUDGET {
            break;
        }
        if let Some(turns) = by_id.get(session_id.as_str()) {
            let block = render_session(turns);
            // Truncate the block to the remaining budget rather than dropping it
            // wholesale; otherwise a single long session larger than the budget
            // would leave the context empty and force a spurious UNKNOWN.
            let remaining = CONTEXT_CHAR_BUDGET - context.len();
            let slice: String = block.chars().take(remaining).collect();
            context.push_str(&slice);
            context.push_str("\n\n");
        }
    }
    context
}

fn resolve_backend() -> Option<Backend> {
    if let Ok(command) = std::env::var(BACKEND_ENV) {
        let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
        if !argv.is_empty() {
            return Some(Backend::command(argv));
        }
    }
    llm::detect(None, &[], None, None)
}

fn load_cases(path: &Path) -> Vec<LongMemEvalCase> {
    let text = std::fs::read_to_string(path).expect("read LongMemEval-S JSON");
    serde_json::from_str(&text).expect("parse LongMemEval-S JSON")
}

fn max_cases() -> usize {
    std::env::var(MAX_CASES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_CASES)
}

fn temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("hm-qa-grader-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}
