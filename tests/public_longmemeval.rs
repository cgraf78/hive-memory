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
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

const DATASET_ENV: &str = "HIVE_MEMORY_LONGMEMEVAL_S_JSON";
const MAX_CASES_ENV: &str = "HIVE_MEMORY_LONGMEMEVAL_MAX_CASES";
const INGEST_MODE_ENV: &str = "HIVE_MEMORY_LONGMEMEVAL_INGEST";
const DEFAULT_MAX_CASES: usize = 100;
const RETRIEVAL_LIMIT: usize = 5;
const ITEM_RETRIEVAL_LIMIT: usize = RETRIEVAL_LIMIT * 5;
const P95_BUDGET_MS: u128 = 500;

#[derive(Debug, Deserialize)]
struct LongMemEvalCase {
    question_id: String,
    question_type: String,
    question: String,
    answer_session_ids: Vec<String>,
    haystack_dates: Vec<String>,
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
    temp_root: PathBuf,
    root: PathBuf,
    cache_root: PathBuf,
    ingest_mode: IngestMode,
    entries_by_session_id: BTreeMap<String, Vec<IndexEntry>>,
    session_id_by_item_id: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct SessionFixture {
    date: String,
    turns: Vec<Turn>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum IngestMode {
    Session,
    Turn,
    Exchange,
}

impl IngestMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Turn => "turn",
            Self::Exchange => "exchange",
        }
    }
}

impl fmt::Display for IngestMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for IngestMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "session" => Ok(Self::Session),
            "turn" => Ok(Self::Turn),
            "exchange" => Ok(Self::Exchange),
            _ => Err(format!(
                "unsupported {INGEST_MODE_ENV}={value:?}; expected session, turn, or exchange"
            )),
        }
    }
}

#[derive(Debug, Clone)]
struct EvalMemoryItem {
    id: String,
    answer_session_id: String,
    parent_session_id: String,
    item_kind: EvalItemKind,
    role: Option<String>,
    turn_index: Option<usize>,
    body: String,
    search_keys: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum EvalItemKind {
    Session,
    Turn,
    Exchange,
}

impl EvalItemKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Turn => "turn",
            Self::Exchange => "exchange",
        }
    }
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
fn parses_longmemeval_fixture_dates_as_utc() {
    let timestamp = parse_longmemeval_date("2023/05/20 (Sat) 02:21");

    assert_eq!(timestamp.year(), 2023);
    assert_eq!(u8::from(timestamp.month()), 5);
    assert_eq!(timestamp.day(), 20);
    assert_eq!(timestamp.hour(), 2);
    assert_eq!(timestamp.minute(), 21);
    assert_eq!(timestamp.offset(), time::UtcOffset::UTC);
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
            .filter_map(|session_id| materialized.entries_by_session_id.get(session_id))
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        for session_id in &case.haystack_session_ids {
            assert!(
                materialized.entries_by_session_id.contains_key(session_id),
                "case {} missing haystack entries for session {}",
                case.question_id,
                session_id
            );
        }

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
            limit: ITEM_RETRIEVAL_LIMIT,
        })
        .unwrap_or_else(|err| panic!("case {} search failed: {err}", case.question_id));
        let elapsed = start.elapsed();
        let actual = unique_session_hits(&hits, &materialized.session_id_by_item_id);

        overall.add(&expected, &actual, elapsed);
        by_type
            .entry(case.question_type.clone())
            .or_default()
            .add(&expected, &actual, elapsed);
    }

    eprintln!(
        "LongMemEval-S mode={} overall cases={} recall@5={:.3} precision@5={:.3} mrr={:.3} non_answer_hits={} p95_ms={}",
        materialized.ingest_mode,
        overall.cases,
        overall.recall_at_5(),
        overall.precision_at_5(),
        overall.mrr(),
        overall.non_answer_hits,
        overall.p95_ms()
    );
    for (question_type, score) in by_type {
        eprintln!(
            "LongMemEval-S mode={} {question_type} cases={} recall@5={:.3} precision@5={:.3} mrr={:.3} non_answer_hits={} p95_ms={}",
            materialized.ingest_mode,
            score.cases,
            score.recall_at_5(),
            score.precision_at_5(),
            score.mrr(),
            score.non_answer_hits,
            score.p95_ms()
        );
    }

    let p95_ms = overall.p95_ms();
    let perf_budget_ms = perf_budget_ms();
    cleanup_materialized(&materialized);
    assert!(
        p95_ms <= perf_budget_ms,
        "LongMemEval-S p95 {}ms exceeded {}ms",
        p95_ms,
        perf_budget_ms
    );
}

fn load_cases(path: &Path) -> Vec<LongMemEvalCase> {
    let text = fs::read_to_string(path).expect("read LongMemEval-S JSON");
    serde_json::from_str(&text).expect("parse LongMemEval-S JSON")
}

fn materialize(cases: &[LongMemEvalCase]) -> Materialized {
    let ingest_mode = ingest_mode();
    let temp_root = temp_dir("public-longmemeval");
    let root = temp_root.join("personal");
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
    let mut sessions = BTreeMap::<String, SessionFixture>::new();
    for case in cases {
        assert_eq!(
            case.haystack_session_ids.len(),
            case.haystack_sessions.len(),
            "case {} haystack ids/sessions length mismatch",
            case.question_id
        );
        assert_eq!(
            case.haystack_session_ids.len(),
            case.haystack_dates.len(),
            "case {} haystack ids/dates length mismatch",
            case.question_id
        );
        for ((session_id, session), date) in case
            .haystack_session_ids
            .iter()
            .zip(case.haystack_sessions.iter())
            .zip(case.haystack_dates.iter())
        {
            sessions
                .entry(session_id.clone())
                .or_insert_with(|| SessionFixture {
                    date: date.clone(),
                    turns: session.clone(),
                });
        }
    }

    let items = sessions
        .iter()
        .flat_map(|(session_id, session)| eval_items(ingest_mode, session_id, &session.turns))
        .collect::<Vec<_>>();

    for (index, item) in items.iter().enumerate() {
        let session_date = sessions
            .get(&item.parent_session_id)
            .unwrap_or_else(|| panic!("missing fixture for {}", item.parent_session_id));
        let created_at =
            parse_longmemeval_date(&session_date.date) + time::Duration::seconds(index as i64);
        let mut tags = vec![
            "longmemeval-s".to_owned(),
            format!("kind:{}", item.item_kind.as_str()),
        ];
        if let Some(role) = &item.role {
            tags.push(format!("role:{role}"));
        }
        if let Some(turn_index) = item.turn_index {
            tags.push(format!("turn-index:{turn_index}"));
        }
        memory::write_record(WriteRecordInput {
            root: &root,
            manifest: &manifest,
            entry_kind: EntryKind::Remember,
            created_at,
            agent_id: "eval".to_owned(),
            host_id: "ci".to_owned(),
            user_id: "default".to_owned(),
            session_id: Some(item.parent_session_id.clone()),
            scope: "global".to_owned(),
            confidence: Confidence::High,
            body: render_item(item),
            project_id: None,
            subject: Some(item.id.clone()),
            kind: Some(MemoryKind::Reference),
            tags,
            audience: Vec::new(),
            source_kind: Some("public-eval".to_owned()),
            source_ref: Some(item.parent_session_id.clone()),
            write_event: false,
            options: options.clone(),
        })
        .expect("write LongMemEval eval item");
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
    let session_id_by_item_id = items
        .iter()
        .map(|item| (item.id.clone(), item.answer_session_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut entries_by_session_id = BTreeMap::<String, Vec<IndexEntry>>::new();
    for entry in report.entries {
        let Some(subject) = entry.subject.clone() else {
            continue;
        };
        let Some(session_id) = session_id_by_item_id.get(&subject) else {
            continue;
        };
        entries_by_session_id
            .entry(session_id.clone())
            .or_default()
            .push(entry);
    }
    assert_eq!(
        entries_by_session_id.len(),
        sessions.len(),
        "indexed session count mismatch"
    );
    assert_eq!(
        entries_by_session_id
            .values()
            .map(std::vec::Vec::len)
            .sum::<usize>(),
        items.len(),
        "indexed eval item count mismatch"
    );

    Materialized {
        temp_root,
        root,
        cache_root: cache,
        ingest_mode,
        entries_by_session_id,
        session_id_by_item_id,
    }
}

fn eval_items(mode: IngestMode, session_id: &str, session: &[Turn]) -> Vec<EvalMemoryItem> {
    match mode {
        IngestMode::Session => vec![EvalMemoryItem {
            id: session_id.to_owned(),
            answer_session_id: session_id.to_owned(),
            parent_session_id: session_id.to_owned(),
            item_kind: EvalItemKind::Session,
            role: None,
            turn_index: None,
            body: render_session(session),
            search_keys: Vec::new(),
        }],
        IngestMode::Turn => session
            .iter()
            .enumerate()
            .map(|(index, turn)| EvalMemoryItem {
                id: format!("{session_id}#turn-{index}"),
                answer_session_id: session_id.to_owned(),
                parent_session_id: session_id.to_owned(),
                item_kind: EvalItemKind::Turn,
                role: Some(turn.role.clone()),
                turn_index: Some(index),
                body: format!("{}: {}", turn.role, turn.content),
                search_keys: Vec::new(),
            })
            .collect(),
        IngestMode::Exchange => render_exchanges(session)
            .into_iter()
            .enumerate()
            .map(|(index, exchange)| EvalMemoryItem {
                id: format!("{session_id}#exchange-{index}"),
                answer_session_id: session_id.to_owned(),
                parent_session_id: session_id.to_owned(),
                item_kind: EvalItemKind::Exchange,
                role: None,
                turn_index: Some(exchange.first_turn_index),
                body: render_session(&exchange.turns),
                search_keys: Vec::new(),
            })
            .collect(),
    }
}

#[derive(Debug)]
struct Exchange {
    first_turn_index: usize,
    turns: Vec<Turn>,
}

fn render_exchanges(session: &[Turn]) -> Vec<Exchange> {
    let mut exchanges = Vec::<Exchange>::new();
    let mut current = Vec::<Turn>::new();
    let mut first_turn_index = 0usize;

    for (index, turn) in session.iter().enumerate() {
        if is_user_turn(turn) && !current.is_empty() {
            exchanges.push(Exchange {
                first_turn_index,
                turns: current,
            });
            current = Vec::new();
            first_turn_index = index;
        } else if current.is_empty() {
            first_turn_index = index;
        }
        current.push(turn.clone());
    }

    if !current.is_empty() {
        exchanges.push(Exchange {
            first_turn_index,
            turns: current,
        });
    }

    exchanges
}

fn is_user_turn(turn: &Turn) -> bool {
    turn.role.eq_ignore_ascii_case("user")
}

fn render_session(session: &[Turn]) -> String {
    session
        .iter()
        .map(|turn| format!("{}: {}", turn.role, turn.content))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_item(item: &EvalMemoryItem) -> String {
    let mut parts = Vec::new();
    if !item.search_keys.is_empty() {
        parts.push(format!("Search keys: {}", item.search_keys.join(", ")));
    }
    parts.push(item.body.clone());
    parts.join("\n")
}

fn parse_longmemeval_date(value: &str) -> OffsetDateTime {
    let mut parts = value.split_whitespace();
    let date = parts
        .next()
        .unwrap_or_else(|| panic!("LongMemEval date missing date: {value}"));
    let time = value
        .split_whitespace()
        .last()
        .unwrap_or_else(|| panic!("LongMemEval date missing time: {value}"));
    let rfc3339 = format!("{}T{time}:00Z", date.replace('/', "-"));
    OffsetDateTime::parse(&rfc3339, &time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|err| panic!("parse LongMemEval date {value:?}: {err}"))
}

fn unique_session_hits(
    hits: &[hive_memory::search::SearchHit],
    session_id_by_item_id: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut seen = BTreeSet::<String>::new();
    let mut session_ids = Vec::<String>::new();
    for hit in hits {
        let Some(item_id) = &hit.entry.subject else {
            continue;
        };
        let Some(session_id) = session_id_by_item_id.get(item_id) else {
            continue;
        };
        if seen.insert(session_id.clone()) {
            session_ids.push(session_id.clone());
        }
        if session_ids.len() == RETRIEVAL_LIMIT {
            break;
        }
    }
    session_ids
}

fn cleanup_materialized(materialized: &Materialized) {
    for path in [&materialized.temp_root, &materialized.cache_root] {
        if let Err(err) = fs::remove_dir_all(path) {
            eprintln!("failed to remove {}: {err}", path.display());
        }
    }
}

fn max_cases() -> usize {
    std::env::var(MAX_CASES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CASES)
}

fn ingest_mode() -> IngestMode {
    std::env::var(INGEST_MODE_ENV)
        .ok()
        .as_deref()
        .unwrap_or("session")
        .parse()
        .unwrap_or_else(|err| panic!("{err}"))
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
