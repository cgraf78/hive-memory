use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "hive-memory-cli-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn write_config(
    path: &std::path::Path,
    personal_root: &std::path::Path,
    work_root: &std::path::Path,
) {
    fs::write(
        path,
        format!(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "{}"
            description = "Personal memory"

            [stores.work]
            root = "{}"
            "#,
            personal_root.display(),
            work_root.display()
        ),
    )
    .expect("write config");
}

fn init_store(root: &std::path::Path, name: &str) {
    let mut cmd = cargo_bin_cmd!("hm");
    cmd.args([
        "stores",
        "init",
        name,
        "--root",
        root.to_str().expect("utf8 path"),
    ])
    .assert()
    .success();
}

fn stdout_value(stdout: &str, key: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .expect("stdout key")
        .trim()
        .to_owned()
}

#[test]
fn version_prints_binary_name() {
    let mut cmd = cargo_bin_cmd!("hm");

    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("hm "));
}

#[test]
fn help_describes_project() {
    let mut cmd = cargo_bin_cmd!("hm");

    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Vendor-neutral shared memory infrastructure for AI agents.",
        ));
}

#[test]
fn stores_init_creates_manifest() {
    let root = temp_dir("stores-init").join("personal");
    let mut cmd = cargo_bin_cmd!("hm");

    cmd.args([
        "stores",
        "init",
        "personal",
        "--root",
        root.to_str().expect("utf8 temp path"),
        "--description",
        "Personal memory",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("initialized store personal at "));

    let manifest = fs::read_to_string(root.join("manifest.toml")).expect("manifest written");
    assert!(manifest.contains("[store]"));
    assert!(manifest.contains("name = \"personal\""));
    assert!(root.join("inbox/notes").is_dir());
}

#[test]
fn stores_init_rejects_unknown_sensitivity() {
    let root = temp_dir("stores-init-bad-sensitivity").join("personal");
    let mut cmd = cargo_bin_cmd!("hm");

    cmd.args([
        "stores",
        "init",
        "personal",
        "--root",
        root.to_str().expect("utf8 temp path"),
        "--sensitivity",
        "classified",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "expected one of: public, internal, private, secret",
    ));
}

#[test]
fn stores_list_reads_configured_stores() {
    let dir = temp_dir("stores-list");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    let mut cmd = cargo_bin_cmd!("hm");

    cmd.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "stores",
        "list",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains(format!(
        "personal\t{}\tmissing",
        personal.display()
    )))
    .stdout(predicate::str::contains(format!(
        "work\t{}\tmissing",
        work.display()
    )));
}

#[test]
fn stores_show_defaults_to_config_default_store() {
    let dir = temp_dir("stores-show");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);

    let mut init = cargo_bin_cmd!("hm");
    init.args([
        "stores",
        "init",
        "personal",
        "--root",
        personal.to_str().expect("utf8 path"),
    ])
    .assert()
    .success();

    let mut show = cargo_bin_cmd!("hm");
    show.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "stores",
        "show",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("name: personal"))
    .stdout(predicate::str::contains("available: true"))
    .stdout(predicate::str::contains("manifest_id: "));
}

#[test]
fn stores_show_rejects_unknown_store() {
    let dir = temp_dir("stores-show-unknown");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    let mut show = cargo_bin_cmd!("hm");

    show.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "stores",
        "show",
        "missing",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("unknown store: missing"));
}

#[test]
fn stores_doctor_warns_for_missing_manifest() {
    let dir = temp_dir("stores-doctor");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    let mut doctor = cargo_bin_cmd!("hm");

    doctor
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "stores",
            "doctor",
            "personal",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("store: personal"))
        .stdout(predicate::str::contains("manifest: missing"))
        .stdout(predicate::str::contains("warning: missing manifest"));
}

#[test]
fn stores_migrate_reports_no_v1_migrations() {
    let dir = temp_dir("stores-migrate");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    let mut migrate = cargo_bin_cmd!("hm");

    migrate
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "stores",
            "migrate",
            "--dry-run",
            "--store",
            "personal",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("stores_checked: 1"))
        .stdout(predicate::str::contains("migrations_run: 0"))
        .stdout(predicate::str::contains("dry_run: true"))
        .stdout(predicate::str::contains(
            "status: no migrations for schema v1",
        ));
}

#[test]
fn remember_writes_note_and_event() {
    let dir = temp_dir("remember");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");
    let mut remember = cargo_bin_cmd!("hm");

    let output = remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "remember",
            "--text",
            "Chris prefers TOML config.",
            "--subject",
            "workflow.preference",
            "--tags",
            "preference,config",
        ])
        .output()
        .expect("run remember");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let note_path = PathBuf::from(stdout_value(&stdout, "note:"));
    let event_path = PathBuf::from(stdout_value(&stdout, "event:"));
    let note = fs::read_to_string(note_path).expect("read note");
    let event = fs::read_to_string(event_path).expect("read event");
    assert!(note.contains("entry_kind = \"remember\""));
    assert!(note.contains("related_event_id = "));
    assert!(note.contains("Chris prefers TOML config."));
    assert!(event.contains("\"type\": \"memory.observation\""));
    assert!(event.contains("\"note_path\": \"inbox/notes/"));
}

#[test]
fn note_respects_never_sidecar_default() {
    let dir = temp_dir("note-no-event");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "{}"

            [stores.work]
            root = "{}"

            [defaults]
            event_sidecar = "never"
            "#,
            personal.display(),
            work.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");
    let mut note = cargo_bin_cmd!("hm");

    let output = note
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "note",
            "--text",
            "Raw lower-confidence note.",
        ])
        .output()
        .expect("run note");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let note_path = PathBuf::from(stdout_value(&stdout, "note:"));
    let note = fs::read_to_string(note_path).expect("read note");
    assert!(note.contains("entry_kind = \"note\""));
    assert!(!stdout.contains("event:"));
}
