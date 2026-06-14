//! Eval for the auto-capture-on-stop policy — the gate that decides whether to
//! wire an agentguard `stop` hook that runs `hm capture --promote` automatically
//! at session end.
//!
//! `reconcile_eval` already proved the *mechanism* (reconcile picks the right
//! op; supersession suppresses stale facts). This eval proves the *policy*: when
//! whole conversation transcripts are auto-captured at session boundaries, does
//! cross-session recall actually improve, and does mem0-style promotion keep the
//! store clean versus blindly appending every extracted fact? It exercises the
//! extra step the policy adds — `capture::extract` over noisy transcripts — that
//! `reconcile_eval` skips by feeding pre-extracted atomic facts.
//!
//! Three arms per scenario, all driven by the same extraction backend:
//!   * **none** — no auto-capture; the store stays empty (recall baseline).
//!   * **blind** — extract facts and append every one as a durable record.
//!   * **promote** — extract, then reconcile each fact and apply (the real
//!     `hm capture --promote`: ADD writes; UPDATE/DELETE supersede).
//!
//! Gate (reported, not hard-asserted, since a real model is non-deterministic):
//! wire the hook only if promotion ADDS recall over `none`, does NOT lose recall
//! versus `blind`, and REDUCES stale leakage / store size versus `blind`.
//!
//! Needs a real model backend, so the scoreboard test is ignored by default:
//!
//! ```console
//! HIVE_MEMORY_RECONCILE_BACKEND="claude -p" \
//!   cargo test --test autocapture_eval -- --ignored --nocapture
//! ```
//!
//! Caveat for nested-CLI backends (`claude -p`, `codex`, `gemini`): these are
//! full agents, not clean completion endpoints. If they run under the operator's
//! own config they load that config's hooks/memory and editorialize instead of
//! returning the requested JSON, so extraction yields zero facts. Point them at
//! an isolated config (e.g. `CLAUDE_CONFIG_DIR=/tmp/clean`) — or use a real API
//! endpoint — so the backend behaves as a plain model. A clean run reports
//! non-zero `records kept`; all-zeros means the backend never returned parseable
//! output, not that the policy failed.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hive_memory::capture;
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
/// Neighbors weighed per fact when reconciling, matching `hm capture --promote`'s
/// default `--limit`.
const RECONCILE_LIMIT: usize = 5;
/// Hits considered when probing recall/leakage — a generous window so a missing
/// current fact reflects retrieval failure, not an overly tight cutoff.
const PROBE_LIMIT: usize = 10;

#[derive(Debug, Deserialize)]
struct Corpus {
    #[serde(default)]
    scenario: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
struct Scenario {
    name: String,
    sessions: Vec<String>,
    #[serde(default)]
    queries: Vec<Query>,
}

#[derive(Debug, Deserialize)]
struct Query {
    ask: String,
    current: String,
    stale: String,
}

#[test]
fn corpus_is_well_formed() {
    let corpus = load_corpus();
    assert!(!corpus.scenario.is_empty(), "need scenarios");
    for s in &corpus.scenario {
        assert!(!s.sessions.is_empty(), "{}: needs sessions", s.name);
        assert!(!s.queries.is_empty(), "{}: needs queries", s.name);
        for q in &s.queries {
            // The probe counts a stale leak by substring match, so a `stale`
            // token contained in `current` would make every current-fact hit
            // also register as a leak — guard against that authoring mistake.
            let current = q.current.to_lowercase();
            let stale = q.stale.to_lowercase();
            assert!(
                !current.contains(&stale),
                "{}: stale marker {:?} is a substring of current {:?}",
                s.name,
                q.stale,
                q.current
            );
        }
    }
}

/// Aggregated counts for one ingest policy across all scenario queries.
#[derive(Default)]
struct Scoreboard {
    current_hits: usize,
    stale_leaks: usize,
    records: usize,
}

#[test]
#[ignore = "requires a real model backend (HIVE_MEMORY_RECONCILE_BACKEND or one on PATH)"]
fn autocapture_recall_and_pollution_scoreboard() {
    let Some(backend) = resolve_backend() else {
        eprintln!("no model backend ({BACKEND_ENV} unset and none detected); skipping");
        return;
    };
    let corpus = load_corpus();
    // Generous per-call ceiling: a nested-CLI backend can pay heavy startup, and
    // a single slow call should not look like a model failure. The harness skips
    // (not panics) on a real timeout, so this only bounds worst-case latency.
    let timeout = Duration::from_secs(240);

    // Most-recent live records added to the reconcile candidate set in the
    // recency arm, on top of BM25 neighbors.
    const RECENCY_M: usize = 3;

    let mut none = Scoreboard::default();
    let mut blind = Scoreboard::default();
    let mut promote = Scoreboard::default();
    let mut promote_recency = Scoreboard::default();
    let mut queries = 0usize;
    let mut measured = 0usize;
    let mut skipped = 0usize;

    for scenario in &corpus.scenario {
        // Extract each session's facts ONCE, then drive both arms from the same
        // fact-lists. This halves model calls and — more importantly — isolates
        // the reconcile policy as the only difference between blind and promote,
        // so the comparison cannot be confounded by extraction variance.
        let Some(session_facts) = extract_sessions(&scenario.sessions, &backend, timeout) else {
            eprintln!("scenario {:?}: extraction failed; skipping", scenario.name);
            skipped += 1;
            continue;
        };
        let blind_store = ingest_blind(&session_facts);
        let promote_store = ingest_promote(&session_facts, &backend, timeout, 0);
        let recency_store = ingest_promote(&session_facts, &backend, timeout, RECENCY_M);
        blind.records += blind_store.record_count;
        promote.records += promote_store.record_count;
        promote_recency.records += recency_store.record_count;
        measured += 1;

        for q in &scenario.queries {
            queries += 1;
            // `none` keeps no store, so it can never retrieve or leak anything;
            // it is the floor that shows how much recall auto-capture adds.
            let (b_current, b_stale) = probe(&blind_store, q);
            let (p_current, p_stale) = probe(&promote_store, q);
            let (r_current, r_stale) = probe(&recency_store, q);
            blind.current_hits += usize::from(b_current);
            blind.stale_leaks += usize::from(b_stale);
            promote.current_hits += usize::from(p_current);
            promote.stale_leaks += usize::from(p_stale);
            promote_recency.current_hits += usize::from(r_current);
            promote_recency.stale_leaks += usize::from(r_stale);
        }
        cleanup(&blind_store);
        cleanup(&promote_store);
        cleanup(&recency_store);
    }
    // No-op stores never created; record the explicit zero baseline.
    none.records = 0;

    eprintln!("measured {measured} scenario(s), skipped {skipped} (backend failure)");
    assert!(
        queries > 0,
        "every scenario was skipped at the backend; no data"
    );
    let rate = |n: usize| n as f64 / queries as f64;

    eprintln!("auto-capture-on-stop scoreboard ({queries} queries):");
    eprintln!(
        "  current-fact recall:  none {:.3} | blind {:.3} | promote {:.3} | promote+recency {:.3}",
        rate(none.current_hits),
        rate(blind.current_hits),
        rate(promote.current_hits),
        rate(promote_recency.current_hits),
    );
    eprintln!(
        "  stale-fact leakage:   none {:.3} | blind {:.3} | promote {:.3} | promote+recency {:.3}",
        rate(none.stale_leaks),
        rate(blind.stale_leaks),
        rate(promote.stale_leaks),
        rate(promote_recency.stale_leaks),
    );
    eprintln!(
        "  records kept (lower is tidier): blind {} | promote {} | promote+recency {}",
        blind.records, promote.records, promote_recency.records
    );

    // The recency arm is the candidate to wire: judge it against the same rule
    // as before, and separately report whether recency beat plain promote.
    let gate = |arm: &Scoreboard| {
        let adds_recall = arm.current_hits > none.current_hits;
        let no_recall_loss = arm.current_hits >= blind.current_hits;
        let cuts_pollution = arm.stale_leaks < blind.stale_leaks && arm.records <= blind.records;
        (adds_recall, no_recall_loss, cuts_pollution)
    };
    let report = |label: &str, arm: &Scoreboard| {
        let (adds, keeps, cuts) = gate(arm);
        eprintln!(
            "  {label}: {} recall over none; {} recall vs blind; {} pollution vs blind => gate {}",
            if adds { "ADDS" } else { "does NOT add" },
            if keeps { "preserves" } else { "LOSES" },
            if cuts { "REDUCES" } else { "does NOT reduce" },
            if adds && keeps && cuts {
                "CLEARED"
            } else {
                "NOT cleared"
            },
        );
    };
    report("promote        ", &promote);
    report("promote+recency", &promote_recency);
    eprintln!(
        "  recency vs plain promote: stale leakage {} ({} -> {}), records {} ({} -> {})",
        if promote_recency.stale_leaks < promote.stale_leaks {
            "REDUCED"
        } else if promote_recency.stale_leaks > promote.stale_leaks {
            "WORSE"
        } else {
            "unchanged"
        },
        promote.stale_leaks,
        promote_recency.stale_leaks,
        if promote_recency.records < promote.records {
            "fewer"
        } else if promote_recency.records > promote.records {
            "more"
        } else {
            "same"
        },
        promote.records,
        promote_recency.records,
    );
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
        Some("autocapture eval".to_owned()),
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

/// Write one durable record, optionally superseding prior ids. `offset` keeps
/// created-at timestamps monotonic so ordering is deterministic.
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

/// Extract durable facts for every session transcript, once. Returns `None` if
/// any extraction fails (timeout or backend error), so the caller skips the
/// whole scenario rather than comparing arms over a partial fact-list.
fn extract_sessions(
    sessions: &[String],
    backend: &Backend,
    timeout: Duration,
) -> Option<Vec<Vec<String>>> {
    let mut out = Vec::with_capacity(sessions.len());
    for transcript in sessions {
        match capture::extract(backend, transcript, timeout) {
            Ok(facts) => out.push(facts),
            Err(err) => {
                eprintln!("  extract failed: {err}");
                return None;
            }
        }
    }
    Some(out)
}

/// Blind auto-capture: append every extracted fact as its own durable record,
/// with no reconciliation. Takes pre-extracted per-session facts so it shares
/// the exact same input as the promote arm.
fn ingest_blind(session_facts: &[Vec<String>]) -> StoreFixture {
    let temp = temp_dir();
    let root = temp.join("store");
    let cache = temp.join("cache");
    let mut offset = 0usize;
    for facts in session_facts {
        for fact in facts {
            write_remember(&root, offset, fact, Vec::new());
            offset += 1;
        }
    }
    let record_count = offset;
    StoreFixture {
        temp,
        root,
        cache,
        record_count,
    }
}

/// The `m` most-recent durable records that are not themselves superseded,
/// newest first. Used as a recency channel for reconcile candidates: an update
/// usually targets a recent fact, so this surfaces the stale predecessor even
/// when extraction paraphrased it beyond lexical (BM25) reach.
fn recent_durable(entries: &[IndexEntry], m: usize) -> Vec<ExistingMemory> {
    let superseded: std::collections::HashSet<&str> = entries
        .iter()
        .flat_map(|e| e.supersedes.iter().map(String::as_str))
        .collect();
    let mut live: Vec<&IndexEntry> = entries
        .iter()
        .filter(|e| e.entry_kind == EntryKind::Remember && !superseded.contains(e.id.as_str()))
        .collect();
    // created_at is RFC3339; lexical descending sort orders newest first.
    live.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    live.into_iter()
        .take(m)
        .map(|e| ExistingMemory {
            id: e.id.clone(),
            text: e.body.clone(),
        })
        .collect()
}

/// Promote auto-capture: reconcile each pre-extracted fact against the current
/// store and apply, mirroring `promote_captured_facts` — one index snapshot per
/// session (facts within a session do not see each other's writes), supersession
/// on UPDATE/DELETE, NOOP skipped. A reconcile failure skips that fact only.
///
/// `recent_m` controls the candidate channel: 0 means BM25 neighbors only (the
/// shipped behavior); >0 unions in the `recent_m` most-recent live records so
/// reconcile can still find a stale predecessor that paraphrase hid from BM25.
fn ingest_promote(
    session_facts: &[Vec<String>],
    backend: &Backend,
    timeout: Duration,
    recent_m: usize,
) -> StoreFixture {
    let temp = temp_dir();
    let root = temp.join("store");
    let cache = temp.join("cache");
    let mut offset = 0usize;
    let mut written = 0usize;
    for facts in session_facts {
        let entries = rebuild(&root, &cache);
        for fact in facts {
            let hits = search::search(SearchInput {
                store_root: &root,
                entries: &entries,
                query: fact,
                scopes: &["global".to_owned()],
                sources: &["remembered".to_owned()],
                include_inbox: false,
                agent_id: None,
                project_id: None,
                limit: RECONCILE_LIMIT,
            })
            .expect("search");
            let mut existing: Vec<ExistingMemory> = hits
                .iter()
                .map(|hit| ExistingMemory {
                    id: hit.entry.id.clone(),
                    text: hit.entry.body.clone(),
                })
                .collect();
            if recent_m > 0 {
                // Union the recency channel into the BM25 candidates, de-duping
                // by id so a record retrieved both ways is offered once.
                let seen: std::collections::HashSet<String> =
                    existing.iter().map(|e| e.id.clone()).collect();
                for cand in recent_durable(&entries, recent_m) {
                    if !seen.contains(&cand.id) {
                        existing.push(cand);
                    }
                }
            }
            // A single reconcile failure skips only this fact, so one slow call
            // doesn't discard the whole scenario's measurement.
            let op = match reconcile::reconcile(backend, fact, &existing, timeout) {
                Ok(op) => op,
                Err(err) => {
                    eprintln!("  reconcile failed: {err}; skipping fact");
                    continue;
                }
            };
            match op {
                Operation::Add => {
                    write_remember(&root, offset, fact, Vec::new());
                    offset += 1;
                    written += 1;
                }
                Operation::Update { target } | Operation::Delete { target } => {
                    write_remember(&root, offset, fact, vec![target]);
                    offset += 1;
                    written += 1;
                }
                Operation::Noop => {}
            }
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
fn probe(store: &StoreFixture, query: &Query) -> (bool, bool) {
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
        limit: PROBE_LIMIT,
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
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/autocapture_corpus.toml");
    let text = std::fs::read_to_string(path).expect("read corpus");
    toml::from_str(&text).expect("parse corpus")
}

fn temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "hm-autocapture-eval-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}
