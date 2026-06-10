//! Injection selection eval (Phase 0).
//!
//! Measures how well a session-start selection *strategy* picks context from a
//! store, against hand-labeled ground truth. The point is to make selection
//! quality a number we can regress on BEFORE changing any real behavior: a
//! later strategy has to win precision without dropping anything important.
//!
//! Everything here is synthetic and committed (`tests/fixtures/`), so the eval
//! runs in CI with no private data. The same scoring is run against the real
//! store locally (uncommitted) before any default is ever flipped.
//!
//! The asymmetry that matters: a false negative (dropping a memory the agent
//! needed) is worse than a false positive (an extra memory that just costs
//! tokens). So we track `high_value_fn` — dropped preferences — separately and
//! hold it at zero.

use hive_memory::config::Sensitivity;
use hive_memory::context::{ContextInput, assemble_context};
use hive_memory::index::{self, IndexEntry, RebuildIndexInput};
use hive_memory::inject::{ClassifyInput, IncidentMarkers, InjectClass, classify};
use hive_memory::memory::{self, WriteRecordInput};
use hive_memory::note::{Confidence, EntryKind};
use hive_memory::path::PathCase;
use hive_memory::store::StoreManifest;
use hive_memory::write::{AtomicWriteOptions, FsyncPolicy};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Fixture model
// ---------------------------------------------------------------------------

/// One synthetic memory. `kind`/`note` are reviewer annotations the baseline
/// harness ignores; it keys on the same signals the real selector uses today.
#[derive(Debug, Deserialize)]
struct Record {
    subject: String,
    #[allow(dead_code)]
    kind: Option<String>,
    entry_kind: String,
    scope: String,
    project_id: Option<String>,
    confidence: String,
    body: String,
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Corpus {
    record: Vec<Record>,
}

/// One labeled session shape: the subjects that should and should not inject.
#[derive(Debug, Deserialize)]
struct LabeledContext {
    name: String,
    /// Empty string means "no active project".
    project_id: String,
    include: Vec<String>,
    exclude: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Labels {
    context: Vec<LabeledContext>,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load_corpus() -> Corpus {
    let text = fs::read_to_string(fixtures_dir().join("inject_corpus.toml")).expect("read corpus");
    toml::from_str(&text).expect("parse corpus")
}

fn load_labels() -> Labels {
    let text = fs::read_to_string(fixtures_dir().join("inject_labels.toml")).expect("read labels");
    toml::from_str(&text).expect("parse labels")
}

// ---------------------------------------------------------------------------
// Store materialization
// ---------------------------------------------------------------------------

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "hm-inject-eval-{name}-{}-{nanos}",
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

fn options() -> AtomicWriteOptions {
    AtomicWriteOptions {
        fsync: FsyncPolicy::Never,
        ..AtomicWriteOptions::default()
    }
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

/// Write the corpus into a fresh store and rebuild its index.
///
/// Records are written with `write_event = true` so the synthetic store carries
/// the same note+event shape as a real store (the index prefers event metadata),
/// keeping the eval honest about the path the hook actually exercises.
/// `created_at` is spread one second apart so recency ordering is deterministic.
fn materialize(corpus: &Corpus, root: &Path) -> Vec<IndexEntry> {
    let manifest = manifest();
    let base = 1_780_000_000_i64;
    for (offset, record) in corpus.record.iter().enumerate() {
        let created_at = OffsetDateTime::from_unix_timestamp(base + offset as i64)
            .expect("valid synthetic timestamp");
        memory::write_record(WriteRecordInput {
            root,
            manifest: &manifest,
            entry_kind: entry_kind(&record.entry_kind),
            created_at,
            agent_id: "eval-agent".to_owned(),
            host_id: "evalhost".to_owned(),
            user_id: "evaluser".to_owned(),
            session_id: None,
            scope: record.scope.clone(),
            confidence: confidence(&record.confidence),
            body: record.body.clone(),
            project_id: record.project_id.clone(),
            subject: Some(record.subject.clone()),
            tags: Vec::new(),
            audience: Vec::new(),
            source_kind: None,
            source_ref: None,
            write_event: true,
            options: options(),
        })
        .expect("write synthetic record");
    }

    index::rebuild_index(RebuildIndexInput {
        store_name: "personal",
        store_root: root,
        cache_dir: &root.join(".cache"),
        options: options(),
        path_case: PathCase::Sensitive,
    })
    .expect("rebuild index")
    .entries
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Selection strategies under evaluation. The eval scores them side by side so
/// a candidate's gains (and residual gaps) are measured against the baseline.
#[derive(Debug, Clone, Copy)]
enum Strategy {
    /// Current behavior: scope + source filtering, no relevance/inject control.
    Baseline,
    /// Baseline plus the read-time inject classifier: search-only candidates are
    /// dropped before assembly. Mutates nothing; pure pre-filter.
    Conservative,
}

/// Run a strategy for one session and return the subjects it would inject.
///
/// `bodies` maps record id to canonical body so the classifier can run without
/// re-reading files; the baseline ignores it.
fn inject(
    strategy: Strategy,
    root: &Path,
    entries: &[IndexEntry],
    bodies: &BTreeMap<String, String>,
    project_id: Option<&str>,
) -> BTreeSet<String> {
    let by_id: BTreeMap<&str, &str> = entries
        .iter()
        .filter_map(|e| e.subject.as_deref().map(|s| (e.id.as_str(), s)))
        .collect();

    // The classifier only ever removes candidates before the existing selector
    // runs, so assembly still owns ordering, project matching, and budgeting.
    let filtered: Vec<IndexEntry> = match strategy {
        Strategy::Baseline => entries.to_vec(),
        Strategy::Conservative => {
            let markers = IncidentMarkers::default();
            entries
                .iter()
                .filter(|entry| {
                    let class = classify(
                        ClassifyInput {
                            scope: &entry.scope,
                            entry_kind: entry.entry_kind,
                            body: bodies.get(&entry.id).map_or("", String::as_str),
                        },
                        &markers,
                    );
                    class != InjectClass::SearchOnly
                })
                .cloned()
                .collect()
        }
    };

    let scopes: [String; 0] = [];
    let sources = ["remembered".to_owned()];
    let output = assemble_context(ContextInput {
        store_name: "personal",
        store_root: root,
        entries: &filtered,
        scopes: &scopes,
        sources: &sources,
        include_inbox: false,
        agent_id: Some("eval-agent"),
        project_id,
        path_hint: Some("/repo/src/main.rs"),
        max_tokens: 4000,
    })
    .expect("assemble context");
    output
        .sections
        .iter()
        .filter_map(|s| by_id.get(s.id.as_str()).map(|s| (*s).to_owned()))
        .collect()
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Confusion-matrix counts for one session. Integer counts (not floats) are the
/// asserted contract so the baseline snapshot is exact and stable.
#[derive(Debug, Default, Clone, Copy)]
struct Score {
    tp: usize,
    fp: usize,
    fn_: usize,
    /// Dropped preferences — the failure we refuse to trade precision for.
    high_value_fn: usize,
}

/// A preference subject is high-value: dropping it changes how the agent works,
/// not just the token bill. Keyed by slug convention so the corpus stays the
/// single source of which records are preferences.
fn is_high_value(subject: &str) -> bool {
    subject.starts_with("pref-")
}

fn score_context(injected: &BTreeSet<String>, ctx: &LabeledContext) -> Score {
    let include: BTreeSet<&str> = ctx.include.iter().map(String::as_str).collect();
    let exclude: BTreeSet<&str> = ctx.exclude.iter().map(String::as_str).collect();
    let mut score = Score::default();
    for subject in &include {
        if injected.contains(*subject) {
            score.tp += 1;
        } else {
            score.fn_ += 1;
            if is_high_value(subject) {
                score.high_value_fn += 1;
            }
        }
    }
    for subject in &exclude {
        if injected.contains(*subject) {
            score.fp += 1;
        }
    }
    score
}

fn precision(tp: usize, fp: usize) -> f64 {
    if tp + fp == 0 {
        1.0
    } else {
        tp as f64 / (tp + fp) as f64
    }
}

fn recall(tp: usize, fn_: usize) -> f64 {
    if tp + fn_ == 0 {
        1.0
    } else {
        tp as f64 / (tp + fn_) as f64
    }
}

/// Run a strategy over every labeled context and return per-context scores plus
/// a micro-averaged aggregate. Prints a report (visible under `--nocapture`).
fn evaluate(strategy: Strategy) -> (Vec<(String, Score)>, Score) {
    let corpus = load_corpus();
    let labels = load_labels();
    let root = temp_dir("store");
    let entries = materialize(&corpus, &root);

    // Map id -> body via subject so the classifier sees canonical bodies without
    // re-reading files. Subjects are unique (asserted by fixtures_are_consistent).
    let body_by_subject: BTreeMap<&str, &str> = corpus
        .record
        .iter()
        .map(|r| (r.subject.as_str(), r.body.as_str()))
        .collect();
    let bodies: BTreeMap<String, String> = entries
        .iter()
        .filter_map(|e| {
            e.subject
                .as_deref()
                .and_then(|s| body_by_subject.get(s))
                .map(|body| (e.id.clone(), (*body).to_owned()))
        })
        .collect();

    let mut per_context = Vec::new();
    let mut total = Score::default();
    println!("\ninjection eval — strategy={strategy:?}");
    for ctx in &labels.context {
        let project = (!ctx.project_id.is_empty()).then_some(ctx.project_id.as_str());
        let injected = inject(strategy, &root, &entries, &bodies, project);
        let score = score_context(&injected, ctx);
        println!(
            "  {:<20} precision={:.3} recall={:.3}  tp={} fp={} fn={} hi-fn={}",
            ctx.name,
            precision(score.tp, score.fp),
            recall(score.tp, score.fn_),
            score.tp,
            score.fp,
            score.fn_,
            score.high_value_fn,
        );
        total.tp += score.tp;
        total.fp += score.fp;
        total.fn_ += score.fn_;
        total.high_value_fn += score.high_value_fn;
        per_context.push((ctx.name.clone(), score));
    }
    println!(
        "  {:<20} precision={:.3} recall={:.3}  tp={} fp={} fn={} hi-fn={}",
        "AGGREGATE",
        precision(total.tp, total.fp),
        recall(total.tp, total.fn_),
        total.tp,
        total.fp,
        total.fn_,
        total.high_value_fn,
    );

    let _ = fs::remove_dir_all(&root);
    (per_context, total)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Fixtures must stay self-consistent: every labeled subject exists in the
/// corpus, include/exclude are disjoint, and together they cover the whole
/// corpus (no record silently unjudged). This guards the ground truth itself.
#[test]
fn fixtures_are_consistent() {
    let corpus = load_corpus();
    let labels = load_labels();
    let corpus_subjects: BTreeSet<&str> =
        corpus.record.iter().map(|r| r.subject.as_str()).collect();
    assert_eq!(
        corpus_subjects.len(),
        corpus.record.len(),
        "corpus subjects must be unique"
    );

    for ctx in &labels.context {
        let include: BTreeSet<&str> = ctx.include.iter().map(String::as_str).collect();
        let exclude: BTreeSet<&str> = ctx.exclude.iter().map(String::as_str).collect();
        assert!(
            include.is_disjoint(&exclude),
            "{}: include and exclude overlap",
            ctx.name
        );
        let labeled: BTreeSet<&str> = include.union(&exclude).copied().collect();
        assert_eq!(
            labeled, corpus_subjects,
            "{}: labels must judge every corpus record exactly once",
            ctx.name
        );
    }
}

/// Baseline characterization: locks in exactly how the current selector behaves
/// so later strategies are measured against a fixed starting point.
///
/// The current selector injects every global remembered record plus
/// matching-project records. That yields perfect recall (nothing important
/// dropped) but over-includes the incidents and reference in every session, so
/// precision sits well below 1.0. The new strategy's job is to raise precision
/// to 1.0 while keeping `fn_` and `high_value_fn` at zero.
#[test]
fn baseline_characterization() {
    let (per_context, total) = evaluate(Strategy::Baseline);

    // Perfect recall, no dropped memory anywhere — the property a precision
    // win must not regress.
    for (name, score) in &per_context {
        assert_eq!(
            score.fn_, 0,
            "{name}: baseline unexpectedly dropped a memory"
        );
    }
    assert_eq!(total.fn_, 0, "baseline must drop nothing");
    assert_eq!(total.high_value_fn, 0, "baseline must drop no preference");

    // Exact confusion matrix: 19 correct inclusions, 12 over-inclusions
    // (3 incidents + 1 reference) injected across the 3 sessions.
    assert_eq!(total.tp, 19, "baseline true positives drifted");
    assert_eq!(total.fp, 12, "baseline false positives drifted");

    // Headline: high recall, mediocre precision (~0.613). This is the gap.
    assert!(
        precision(total.tp, total.fp) < 0.65,
        "baseline precision should be the low number we are trying to beat"
    );
    assert!((recall(total.tp, total.fn_) - 1.0).abs() < f64::EPSILON);
}

/// The read-time classifier must raise precision while keeping the baseline's
/// safety guarantees: drop nothing it should keep, and never drop a preference.
///
/// It catches the clearly dated+operational incidents but, by design, leaves in
/// a marker-less incident and a plain reference (a content heuristic cannot
/// safely tell those from durable facts). That residual is the measured case
/// for an explicit `kind` signal — the classifier is the safe bridge, not the
/// final answer. Crucially, the adversarial dated *preference* is NOT dropped.
#[test]
fn conservative_improves_precision_safely() {
    let (per_context, total) = evaluate(Strategy::Conservative);

    // Same safety floor as baseline: nothing important dropped anywhere.
    for (name, score) in &per_context {
        assert_eq!(score.fn_, 0, "{name}: classifier dropped a wanted memory");
    }
    assert_eq!(total.fn_, 0, "classifier must drop nothing wanted");
    assert_eq!(
        total.high_value_fn, 0,
        "classifier must never drop a preference (incl. the dated one)"
    );

    // Keeps every true positive the baseline had...
    assert_eq!(total.tp, 19, "classifier must not lose true positives");
    // ...and halves the over-inclusion: catches the 2 dated incidents in every
    // session (6 fewer false positives), leaving the marker-less incident and
    // the reference (2 records x 3 sessions = 6) as the honest residual.
    assert_eq!(
        total.fp, 6,
        "classifier should catch only the clearly dated incidents"
    );

    let baseline = evaluate(Strategy::Baseline).1;
    assert!(
        precision(total.tp, total.fp) > precision(baseline.tp, baseline.fp),
        "classifier must beat baseline precision"
    );
    // Still short of 1.0: content alone cannot close the gap safely.
    assert!(
        precision(total.tp, total.fp) < 1.0,
        "residual over-inclusion is expected and motivates an explicit kind"
    );
}
