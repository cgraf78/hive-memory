//! End-to-end background classifier eval.
//!
//! The plumbing test intentionally uses the public `hm` binary rather than
//! library calls. The behavior that matters is the real workflow: write a
//! record, classify it through a backend command, rebuild/read the index, and
//! observe relevance-mode context selection change.
//!
//! The ignored real-backend eval below measures classification quality
//! against a labeled set, and is the gate to run before bumping
//! `llm::VERDICT_VERSION` (which re-queues every prior LLM verdict).

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "hive-memory-classify-eval-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn init_store(root: &Path, name: &str) {
    cargo_bin_cmd!("hm")
        .args([
            "stores",
            "init",
            name,
            "--root",
            root.to_str().expect("utf8 path"),
        ])
        .assert()
        .success();
}

#[test]
fn classifier_verdict_flows_into_relevance_context() {
    let dir = temp_dir("relevance");
    let config = dir.join("config.toml");
    let store = dir.join("store");
    let fake_llm = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-llm");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            state_dir = "{}"
            cache_dir = "{}"

            [stores.personal]
            root = "{}"

            [defaults]
            context_strategy = "relevance"

            [classifier]
            mode = "on"
            backend = "command"
            command = ["{}"]
            timeout_seconds = 5
            "#,
            dir.join("state").display(),
            dir.join("cache").display(),
            store.display(),
            fake_llm.display()
        ),
    )
    .expect("write config");
    init_store(&store, "personal");

    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            "Repo alpha deploy window is Friday afternoons.",
            "--scope",
            "project",
            "--project-id",
            "repo-alpha",
            "--no-infer-kind",
        ])
        .assert()
        .success();

    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "context",
            "--project-id",
            "repo-alpha",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Repo alpha deploy window"));

    cargo_bin_cmd!("hm")
        .env("FAKE_LLM_KIND", "incident")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "classify",
        ])
        .assert()
        .success();

    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "context",
            "--project-id",
            "repo-alpha",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Repo alpha deploy window").not());
}

/// One labeled case for the real-backend quality eval.
struct LabeledCase {
    body: &'static str,
    scope: &'static str,
    project_id: Option<&'static str>,
    expected: hive_memory::note::MemoryKind,
}

/// Real-backend classification quality eval.
///
/// Ignored by default: it invokes whatever backend CLI auto-detection finds on
/// `$PATH` and costs real model calls. Run it manually before bumping
/// `llm::VERDICT_VERSION`, since a version bump re-queues every prior LLM
/// verdict for re-review:
///
/// ```sh
/// cargo test --test classify_eval -- --ignored --nocapture
/// ```
#[test]
#[ignore = "invokes a real LLM backend from PATH; run manually before bumping VERDICT_VERSION"]
fn real_backend_meets_accuracy_floor_on_labeled_set() {
    use hive_memory::llm;
    use hive_memory::note::MemoryKind;

    let Some(backend) = llm::detect(None, &[], None, None) else {
        // Skipping (not failing) keeps the eval runnable on any machine; the
        // point of the test is model quality, not backend availability.
        eprintln!("classify eval: no backend CLI on PATH; skipping");
        return;
    };
    eprintln!("classify eval: using backend {}", backend.label);

    let cases = [
        LabeledCase {
            body: "Always run `cargo clippy --all-targets` before presenting work as done.",
            scope: "global",
            project_id: None,
            expected: MemoryKind::Preference,
        },
        LabeledCase {
            body: "Prefers `rg` over `grep` and `fd` over `find` for searching.",
            scope: "global",
            project_id: None,
            expected: MemoryKind::Preference,
        },
        LabeledCase {
            body: "2026-03-04: nightly backup job failed with a full disk; rotated old \
                   archives to recover.",
            scope: "global",
            project_id: None,
            expected: MemoryKind::Incident,
        },
        LabeledCase {
            body: "Deploy of api-gateway v2.3 was rolled back on 2026-05-01 after elevated \
                   500 rates.",
            scope: "global",
            project_id: None,
            expected: MemoryKind::Incident,
        },
        LabeledCase {
            body: "repo-alpha uses `just test` as the canonical test entry point; CI calls \
                   the same target.",
            scope: "project",
            project_id: Some("repo-alpha"),
            expected: MemoryKind::ProjectFact,
        },
        LabeledCase {
            body: "The staging database for repo-alpha runs on the db-staging host and is \
                   reset weekly.",
            scope: "project",
            project_id: Some("repo-alpha"),
            expected: MemoryKind::ProjectFact,
        },
        LabeledCase {
            body: "On-call runbook lives at https://wiki.example.com/runbooks/oncall.",
            scope: "global",
            project_id: None,
            expected: MemoryKind::Reference,
        },
        LabeledCase {
            body: "For VPN setup details, read `docs/network/vpn.md` in the infra repo.",
            scope: "global",
            project_id: None,
            expected: MemoryKind::Reference,
        },
    ];

    let timeout = std::time::Duration::from_secs(60);
    let mut correct = 0usize;
    for (number, case) in cases.iter().enumerate() {
        let prompt = llm::classification_prompt(case.body, case.scope, case.project_id, None);
        // Rapid sequential CLI calls can hit transient rate limits; retry once
        // so the eval measures model quality, not API weather.
        let mut verdict = None;
        for attempt in 0..2 {
            match llm::invoke(&backend, &prompt, timeout) {
                Ok(value) => {
                    verdict = Some(value);
                    break;
                }
                Err(err) => {
                    eprintln!(
                        "classify eval: case {number} attempt {attempt} backend error: {err}"
                    );
                }
            }
        }
        let got = match verdict.map(|verdict| verdict.kind) {
            Some(llm::VerdictKind::Kind(kind)) => Some(kind),
            Some(llm::VerdictKind::Unclear) | None => None,
        };
        let hit = got == Some(case.expected);
        correct += usize::from(hit);
        eprintln!(
            "classify eval: case {number} expected={:?} got={got:?} {}",
            case.expected,
            if hit { "ok" } else { "MISS" }
        );
    }

    // 6/8 tolerates model wobble on genuinely judgment-call cases while still
    // catching prompt or adapter regressions before a verdict-version bump.
    let floor = 6;
    assert!(
        correct >= floor,
        "real-backend eval below accuracy floor: {correct}/{} correct (floor {floor})",
        cases.len()
    );
}
