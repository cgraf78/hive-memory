//! Black-box CLI proof that `hm reconcile` (and `hm capture --promote`) refuse a
//! candidate that looks like a secret BEFORE any durable write.
//!
//! The secret guard in `run_reconcile`/promotion fires after backend *detection*
//! (a PATH/`command` probe, no model call) but before the model is *invoked* and
//! before anything is written. So a fake `command` backend is enough to get past
//! detection; the refusal must then short-circuit without spawning it or touching
//! the store. These tests assert the refusal exits non-zero, prints the documented
//! message, and leaves the store empty — and that a non-secret candidate still
//! promotes through the same fake backend, proving the guard is the thing
//! rejecting the secret, not a broken backend.

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
        "hive-memory-reconcile-secret-{name}-{}-{nanos}",
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

/// Write an executable fake model backend that echoes `output` on stdout
/// regardless of the prompt. For reconcile, an `{"op":"ADD"}` body drives a
/// durable write; for the secret cases the backend must never even be reached.
fn write_fake_backend(path: &Path, output: &str) {
    use std::os::unix::fs::PermissionsExt;
    let escaped = output.replace('\'', r"'\''");
    fs::write(
        path,
        format!("#!/usr/bin/env bash\ncat >/dev/null\nprintf '%s' '{escaped}'\n"),
    )
    .expect("write fake backend");
    let mut perms = fs::metadata(path).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod fake backend");
}

/// A backend that fails loudly if it is ever spawned, used to prove the secret
/// guard short-circuits before any model invocation.
fn write_exploding_backend(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(
        path,
        "#!/usr/bin/env bash\ncat >/dev/null\necho 'BACKEND SHOULD NOT RUN' >&2\nexit 1\n",
    )
    .expect("write exploding backend");
    let mut perms = fs::metadata(path).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod exploding backend");
}

/// Write a backend that answers both capture and reconcile prompts: the
/// extraction prompt (which asks for a "JSON array") gets `facts`; every reconcile
/// decision prompt gets `op`. Lets one `hm capture --promote` run drive extraction
/// and per-fact reconciliation through a single fake backend.
fn write_promote_backend(path: &Path, facts: &str, op: &str) {
    use std::os::unix::fs::PermissionsExt;
    let facts = facts.replace('\'', r"'\''");
    let op = op.replace('\'', r"'\''");
    fs::write(
        path,
        format!(
            "#!/usr/bin/env bash\nprompt=\"$(cat)\"\nif printf '%s' \"$prompt\" | grep -q 'JSON array'; then\n  printf '%s' '{facts}'\nelse\n  printf '%s' '{op}'\nfi\n"
        ),
    )
    .expect("write promote backend");
    let mut perms = fs::metadata(path).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod promote backend");
}

fn write_capture_config(config: &Path, dir: &Path, personal: &Path, backend: &Path) {
    fs::write(
        config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"
            state_dir = "{}"
            cache_dir = "{}"

            [classifier]
            backend = "command"
            command = ["{}"]

            [stores.personal]
            root = "{}"
            description = "Personal memory"
            "#,
            dir.join("data").display(),
            dir.join("state").display(),
            dir.join("cache").display(),
            backend.display(),
            personal.display(),
        ),
    )
    .expect("write capture config");
}

/// A clearly-secret AWS access key id. Assembled from parts so the literal does
/// not itself trip repository secret scanners, matching `src/secret.rs` tests.
fn aws_access_key() -> String {
    ["AKIA", "ABCDEFGHIJKLMNOP"].concat()
}

#[test]
fn reconcile_refuses_secret_candidate_and_writes_nothing() {
    let dir = temp_dir("reconcile-refuse");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let backend = dir.join("fake-backend");
    // If the guard regressed and let the candidate through, this backend would be
    // spawned and fail — so a passing test proves the refusal happens first.
    write_exploding_backend(&backend);
    write_capture_config(&config, &dir, &personal, &backend);
    init_store(&personal, "personal");

    let secret = format!("aws key for the deploy bot: {}", aws_access_key());
    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "reconcile",
        ])
        .write_stdin(secret)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing to reconcile a candidate that looks like a secret",
        ));

    // Nothing durable was written: a default (non-inbox) search finds no record,
    // and no outbox item was enqueued either.
    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "search",
            "deploy bot aws key",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hits: 0"));
    assert!(
        !dir.join("data/outbox").exists(),
        "a refused secret must not be staged to the outbox"
    );
}

#[test]
fn reconcile_refuses_password_assignment_candidate() {
    let dir = temp_dir("reconcile-refuse-password");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let backend = dir.join("fake-backend");
    write_exploding_backend(&backend);
    write_capture_config(&config, &dir, &personal, &backend);
    init_store(&personal, "personal");

    // A `key = value` secret with a realistic value (the `secret-assignment`
    // detector). Use `--text` here to exercise the non-stdin path too.
    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "reconcile",
            "--text",
            "db creds: password=supersecretvalue123",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing to reconcile a candidate that looks like a secret",
        ));

    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "search",
            "db creds password",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hits: 0"));
}

#[test]
fn reconcile_accepts_normal_candidate_through_same_backend() {
    let dir = temp_dir("reconcile-accept");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let backend = dir.join("fake-backend");
    // A real (ADD-returning) backend: the contrast case proving the secret guard,
    // not a missing/broken backend, is what rejects the secret candidates above.
    write_fake_backend(&backend, r#"{"op":"ADD"}"#);
    write_capture_config(&config, &dir, &personal, &backend);
    init_store(&personal, "personal");

    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "reconcile",
            "--text",
            "the user prefers ripgrep for searching",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("add: wrote"));

    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "search",
            "ripgrep searching",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "the user prefers ripgrep for searching",
        ));
}

#[test]
fn capture_promote_drops_secret_fact_but_promotes_normal_one() {
    let dir = temp_dir("promote-drop-secret");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let backend = dir.join("fake-backend");
    // The fake backend answers BOTH the extraction prompt (a JSON array) and the
    // per-fact reconcile decision (an op). Extraction returns one secret fact and
    // one normal fact. `capture --promote` must drop the secret-bearing fact (so
    // a credential never reaches durable memory) while still promoting the normal
    // one — unlike `hm reconcile`, capture promotion does NOT abort, it skips the
    // secret and succeeds.
    let key = aws_access_key();
    write_promote_backend(
        &backend,
        &format!(r#"["aws deploy key {key}", "the user prefers ripgrep for searching"]"#),
        r#"{"op":"ADD"}"#,
    );
    write_capture_config(&config, &dir, &personal, &backend);
    init_store(&personal, "personal");

    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "capture",
            "--promote",
            "--text",
            "user: here is the deploy key, and I prefer ripgrep.",
        ])
        .assert()
        .success()
        // Exactly one fact promotes; the secret is not counted as promoted.
        .stdout(predicate::str::contains("promoted 1 captured fact(s)"));

    // The normal fact is durable memory; the secret was never written.
    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "search",
            "ripgrep searching",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "the user prefers ripgrep for searching",
        ));
    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "search",
            "aws deploy key",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hits: 0"));
}
