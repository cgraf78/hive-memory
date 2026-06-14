//! Eval for mem0-style reconciliation, the gate for `hm capture --promote` /
//! auto-capture. It measures two things on a labeled synthetic corpus:
//!
//!  1. **Decision accuracy** — does `reconcile::reconcile` pick the labeled
//!     ADD/UPDATE/DELETE/NOOP operation for a candidate against existing memory?
//!  2. **End-to-end store quality** — ingesting an ordered fact stream with
//!     reconcile-promote (decide + apply via supersession) versus blind append:
//!     does a later query return the *current* fact and suppress the *stale*
//!     one, and is the store more compact?
//!
//! Both need a real model backend, so the scoreboard test is ignored by default.
//! Run with:
//!
//! ```console
//! HIVE_MEMORY_RECONCILE_BACKEND="claude -p" \
//!   cargo test --test reconcile_eval -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hive_memory::config::Sensitivity;
use hive_memory::index::{self, IndexEntry, RebuildIndexInput};
use hive_memory::llm::{self, Backend};
use hive_memory::memory::{self, WriteRecordInput};
use hive_memory::note::{Confidence, EntryKind};
use hive_memory::path::PathCase;
use hive_memory::reconcile::{self, ExistingMemory, Operation};
use hive_memory::search::{self, SearchInput};
use hive_memory::store::StoreManifest;
use hive_memory::write::{AtomicWriteOptions, FsyncPolicy};
use serde::Deserialize;
use time::OffsetDateTime;

const BACKEND_ENV: &str = "HIVE_MEMORY_RECONCILE_BACKEND";

#[derive(Debug, Deserialize)]
struct Corpus {
    #[serde(default)]
    decision: Vec<DecisionCase>,
    #[serde(default)]
    stream: Vec<StreamCase>,
}

#[derive(Debug, Deserialize)]
struct DecisionCase {
    candidate: String,
    expected: String,
    #[serde(default)]
    existing: Vec<ExistingRow>,
}

#[derive(Debug, Deserialize)]
struct ExistingRow {
    id: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct StreamCase {
    facts: Vec<String>,
    #[serde(default)]
    query: Vec<StreamQuery>,
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    ask: String,
    current: String,
    stale: String,
}

/// Stable operation label, matching the corpus `expected` vocabulary.
fn op_label(operation: &Operation) -> &'static str {
    match operation {
        Operation::Add => "add",
        Operation::Update { .. } => "update",
        Operation::Delete { .. } => "delete",
        Operation::Noop => "noop",
    }
}

/// Collapse update/delete to "supersede": both apply identically (write the
/// candidate, supersede the target), so this is the operationally meaningful
/// granularity even when the model picks update vs delete differently.
fn collapse(label: &str) -> &str {
    match label {
        "update" | "delete" => "supersede",
        other => other,
    }
}

#[test]
fn op_label_and_collapse_map_operations() {
    assert_eq!(op_label(&Operation::Add), "add");
    assert_eq!(
        op_label(&Operation::Update {
            target: "x".to_owned()
        }),
        "update"
    );
    assert_eq!(collapse("delete"), "supersede");
    assert_eq!(collapse("update"), "supersede");
    assert_eq!(collapse("add"), "add");
    assert_eq!(collapse("noop"), "noop");
}

#[test]
fn corpus_is_well_formed() {
    let corpus = load_corpus();
    assert!(!corpus.decision.is_empty(), "need decision cases");
    assert!(!corpus.stream.is_empty(), "need stream cases");
    for case in &corpus.decision {
        assert!(
            matches!(case.expected.as_str(), "add" | "update" | "delete" | "noop"),
            "bad expected op: {}",
            case.expected
        );
        if matches!(case.expected.as_str(), "update" | "delete") {
            assert!(
                !case.existing.is_empty(),
                "update/delete case needs an existing target"
            );
        }
    }
    for stream in &corpus.stream {
        assert!(stream.facts.len() >= 2, "stream needs >=2 facts");
        assert!(!stream.query.is_empty(), "stream needs queries");
    }
}

#[test]
#[ignore = "requires a real model backend (HIVE_MEMORY_RECONCILE_BACKEND or one on PATH)"]
fn reconcile_decision_and_store_quality_scoreboard() {
    let Some(backend) = resolve_backend() else {
        eprintln!("no model backend ({BACKEND_ENV} unset and none detected); skipping");
        return;
    };
    let corpus = load_corpus();
    let timeout = Duration::from_secs(120);

    // --- Part 1: decision accuracy -----------------------------------------
    let mut exact = 0usize;
    let mut collapsed = 0usize;
    for case in &corpus.decision {
        let existing: Vec<ExistingMemory> = case
            .existing
            .iter()
            .map(|row| ExistingMemory {
                id: row.id.clone(),
                text: row.text.clone(),
            })
            .collect();
        let op = match reconcile::reconcile(&backend, &case.candidate, &existing, timeout) {
            Ok(op) => op,
            Err(err) => {
                eprintln!("decision backend failed: {err}; skipping case");
                continue;
            }
        };
        let got = op_label(&op);
        if got == case.expected {
            exact += 1;
        }
        if collapse(got) == collapse(&case.expected) {
            collapsed += 1;
        }
    }
    let total = corpus.decision.len();
    let pct = |n: usize| {
        if total == 0 {
            0.0
        } else {
            n as f64 / total as f64
        }
    };
    eprintln!(
        "reconcile decision accuracy: exact {:.3} ({}/{}); collapsed(add/supersede/noop) {:.3} ({}/{})",
        pct(exact),
        exact,
        total,
        pct(collapsed),
        collapsed,
        total
    );

    // --- Part 2: end-to-end store quality ----------------------------------
    let mut blind_stale_leaks = 0usize;
    let mut reconcile_stale_leaks = 0usize;
    let mut blind_current_hits = 0usize;
    let mut reconcile_current_hits = 0usize;
    let mut queries = 0usize;
    let mut blind_records = 0usize;
    let mut reconcile_records = 0usize;

    for stream in &corpus.stream {
        let blind = ingest_blind(&stream.facts);
        let reconciled = ingest_reconcile(&stream.facts, &backend, timeout);
        blind_records += blind.record_count;
        reconcile_records += reconciled.record_count;

        for q in &stream.query {
            queries += 1;
            let (b_current, b_stale) = probe(&blind, q);
            let (r_current, r_stale) = probe(&reconciled, q);
            blind_current_hits += usize::from(b_current);
            reconcile_current_hits += usize::from(r_current);
            blind_stale_leaks += usize::from(b_stale);
            reconcile_stale_leaks += usize::from(r_stale);
        }
        cleanup(&blind);
        cleanup(&reconciled);
    }

    let qpct = |n: usize| {
        if queries == 0 {
            0.0
        } else {
            n as f64 / queries as f64
        }
    };
    eprintln!(
        "end-to-end ({queries} queries): current-fact retrieved blind {:.3} vs reconcile {:.3}; STALE-fact leaked blind {:.3} vs reconcile {:.3}",
        qpct(blind_current_hits),
        qpct(reconcile_current_hits),
        qpct(blind_stale_leaks),
        qpct(reconcile_stale_leaks),
    );
    eprintln!(
        "store compactness: blind kept {blind_records} records, reconcile kept {reconcile_records} (lower is tidier)"
    );
    eprintln!(
        "verdict: reconcile {} stale leakage and keeps {} the store size",
        if reconcile_stale_leaks < blind_stale_leaks {
            "REDUCES"
        } else {
            "does NOT reduce"
        },
        if reconcile_records < blind_records {
            "smaller"
        } else {
            "the same or larger"
        },
    );

    assert!(queries > 0, "every stream query failed at the backend");
}

struct StoreFixture {
    temp: PathBuf,
    root: PathBuf,
    cache: PathBuf,
    record_count: usize,
}

fn manifest() -> StoreManifest {
    StoreManifest::with_identity(
        "personal",
        Some("reconcile eval".to_owned()),
        Sensitivity::Private,
        "018f5f57-bd9b-7d33-9e21-1f44f0c5a013".to_owned(),
        "2026-05-16T00:00:00Z".to_owned(),
    )
}

fn options() -> AtomicWriteOptions {
    AtomicWriteOptions {
        fsync: FsyncPolicy::Never,
        ..AtomicWriteOptions::default()
    }
}

/// Write one durable record, optionally superseding prior ids.
fn write_remember(root: &Path, offset: usize, body: &str, supersedes: Vec<String>) -> String {
    let created_at =
        OffsetDateTime::from_unix_timestamp(1_780_000_000 + offset as i64).expect("timestamp");
    memory::write_record(WriteRecordInput {
        root,
        manifest: &manifest(),
        entry_kind: EntryKind::Remember,
        created_at,
        agent_id: "eval".to_owned(),
        host_id: "ci".to_owned(),
        user_id: "default".to_owned(),
        session_id: None,
        scope: "global".to_owned(),
        confidence: Confidence::Medium,
        body: body.to_owned(),
        project_id: None,
        subject: None,
        kind: None,
        valid_from: None,
        valid_to: None,
        supersedes,
        tags: Vec::new(),
        audience: Vec::new(),
        source_kind: None,
        source_ref: None,
        write_event: false,
        options: options(),
    })
    .expect("write record")
    .id
}

fn rebuild(root: &Path, cache: &Path) -> Vec<IndexEntry> {
    index::rebuild_index(RebuildIndexInput {
        store_name: "personal",
        store_root: root,
        cache_dir: cache,
        options: options(),
        path_case: PathCase::Sensitive,
    })
    .expect("rebuild index")
    .entries
}

/// Blind capture: every fact becomes its own durable record.
fn ingest_blind(facts: &[String]) -> StoreFixture {
    let temp = temp_dir();
    let root = temp.join("store");
    let cache = temp.join("cache");
    for (offset, fact) in facts.iter().enumerate() {
        write_remember(&root, offset, fact, Vec::new());
    }
    StoreFixture {
        temp,
        root,
        cache,
        record_count: facts.len(),
    }
}

/// Reconcile-promote: each fact is reconciled against the current store, then
/// applied (ADD writes; UPDATE/DELETE write + supersede the target; NOOP skips).
fn ingest_reconcile(facts: &[String], backend: &Backend, timeout: Duration) -> StoreFixture {
    let temp = temp_dir();
    let root = temp.join("store");
    let cache = temp.join("cache");
    let mut written = 0usize;
    for (offset, fact) in facts.iter().enumerate() {
        let entries = rebuild(&root, &cache);
        let hits = search::search(SearchInput {
            store_root: &root,
            entries: &entries,
            query: fact,
            scopes: &["global".to_owned()],
            sources: &["remembered".to_owned()],
            include_inbox: false,
            agent_id: None,
            project_id: None,
            limit: 5,
        })
        .expect("search");
        let existing: Vec<ExistingMemory> = hits
            .iter()
            .map(|hit| ExistingMemory {
                id: hit.entry.id.clone(),
                text: hit.entry.body.clone(),
            })
            .collect();
        let op = reconcile::reconcile(backend, fact, &existing, timeout).expect("reconcile");
        match op {
            Operation::Add => {
                write_remember(&root, offset, fact, Vec::new());
                written += 1;
            }
            Operation::Update { target } | Operation::Delete { target } => {
                write_remember(&root, offset, fact, vec![target]);
                written += 1;
            }
            Operation::Noop => {}
        }
    }
    StoreFixture {
        temp,
        root,
        cache,
        record_count: written,
    }
}

/// Query a store and report (current-fact retrieved, stale-fact leaked).
fn probe(store: &StoreFixture, query: &StreamQuery) -> (bool, bool) {
    let entries = rebuild(&store.root, &store.cache);
    let hits = search::search(SearchInput {
        store_root: &store.root,
        entries: &entries,
        query: &query.ask,
        scopes: &["global".to_owned()],
        sources: &["remembered".to_owned()],
        include_inbox: false,
        agent_id: None,
        project_id: None,
        limit: 10,
    })
    .expect("search");
    let bodies: Vec<String> = hits
        .iter()
        .map(|hit| hit.entry.body.to_lowercase())
        .collect();
    let current = bodies
        .iter()
        .any(|b| b.contains(&query.current.to_lowercase()));
    let stale = bodies
        .iter()
        .any(|b| b.contains(&query.stale.to_lowercase()));
    (current, stale)
}

fn cleanup(store: &StoreFixture) {
    let _ = std::fs::remove_dir_all(&store.temp);
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

fn load_corpus() -> Corpus {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/reconcile_corpus.toml");
    let text = std::fs::read_to_string(path).expect("read corpus");
    toml::from_str(&text).expect("parse corpus")
}

fn temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("hm-reconcile-eval-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}
