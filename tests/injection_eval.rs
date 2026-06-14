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
use hive_memory::inject::Strategy as InjectStrategy;
use hive_memory::memory::{self, WriteRecordInput};
use hive_memory::note::{Confidence, EntryKind, MemoryKind};
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
    kind: Option<String>,
    /// When true, the synthetic store carries the explicit `kind` (simulating a
    /// write made after kind support). When false/absent, the record is stored
    /// untagged (legacy) and relies on the content heuristic.
    tagged: Option<bool>,
    entry_kind: String,
    scope: String,
    project_id: Option<String>,
    confidence: String,
    body: String,
    #[allow(dead_code)]
    note: Option<String>,
}

/// Map a corpus `kind` annotation to the schema enum. Only the kinds that can be
/// explicitly tagged are accepted; `raw-note` records are never tagged.
fn memory_kind(value: &str) -> MemoryKind {
    match value {
        "preference" => MemoryKind::Preference,
        "project-fact" => MemoryKind::ProjectFact,
        "incident" => MemoryKind::Incident,
        "reference" => MemoryKind::Reference,
        other => panic!("corpus record tagged with un-taggable kind: {other}"),
    }
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
        // Tagged records carry the explicit kind; untagged ones are stored bare
        // so the content-heuristic fallback is exercised on the same run.
        let kind = if record.tagged.unwrap_or(false) {
            record.kind.as_deref().map(memory_kind)
        } else {
            None
        };
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
            kind,
            valid_from: None,
            valid_to: None,
            supersedes: Vec::new(),
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

/// Run a strategy for one session and return the injected subjects mapped to the
/// tokens each consumed.
///
/// This drives the real production path: the strategy is handed to
/// `assemble_context` exactly as the live hook does, so the eval scores the
/// shipped selector rather than a parallel reimplementation. Returning the
/// per-section token cost lets the eval express the precision win in tokens (the
/// guardrail that keeps a recall change from quietly stuffing context).
fn inject(
    strategy: InjectStrategy,
    root: &Path,
    entries: &[IndexEntry],
    project_id: Option<&str>,
) -> BTreeMap<String, usize> {
    let by_id: BTreeMap<&str, &str> = entries
        .iter()
        .filter_map(|e| e.subject.as_deref().map(|s| (e.id.as_str(), s)))
        .collect();

    let scopes: [String; 0] = [];
    let sources = ["remembered".to_owned()];
    let output = assemble_context(ContextInput {
        store_name: "personal",
        store_root: root,
        entries,
        scopes: &scopes,
        sources: &sources,
        include_inbox: false,
        include_search_only: false,
        agent_id: Some("eval-agent"),
        project_id,
        path_hint: Some("/repo/src/main.rs"),
        max_tokens: 4000,
        inject_strategy: strategy,
        explain: false,
    })
    .expect("assemble context");
    output
        .sections
        .iter()
        .filter_map(|section| {
            by_id
                .get(section.id.as_str())
                .map(|subject| ((*subject).to_owned(), section.estimated_tokens))
        })
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
    /// Tokens across all injected sections (true and false positives).
    injected_tokens: usize,
    /// Tokens spent on false positives — context noise the agent pays for every
    /// turn. The standing guardrail: a recall change must not inflate this.
    wasted_tokens: usize,
}

/// A preference subject is high-value: dropping it changes how the agent works,
/// not just the token bill. Keyed by slug convention so the corpus stays the
/// single source of which records are preferences.
fn is_high_value(subject: &str) -> bool {
    subject.starts_with("pref-")
}

fn score_context(injected: &BTreeMap<String, usize>, ctx: &LabeledContext) -> Score {
    let include: BTreeSet<&str> = ctx.include.iter().map(String::as_str).collect();
    let exclude: BTreeSet<&str> = ctx.exclude.iter().map(String::as_str).collect();
    let mut score = Score {
        injected_tokens: injected.values().sum(),
        ..Score::default()
    };
    for subject in &include {
        if injected.contains_key(*subject) {
            score.tp += 1;
        } else {
            score.fn_ += 1;
            if is_high_value(subject) {
                score.high_value_fn += 1;
            }
        }
    }
    for subject in &exclude {
        if let Some(tokens) = injected.get(*subject) {
            score.fp += 1;
            score.wasted_tokens += tokens;
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
fn evaluate(strategy: InjectStrategy) -> (Vec<(String, Score)>, Score) {
    let corpus = load_corpus();
    let labels = load_labels();
    let root = temp_dir("store");
    let entries = materialize(&corpus, &root);

    let mut per_context = Vec::new();
    let mut total = Score::default();
    println!("\ninjection eval — strategy={strategy:?}");
    for ctx in &labels.context {
        let project = (!ctx.project_id.is_empty()).then_some(ctx.project_id.as_str());
        let injected = inject(strategy, &root, &entries, project);
        let score = score_context(&injected, ctx);
        println!(
            "  {:<20} precision={:.3} recall={:.3}  tp={} fp={} fn={} hi-fn={} tokens={} wasted={}",
            ctx.name,
            precision(score.tp, score.fp),
            recall(score.tp, score.fn_),
            score.tp,
            score.fp,
            score.fn_,
            score.high_value_fn,
            score.injected_tokens,
            score.wasted_tokens,
        );
        total.tp += score.tp;
        total.fp += score.fp;
        total.fn_ += score.fn_;
        total.high_value_fn += score.high_value_fn;
        total.injected_tokens += score.injected_tokens;
        total.wasted_tokens += score.wasted_tokens;
        per_context.push((ctx.name.clone(), score));
    }
    println!(
        "  {:<20} precision={:.3} recall={:.3}  tp={} fp={} fn={} hi-fn={} tokens={} wasted={}",
        "AGGREGATE",
        precision(total.tp, total.fp),
        recall(total.tp, total.fn_),
        total.tp,
        total.fp,
        total.fn_,
        total.high_value_fn,
        total.injected_tokens,
        total.wasted_tokens,
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
    let (per_context, total) = evaluate(InjectStrategy::Recency);

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

    // Exact confusion matrix: 19 correct inclusions, 19 over-inclusions
    // (3 incidents + 1 reference + 1 legacy global project/tool fact + 1
    // design sketch across 3 sessions, plus one project-scoped transient PR
    // status in its matching project).
    assert_eq!(total.tp, 19, "baseline true positives drifted");
    assert_eq!(total.fp, 19, "baseline false positives drifted");

    // Headline: high recall, mediocre precision (~0.613). This is the gap.
    assert!(
        precision(total.tp, total.fp) < 0.65,
        "baseline precision should be the low number we are trying to beat"
    );
    assert!((recall(total.tp, total.fn_) - 1.0).abs() < f64::EPSILON);
}

/// Relevance reaches full precision while preserving every safety guarantee.
///
/// Three paths combine: explicit `kind` withholds known incidents/references,
/// the operational heuristic catches dated incidents, and the strict legacy
/// global fallback withholds ambiguous project/tool facts. The dated
/// *preference* is still kept (never dropped), and project facts still inject
/// only in their own project.
#[test]
fn relevance_reaches_full_precision_with_kind() {
    let (per_context, total) = evaluate(InjectStrategy::Relevance);

    // Safety floor: nothing wanted dropped anywhere, no preference dropped.
    for (name, score) in &per_context {
        assert_eq!(score.fn_, 0, "{name}: classifier dropped a wanted memory");
    }
    assert_eq!(total.fn_, 0, "classifier must drop nothing wanted");
    assert_eq!(
        total.high_value_fn, 0,
        "classifier must never drop a preference"
    );

    // Keeps every true positive the baseline had, and drops every over-inclusion.
    assert_eq!(total.tp, 19, "classifier must not lose true positives");
    assert_eq!(
        total.fp, 0,
        "explicit kind plus the heuristic should withhold all search-only records"
    );

    let baseline = evaluate(InjectStrategy::Recency).1;
    assert!(
        precision(total.tp, total.fp) > precision(baseline.tp, baseline.fp),
        "relevance must beat baseline precision"
    );
    assert!(
        (precision(total.tp, total.fp) - 1.0).abs() < f64::EPSILON,
        "explicit kind closes the residual to full precision"
    );
}

/// Adaptive (the new default) is a strict, recall-safe precision win over
/// Recency: it withholds only records carrying an explicit non-startup `kind`
/// (the tagged incident and the two tagged references in the corpus) and never
/// drops a wanted memory or an untagged record. The untagged incidents and the
/// untagged legacy global fact are deliberately still injected — Adaptive does
/// not guess against unlabeled content; that is the property that makes it safe
/// to enable by default.
#[test]
fn adaptive_is_a_recall_safe_precision_win() {
    let (per_context, total) = evaluate(InjectStrategy::Adaptive);

    // Recall-safety floor: nothing wanted dropped, no preference dropped.
    for (name, score) in &per_context {
        assert_eq!(score.fn_, 0, "{name}: adaptive dropped a wanted memory");
    }
    assert_eq!(total.fn_, 0, "adaptive must drop nothing wanted");
    assert_eq!(
        total.high_value_fn, 0,
        "adaptive must never drop a preference"
    );
    assert_eq!(total.tp, 19, "adaptive must keep every true positive");

    let recency = evaluate(InjectStrategy::Recency).1;
    // Strictly fewer false positives and fewer wasted tokens than Recency,
    // because the tagged incident/reference records are withheld.
    assert!(
        total.fp < recency.fp,
        "adaptive must withhold the explicitly-tagged non-startup records"
    );
    assert!(total.wasted_tokens < recency.wasted_tokens);
    assert!(total.injected_tokens < recency.injected_tokens);
    assert!(precision(total.tp, total.fp) > precision(recency.tp, recency.fp));
    // But it is not as aggressive as Relevance: it keeps the untagged excludes,
    // so some false positives remain. This is the safety/precision trade.
    assert!(
        total.fp > 0,
        "adaptive must not guess against untagged content"
    );
}

/// The injected-token guardrail made explicit: both precision strategies spend
/// fewer tokens on false-positive context than Recency, with Relevance driving
/// wasted tokens to zero on the fully-tagged corpus. This is the standing
/// regression guard so a future recall change cannot quietly stuff context.
#[test]
fn precision_strategies_cut_wasted_tokens_versus_recency() {
    let recency = evaluate(InjectStrategy::Recency).1;
    let adaptive = evaluate(InjectStrategy::Adaptive).1;
    let relevance = evaluate(InjectStrategy::Relevance).1;

    assert!(
        recency.wasted_tokens > 0,
        "recency over-injects by construction"
    );
    assert!(adaptive.wasted_tokens < recency.wasted_tokens);
    assert!(relevance.wasted_tokens <= adaptive.wasted_tokens);
    assert_eq!(
        relevance.wasted_tokens, 0,
        "relevance reaches full precision, so it wastes no tokens"
    );
}
