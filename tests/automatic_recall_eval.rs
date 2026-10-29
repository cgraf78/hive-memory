//! Opt-in private replay of real prompt sequences against frozen memory data.
//!
//! No real prompts or corpus records live in this repository. The dataset and
//! output must share an external private directory. Every binary arm receives
//! its own copied store, HOME, state, and cache; hooks run only through the
//! common hermetic builder. This evaluates the CLI consumer, including startup
//! delivery dedupe, rather than duplicating the selector in test code.
//!
//! Set `HM_RECALL_EVAL_DATASET`, `HM_RECALL_EVAL_OUTPUT`, and
//! `HM_RECALL_EVAL_ARMS` (a JSON array of name/binary objects) to run this
//! ignored test. `HM_RECALL_EVAL_REPEATS` alternates arm order with fresh state.
//! Reports disclose development cases by default; set
//! `HM_RECALL_EVAL_DISCLOSE=all` only after the selection policy is frozen.

use hive_memory::hash::lower_hex;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

mod common;

#[derive(Deserialize)]
struct Dataset {
    schema: u32,
    snapshot_store: PathBuf,
    source_index: PathBuf,
    live_defaults: Value,
    sessions: Vec<Session>,
}

#[derive(Deserialize)]
struct Session {
    id: String,
    agent: String,
    project_hint: Option<String>,
    turns: Vec<Turn>,
}

#[derive(Deserialize)]
struct Turn {
    id: String,
    prompt: String,
    kind: String,
    partition: String,
    expected_abstain: bool,
    labels: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Arm {
    name: String,
    binary: PathBuf,
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create isolated directory");
    for entry in fs::read_dir(source).expect("read frozen store") {
        let entry = entry.expect("directory entry");
        assert!(!entry.file_type().expect("file type").is_symlink());
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy frozen file");
        }
    }
}

fn invoke(
    arm: &Arm,
    home: &Path,
    config: &Path,
    session: &Session,
    args: &[String],
) -> (Value, u128) {
    let mut command = common::hermetic_hm_at(&arm.binary);
    command
        .env("HOME", home)
        .env("HIVE_MEMORY_BACKGROUND_REFRESH", "0")
        .env("HIVE_MEMORY_PROJECT_INFER", "0")
        .env("HIVE_MEMORY_SESSION_ID", &session.id)
        .args([
            "--config",
            config.to_str().expect("config path"),
            "--as-agent",
            &session.agent,
        ])
        .args(args);
    let start = Instant::now();
    let output = command.output().expect("invoke frozen binary");
    let elapsed = start.elapsed().as_micros();
    // Do not expose private prompts or hook bodies in panic messages or CI logs.
    assert!(
        output.status.success(),
        "evaluation binary {} failed with {:?}",
        arm.name,
        output.status.code()
    );
    (
        serde_json::from_slice(&output.stdout).expect("hook JSON"),
        elapsed,
    )
}

fn emitted(response: &Value) -> BTreeSet<String> {
    response["delivery"]["emitted_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn context_bytes(response: &Value) -> usize {
    response["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|action| action["kind"] == "inject_context")
        .filter_map(|action| action["body"].as_str())
        .map(str::len)
        .sum()
}

fn percentile(values: &[u128], numerator: usize) -> u128 {
    if values.is_empty() {
        return 0;
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    values[(values.len() - 1) * numerator / 100]
}

/// Unknown candidate ids stay visible instead of inflating labeled precision.
fn label_counts(turn: &Turn, ids: &BTreeSet<String>) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::from([
        ("relevant", 0),
        ("irrelevant", 0),
        ("uncertain", 0),
        ("unlabeled", 0),
    ]);
    for id in ids {
        *counts
            .get_mut(turn.labels.get(id).map_or("unlabeled", String::as_str))
            .expect("label enum") += 1;
    }
    counts
}

/// Keep total prompt delivery and automatic recall separate: startup backfill
/// must not disappear from the usefulness assessment of what the agent sees.
fn summarize(cases: &[Value]) -> Value {
    let mut result = json!({"cases":cases.len()});
    for stage in ["labels", "recall_labels", "backfill_labels"] {
        for label in ["relevant", "irrelevant", "uncertain", "unlabeled"] {
            result[format!("{stage}_{label}")] = json!(
                cases
                    .iter()
                    .map(|case| case[stage][label].as_u64().unwrap_or(0))
                    .sum::<u64>()
            );
        }
    }
    let latencies: Vec<_> = cases
        .iter()
        .filter_map(|case| case["latency_us"].as_u64().map(u128::from))
        .collect();
    result["p50_latency_us"] = json!(percentile(&latencies, 50));
    result["p95_latency_us"] = json!(percentile(&latencies, 95));
    result["context_bytes"] = json!(
        cases
            .iter()
            .map(|case| case["context_bytes"].as_u64().unwrap_or(0))
            .sum::<u64>()
    );
    result["abstentions"] = json!(
        cases
            .iter()
            .filter(|case| case["abstained"] == true)
            .count()
    );
    result["recall_abstentions"] = json!(
        cases
            .iter()
            .filter(|case| case["recall_ids"].as_array().is_some_and(Vec::is_empty))
            .count()
    );
    result["useful_cases"] = json!(
        cases
            .iter()
            .filter(|case| case["known_relevant"].as_u64().unwrap_or(0) > 0)
            .count()
    );
    result["useful_context_cases"] = json!(
        cases
            .iter()
            .filter(|case| case["useful_available"] == true)
            .count()
    );
    result["needed_new_recall_cases"] = json!(
        cases
            .iter()
            .filter(|case| case["known_relevant"].as_u64().unwrap_or(0) > 0
                && case["useful_before"] == false)
            .count()
    );
    result["useful_new_recall_cases"] = json!(
        cases
            .iter()
            .filter(|case| case["known_relevant"].as_u64().unwrap_or(0) > 0
                && case["useful_before"] == false
                && case["recall_labels"]["relevant"].as_u64().unwrap_or(0) > 0)
            .count()
    );
    result["no_recall_cases"] = json!(
        cases
            .iter()
            .filter(|case| case["expected_abstain"] == true)
            .count()
    );
    result["no_recall_correct"] = json!(
        cases
            .iter()
            .filter(|case| case["expected_abstain"] == true
                && case["recall_ids"].as_array().is_some_and(Vec::is_empty))
            .count()
    );
    result["no_recall_delivery_correct"] = json!(
        cases
            .iter()
            .filter(|case| case["expected_abstain"] == true && case["abstained"] == true)
            .count()
    );
    result
}

/// Verify the canonical projection copied into an arm retains policy and ranking
/// facts. File locations and absent event sidecars are intentionally different;
/// bodies, validity, supersession, scope, and extracted entities must be exact.
fn validate_projection(cache: &Path, source_index: &Path) {
    let source = fs::read_to_string(source_index).expect("source projection");
    let source: Vec<Value> = source
        .lines()
        .map(|line| serde_json::from_str(line).expect("source entry"))
        .collect();
    let files: Vec<_> = fs::read_dir(cache.join("indexes"))
        .expect("rebuilt indexes")
        .map(|entry| entry.expect("index entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert_eq!(files.len(), 1, "one isolated index");
    let rebuilt = fs::read_to_string(&files[0]).expect("rebuilt projection");
    let rebuilt: Vec<Value> = rebuilt
        .lines()
        .map(|line| serde_json::from_str(line).expect("rebuilt entry"))
        .collect();
    assert!(
        source[0]["projection"]["entity_registry"] == rebuilt[0]["projection"]["entity_registry"],
        "entity registry retained"
    );
    assert!(
        source[0]["projection"]["project_aliases"] == rebuilt[0]["projection"]["project_aliases"],
        "project aliases retained"
    );
    let original: BTreeMap<_, _> = source[1..]
        .iter()
        .map(|entry| (entry["id"].as_str().expect("id"), entry))
        .collect();
    let actual: BTreeMap<_, _> = rebuilt[1..]
        .iter()
        .map(|entry| (entry["id"].as_str().expect("id"), entry))
        .collect();
    assert_eq!(original.len(), actual.len(), "complete entry count");
    for (id, entry) in original {
        let other = actual.get(id).expect("original id retained");
        for field in [
            "id",
            "store_id",
            "body",
            "scope",
            "project_id",
            "audience",
            "tags",
            "subject",
            "confidence",
            "valid_from",
            "valid_to",
            "supersedes",
            "kind",
            "entities",
            "created_at",
            "classified",
        ] {
            assert!(
                entry[field] == other[field],
                "snapshot metadata differs: {field}"
            );
        }
    }
}

#[test]
#[ignore = "requires a private frozen dataset and explicit evaluation binary arms"]
fn replay_private_automatic_recall() {
    let dataset_path =
        PathBuf::from(std::env::var("HM_RECALL_EVAL_DATASET").expect("private dataset path"));
    let root = dataset_path.parent().expect("private root");
    assert_eq!(
        fs::metadata(root)
            .expect("private directory")
            .permissions()
            .mode()
            & 0o077,
        0,
        "evaluation data requires an owner-only directory"
    );
    let output_path =
        PathBuf::from(std::env::var("HM_RECALL_EVAL_OUTPUT").expect("private output path"));
    assert!(
        output_path
            .parent()
            .expect("output parent")
            .canonicalize()
            .expect("output directory")
            == root.canonicalize().expect("dataset directory"),
        "keep private evaluation results beside the private dataset"
    );
    assert!(
        !output_path.exists() && !output_path.is_symlink(),
        "preserve earlier evaluation reports"
    );
    let dataset_bytes = fs::read(&dataset_path).expect("dataset");
    let freeze: Value =
        serde_json::from_slice(&fs::read(root.join("freeze.json")).expect("frozen label manifest"))
            .expect("freeze schema");
    assert!(
        freeze["dataset_sha256"] == lower_hex(&Sha256::digest(&dataset_bytes)),
        "labels must remain frozen before evaluating candidate outputs"
    );
    let dataset: Dataset = serde_json::from_slice(&dataset_bytes).expect("dataset schema");
    assert_eq!(dataset.schema, 1);
    assert!(
        dataset
            .snapshot_store
            .canonicalize()
            .expect("snapshot store")
            .starts_with(root.canonicalize().expect("private root")),
        "read only the explicit private corpus snapshot"
    );
    let manifest_bytes = fs::read(root.join("corpus_manifest.json")).expect("corpus manifest");
    assert!(
        freeze["corpus_manifest_sha256"] == lower_hex(&Sha256::digest(&manifest_bytes)),
        "corpus manifest must remain frozen"
    );
    let manifest: Value = serde_json::from_slice(&manifest_bytes).expect("corpus manifest schema");
    assert!(
        manifest["source_index_sha256"]
            == lower_hex(&Sha256::digest(
                fs::read(&dataset.source_index).expect("source index bytes")
            )),
        "source projection must remain frozen"
    );
    for file in manifest["files"].as_array().expect("snapshot files") {
        let path = dataset
            .snapshot_store
            .join(file["path"].as_str().expect("relative path"));
        assert!(
            path.canonicalize().expect("snapshot file").starts_with(
                dataset
                    .snapshot_store
                    .canonicalize()
                    .expect("snapshot root")
            ),
            "manifest paths stay inside the snapshot"
        );
        let bytes = fs::read(path).expect("snapshot file bytes");
        assert!(
            file["sha256"] == lower_hex(&Sha256::digest(&bytes)) && file["bytes"] == bytes.len(),
            "materialized corpus files must remain frozen"
        );
    }
    let arms: Vec<Arm> =
        serde_json::from_str(&std::env::var("HM_RECALL_EVAL_ARMS").expect("binary arms"))
            .expect("arm schema");
    assert!(!arms.is_empty());
    let names: BTreeSet<_> = arms.iter().map(|arm| &arm.name).collect();
    assert_eq!(names.len(), arms.len(), "arm names must not share state");
    assert!(
        names.iter().all(|name| !name.is_empty()
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')),
        "arm names are simple sandbox labels"
    );
    let repeats: usize = std::env::var("HM_RECALL_EVAL_REPEATS")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .expect("positive repeat count");
    assert!((1..=20).contains(&repeats), "keep the local replay bounded");
    let disclose =
        std::env::var("HM_RECALL_EVAL_DISCLOSE").unwrap_or_else(|_| "development".into());
    assert!(
        matches!(disclose.as_str(), "development" | "all"),
        "explicit disclosure mode"
    );
    let scratch = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(root)
        .expect("private replay sandbox");
    let mut results = Vec::new();
    for repeat in 0..repeats {
        // Reverse the arm order on alternate passes so one binary does not
        // always receive the warmer host. Every pass gets fresh delivery state.
        let order: Vec<_> = if repeat % 2 == 0 {
            arms.iter().collect()
        } else {
            arms.iter().rev().collect()
        };
        for arm in order {
            let base = scratch
                .path()
                .join(format!("repeat-{repeat}"))
                .join(&arm.name);
            let home = base.join("home");
            let store = base.join("store");
            let cache = base.join("cache");
            fs::create_dir_all(&home).expect("isolated HOME");
            copy_tree(&dataset.snapshot_store, &store);
            let config = base.join("config.toml");
            let defaults: toml::Value =
                serde_json::from_value(dataset.live_defaults.clone()).expect("defaults schema");
            let text = format!(
                "default_store = \"personal\"\nstate_dir = {:?}\ncache_dir = {:?}\ndata_dir = {:?}\n[stores.personal]\nroot = {:?}\n[defaults]\n{}\n[classifier]\nmode = \"off\"\n",
                base.join("state"),
                cache,
                base.join("data"),
                store,
                toml::to_string(&defaults).expect("serialize defaults")
            );
            fs::write(&config, text).expect("private config");
            let first = dataset.sessions.first().expect("sessions");
            invoke(
                arm,
                &home,
                &config,
                first,
                &["refresh".into(), "--force".into(), "--json".into()],
            );
            validate_projection(&cache, &dataset.source_index);
            let mut cases = Vec::new();
            let mut startups = Vec::new();
            for session in &dataset.sessions {
                let budget = if session.agent == "claude" {
                    9999
                } else {
                    9499
                };
                let mut common_args = vec![
                    "--json".into(),
                    "--delivery".into(),
                    "persistent".into(),
                    "--max-context-bytes".into(),
                    budget.to_string(),
                ];
                if let Some(project) = &session.project_hint {
                    common_args.extend(["--project".into(), project.clone()]);
                }
                let mut args = vec!["hook".into(), "session-start".into(), "--reset".into()];
                args.extend(common_args.clone());
                // AgentGuard already contributes 211 bytes of Codex Git guidance
                // plus its separator before asking hm for startup memory.
                let startup_budget = if session.agent == "codex" {
                    9287
                } else {
                    budget
                };
                let position = args
                    .iter()
                    .position(|arg| arg == "--max-context-bytes")
                    .expect("startup budget argument");
                args[position + 1] = startup_budget.to_string();
                let (startup, startup_latency) = invoke(arm, &home, &config, session, &args);
                assert!(
                    context_bytes(&startup) <= startup_budget,
                    "startup memory exceeds its remaining host budget"
                );
                let mut visible = emitted(&startup);
                startups.push(json!({"session":session.id,"emitted_ids":visible,"requested_budget":startup_budget,"context_bytes":context_bytes(&startup),"latency_us":startup_latency}));
                for turn in &session.turns {
                    let mut args = vec![
                        "hook".into(),
                        "prompt-submit".into(),
                        "--text".into(),
                        turn.prompt.clone(),
                    ];
                    args.extend(common_args.clone());
                    let (response, latency) = invoke(arm, &home, &config, session, &args);
                    let ids = emitted(&response);
                    let selected: BTreeSet<_> = response["recall"]["selected_ids"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    let recall_ids = ids.intersection(&selected).cloned().collect();
                    let backfill_ids = ids.difference(&selected).cloned().collect();
                    let counts = label_counts(turn, &ids);
                    let recall_counts = label_counts(turn, &recall_ids);
                    let backfill_counts = label_counts(turn, &backfill_ids);
                    let relevant: BTreeSet<_> = turn
                        .labels
                        .iter()
                        .filter(|(_, label)| label.as_str() == "relevant")
                        .map(|(id, _)| id.clone())
                        .collect();
                    let useful_before = relevant.iter().any(|id| visible.contains(id));
                    visible.extend(ids.iter().cloned());
                    let useful_available = relevant.iter().any(|id| visible.contains(id));
                    // Record only this disclosed turn's relevant visible ids.
                    // This proves exact useful-note retention even when an
                    // earlier heldout turn delivered a note before it was needed.
                    let visible_relevant_ids: BTreeSet<_> =
                        relevant.intersection(&visible).cloned().collect();
                    let bytes = context_bytes(&response);
                    assert!(
                        bytes <= budget,
                        "complete emitted memory context exceeds requested budget"
                    );
                    cases.push(json!({"id":turn.id,"session":session.id,"kind":turn.kind,"partition":turn.partition,"expected_abstain":turn.expected_abstain,"emitted_ids":ids,"recall_ids":recall_ids,"backfill_ids":backfill_ids,"labels":counts,"recall_labels":recall_counts,"backfill_labels":backfill_counts,"known_relevant":relevant.len(),"useful_before":useful_before,"useful_available":useful_available,"visible_relevant_ids":visible_relevant_ids,"abstained":ids.is_empty(),"context_bytes":bytes,"latency_us":latency,"recall_reason":response["recall"]["reason"]}));
                }
            }
            let replayed_cases = cases.len();
            // Replay hidden turns to preserve session context, but do not reveal
            // their results while an implementer is still tuning the policy.
            if disclose == "development" {
                cases.retain(|case| case["partition"] == "development");
            }
            let mut summary = summarize(&cases);
            summary["arm"] = json!(arm.name);
            summary["repeat"] = json!(repeat + 1);
            let by_session: BTreeMap<_, _> = dataset
                .sessions
                .iter()
                .map(|session| {
                    (
                        session.id.clone(),
                        summarize(
                            &cases
                                .iter()
                                .filter(|case| case["session"] == session.id)
                                .cloned()
                                .collect::<Vec<_>>(),
                        ),
                    )
                })
                .collect();
            let partitions = if disclose == "all" {
                vec!["development", "heldout"]
            } else {
                vec!["development"]
            };
            let by_partition: BTreeMap<_, _> = partitions
                .into_iter()
                .map(|partition| {
                    (
                        partition,
                        summarize(
                            &cases
                                .iter()
                                .filter(|case| case["partition"] == partition)
                                .cloned()
                                .collect::<Vec<_>>(),
                        ),
                    )
                })
                .collect();
            println!("{}", summary);
            results.push(json!({"repeat":repeat+1,"replayed_cases":replayed_cases,"summary":summary,"by_session":by_session,"by_partition":by_partition,"startups":startups,"cases":cases}));
        }
    }
    let pooled_latency: BTreeMap<_, _> = arms.iter().map(|arm| {
        let samples: Vec<_> = results.iter().filter(|run|run["summary"]["arm"]==arm.name).flat_map(|run|run["cases"].as_array().into_iter().flatten()).filter_map(|case|case["latency_us"].as_u64().map(u128::from)).collect();
        (arm.name.clone(),json!({"observations":samples.len(),"repeats":repeats,"p50_latency_us":percentile(&samples,50),"p95_latency_us":percentile(&samples,95)}))
    }).collect();
    let binaries: BTreeMap<_, _> = arms
        .iter()
        .map(|arm| {
            (
                arm.name.clone(),
                lower_hex(&Sha256::digest(
                    fs::read(&arm.binary).expect("binary fingerprint"),
                )),
            )
        })
        .collect();
    fs::write(
        output_path,
        serde_json::to_vec_pretty(&json!({"schema":1,"dataset_sha256":freeze["dataset_sha256"],"corpus_manifest_sha256":freeze["corpus_manifest_sha256"],"binary_sha256":binaries,"disclosure":disclose,"repeats":repeats,"pooled_latency":pooled_latency,"arms":results})).expect("report JSON"),
    )
    .expect("write private report");
}
