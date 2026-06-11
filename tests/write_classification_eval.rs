//! Write-time memory kind classification eval.
//!
//! Session-start injection quality depends on new writes carrying useful
//! machine-readable kind metadata. This eval keeps the automatic classifier
//! honest: it scores exact kind labels and separately tracks high-value
//! preference misses, because dropping behavior guidance is the costly failure.

use hive_memory::note::MemoryKind;
use hive_memory::write_classify::{self, InferKindInput};
use serde::Deserialize;

const CORPUS: &str = include_str!("fixtures/write_classification_corpus.toml");

#[derive(Debug, Deserialize)]
struct Corpus {
    record: Vec<Record>,
}

#[derive(Debug, Deserialize)]
struct Record {
    id: String,
    scope: String,
    project_id: Option<String>,
    body: String,
    expected_kind: String,
    #[serde(default)]
    high_value: bool,
}

#[derive(Debug, Default)]
struct Score {
    total: usize,
    correct: usize,
    high_value_fn: usize,
}

fn expected_kind(value: &str) -> Option<MemoryKind> {
    match value {
        "" => None,
        "preference" => Some(MemoryKind::Preference),
        "project-fact" => Some(MemoryKind::ProjectFact),
        "incident" => Some(MemoryKind::Incident),
        "reference" => Some(MemoryKind::Reference),
        other => panic!("unknown expected kind in fixture: {other}"),
    }
}

fn classify(record: &Record) -> Option<MemoryKind> {
    write_classify::infer_kind(InferKindInput {
        scope: &record.scope,
        project_id: record.project_id.as_deref(),
        body: &record.body,
    })
    .map(|inference| inference.kind)
}

#[test]
fn write_kind_classifier_matches_fixture_labels() {
    let corpus: Corpus = toml::from_str(CORPUS).expect("parse corpus");
    let mut score = Score::default();
    let mut failures = Vec::new();

    for record in &corpus.record {
        let expected = expected_kind(&record.expected_kind);
        let actual = classify(record);
        score.total += 1;
        if actual == expected {
            score.correct += 1;
            continue;
        }
        if record.high_value && expected == Some(MemoryKind::Preference) {
            score.high_value_fn += 1;
        }
        failures.push(format!(
            "{} expected {:?} got {:?}",
            record.id, expected, actual
        ));
    }

    assert!(
        failures.is_empty(),
        "write classifier mismatches:\n{}",
        failures.join("\n")
    );
    assert_eq!(score.high_value_fn, 0, "must not miss key preferences");
    assert_eq!(score.correct, score.total);
}
