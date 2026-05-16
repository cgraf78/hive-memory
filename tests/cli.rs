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
