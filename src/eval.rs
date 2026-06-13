//! Retrieval eval helpers.
//!
//! Eval corpuses are intentionally small, explicit TOML fixtures. They are not
//! canonical memory; they are labels that let retrieval changes prove whether
//! they helped, regressed, or merely changed ranking anecdotes.

use crate::config::Sensitivity;
use crate::index::{self, IndexEntry, RebuildIndexInput};
use crate::memory::{self, WriteRecordInput};
use crate::note::{Confidence, EntryKind, MemoryKind};
use crate::path::PathCase;
use crate::search::{self, SearchInput};
use crate::store::StoreManifest;
use crate::write::{AtomicWriteOptions, FsyncPolicy};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

/// Input for running a retrieval eval corpus.
#[derive(Debug, Clone)]
pub struct RetrievalEvalInput {
    /// TOML corpus path.
    pub corpus_path: PathBuf,
    /// Search limit used for each retrieval case.
    pub limit: usize,
}

/// Retrieval eval report with one or more scored candidates.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetrievalEvalReport {
    /// Corpus path that was evaluated.
    pub corpus: String,
    /// Per-candidate metrics.
    pub candidates: Vec<RetrievalCandidateMetrics>,
}

/// Metrics for one candidate retrieval configuration.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetrievalCandidateMetrics {
    /// Candidate name.
    pub name: String,
    /// Feature-bucket metrics.
    pub features: Vec<RetrievalMetrics>,
}

/// Aggregate retrieval metrics for one feature bucket.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetrievalMetrics {
    /// Feature bucket name from the corpus.
    pub feature: String,
    /// Number of cases in this bucket.
    pub cases: usize,
    /// Average recall at the configured limit.
    pub recall_at_k: f64,
    /// Average precision at the configured limit.
    pub precision_at_k: f64,
    /// Mean reciprocal rank.
    pub mrr: f64,
    /// Number of forbidden subjects that appeared in results.
    pub forbidden_hits: usize,
    /// p95 search latency in milliseconds.
    pub p95_ms: u128,
}

/// Retrieval eval failure.
#[derive(Debug)]
pub enum EvalError {
    /// Corpus could not be read.
    ReadCorpus { path: PathBuf, message: String },
    /// Corpus TOML could not be parsed.
    ParseCorpus { path: PathBuf, message: String },
    /// Fixture memory could not be written.
    WriteMemory(String),
    /// Index rebuild failed.
    Index(index::IndexError),
    /// Search failed.
    Search(search::SearchError),
    /// Timestamp generation failed.
    Time(String),
}

impl Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadCorpus { path, message } => {
                write!(
                    f,
                    "failed to read eval corpus {}: {message}",
                    path.display()
                )
            }
            Self::ParseCorpus { path, message } => {
                write!(
                    f,
                    "failed to parse eval corpus {}: {message}",
                    path.display()
                )
            }
            Self::WriteMemory(message) => write!(f, "failed to write eval memory: {message}"),
            Self::Index(err) => write!(f, "{err}"),
            Self::Search(err) => write!(f, "{err}"),
            Self::Time(message) => write!(f, "failed to create eval timestamp: {message}"),
        }
    }
}

impl Error for EvalError {}

impl From<index::IndexError> for EvalError {
    fn from(value: index::IndexError) -> Self {
        Self::Index(value)
    }
}

impl From<search::SearchError> for EvalError {
    fn from(value: search::SearchError) -> Self {
        Self::Search(value)
    }
}

/// Run the standard retrieval A/B scoreboard.
pub fn run_retrieval_eval(input: RetrievalEvalInput) -> Result<RetrievalEvalReport, EvalError> {
    let corpus = load_corpus(&input.corpus_path)?;
    let materialized = materialize(&corpus)?;
    let baseline = materialized.without_entities();
    Ok(RetrievalEvalReport {
        corpus: input.corpus_path.display().to_string(),
        candidates: vec![
            RetrievalCandidateMetrics {
                name: "no-entity-baseline".to_owned(),
                features: score_retrieval(&corpus, &baseline, input.limit)?,
            },
            RetrievalCandidateMetrics {
                name: "entity-linked".to_owned(),
                features: score_retrieval(&corpus, &materialized, input.limit)?,
            },
        ],
    })
}

#[derive(Debug, Deserialize)]
struct Corpus {
    #[serde(default)]
    record: Vec<Record>,
    #[serde(default)]
    retrieval_case: Vec<RetrievalCase>,
}

#[derive(Debug, Deserialize)]
struct Record {
    subject: String,
    entry_kind: EntryKind,
    scope: String,
    project_id: Option<String>,
    confidence: Confidence,
    kind: Option<MemoryKind>,
    body: String,
}

#[derive(Debug, Deserialize)]
struct RetrievalCase {
    #[allow(dead_code)]
    name: String,
    feature: String,
    query: String,
    project_id: Option<String>,
    expected: Vec<String>,
    forbidden: Vec<String>,
}

#[derive(Debug)]
struct Materialized {
    root: PathBuf,
    entries: Vec<IndexEntry>,
}

impl Materialized {
    fn without_entities(&self) -> Self {
        let mut entries = self.entries.clone();
        for entry in &mut entries {
            entry.entities.clear();
        }
        Self {
            root: self.root.clone(),
            entries,
        }
    }
}

fn load_corpus(path: &Path) -> Result<Corpus, EvalError> {
    let text = fs::read_to_string(path).map_err(|err| EvalError::ReadCorpus {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    toml::from_str(&text).map_err(|err| EvalError::ParseCorpus {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

fn materialize(corpus: &Corpus) -> Result<Materialized, EvalError> {
    let root = temp_dir("retrieval-eval").join("personal");
    fs::create_dir_all(&root).map_err(|err| EvalError::WriteMemory(err.to_string()))?;
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
        let offset = i64::try_from(index).map_err(|err| EvalError::Time(err.to_string()))?;
        let created_at = OffsetDateTime::from_unix_timestamp(1_778_946_153 + offset)
            .map_err(|err| EvalError::Time(err.to_string()))?;
        memory::write_record(WriteRecordInput {
            root: &root,
            manifest: &manifest,
            entry_kind: record.entry_kind,
            created_at,
            agent_id: "eval".to_owned(),
            host_id: "ci".to_owned(),
            user_id: "default".to_owned(),
            session_id: None,
            scope: record.scope.clone(),
            confidence: record.confidence,
            body: record.body.clone(),
            project_id: record.project_id.clone(),
            subject: Some(record.subject.clone()),
            kind: record.kind,
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
        .map_err(|err| EvalError::WriteMemory(err.to_string()))?;
    }

    let cache = temp_dir("retrieval-eval-cache");
    let report = index::rebuild_index(RebuildIndexInput {
        store_name: "personal",
        store_root: &root,
        cache_dir: &cache,
        options,
        path_case: PathCase::Sensitive,
    })?;

    Ok(Materialized {
        root,
        entries: report.entries,
    })
}

fn score_retrieval(
    corpus: &Corpus,
    materialized: &Materialized,
    limit: usize,
) -> Result<Vec<RetrievalMetrics>, EvalError> {
    let mut by_feature = BTreeMap::<String, Vec<CaseResult>>::new();
    for case in &corpus.retrieval_case {
        let start = Instant::now();
        let hits = search::search(SearchInput {
            store_root: &materialized.root,
            entries: &materialized.entries,
            query: &case.query,
            scopes: &["global".to_owned(), "project".to_owned()],
            sources: &["remembered".to_owned()],
            include_inbox: false,
            agent_id: Some("eval"),
            project_id: case.project_id.as_deref(),
            limit,
        })?;
        let elapsed = start.elapsed();
        let actual = hits
            .iter()
            .filter_map(|hit| hit.entry.subject.clone())
            .collect::<Vec<_>>();
        by_feature
            .entry(case.feature.clone())
            .or_default()
            .push(CaseResult::score(case, &actual, elapsed));
    }

    Ok(by_feature
        .into_iter()
        .map(|(feature, results)| RetrievalMetrics {
            feature,
            cases: results.len(),
            recall_at_k: average(results.iter().map(|result| result.recall_at_k)),
            precision_at_k: average(results.iter().map(|result| result.precision_at_k)),
            mrr: average(results.iter().map(|result| result.reciprocal_rank)),
            forbidden_hits: results.iter().map(|result| result.forbidden_hits).sum(),
            p95_ms: p95(results
                .iter()
                .map(|result| result.elapsed)
                .collect::<Vec<_>>())
            .as_millis(),
        })
        .collect())
}

#[derive(Debug)]
struct CaseResult {
    recall_at_k: f64,
    precision_at_k: f64,
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
            recall_at_k: if expected.is_empty() {
                1.0
            } else {
                matched as f64 / expected.len() as f64
            },
            precision_at_k: if actual.is_empty() {
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

fn average(values: impl Iterator<Item = f64>) -> f64 {
    let mut total = 0.0;
    let mut count = 0usize;
    for value in values {
        total += value;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        total / count as f64
    }
}

fn p95(mut values: Vec<Duration>) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    values.sort();
    let index = ((values.len() as f64) * 0.95).ceil() as usize;
    values[index.saturating_sub(1).min(values.len() - 1)]
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
