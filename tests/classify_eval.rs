//! End-to-end background classifier eval.
//!
//! This test intentionally uses the public `hm` binary rather than library
//! calls. The behavior that matters is the real workflow: write a record,
//! classify it through a backend command, rebuild/read the index, and observe
//! relevance-mode context selection change.

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
