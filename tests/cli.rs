use assert_cmd::cargo::cargo_bin_cmd;
use hive_memory::{outbox, store};
use predicates::prelude::*;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
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

fn init_store_with_sensitivity(root: &std::path::Path, name: &str, sensitivity: &str) {
    let mut cmd = cargo_bin_cmd!("hm");
    cmd.args([
        "stores",
        "init",
        name,
        "--root",
        root.to_str().expect("utf8 path"),
        "--sensitivity",
        sensitivity,
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

fn write_outbox_note_item(
    data_dir: &std::path::Path,
    store_name: &str,
    item_id: &str,
    expected_store_id: Option<String>,
    final_note_path: &str,
    note_body: &[u8],
    state: outbox::OutboxState,
) {
    let item_dir = data_dir.join("outbox").join(store_name).join(item_id);
    fs::create_dir_all(&item_dir).expect("create outbox item");
    fs::write(item_dir.join("note.md"), note_body).expect("write outbox note");
    let meta = outbox::OutboxMeta {
        schema_version: outbox::OUTBOX_SCHEMA_VERSION,
        id: item_id.to_owned(),
        store: store_name.to_owned(),
        expected_store_id,
        final_note_path: final_note_path.to_owned(),
        note_sha256: outbox::payload_sha256(note_body),
        final_event_path: None,
        event_sha256: None,
        created_at: "2026-05-16T00:00:00Z".to_owned(),
        attempt_count: 0,
        last_error: None,
        state,
    };
    fs::write(
        item_dir.join("meta.toml"),
        outbox::render_meta(&meta).expect("render outbox meta"),
    )
    .expect("write outbox meta");
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
fn doctor_quick_reports_clean_adapter_install() {
    let dir = temp_dir("doctor-clean-adapter");
    let config = dir.join("config.toml");
    let data = dir.join("data");
    let personal = dir.join("personal");
    let output = dir.join("generated").join("codex.md");
    let target = dir.join("AGENTS.md");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"

            [stores.personal]
            root = "{}"

            [adapters.codex]
            enabled = true
            stores = ["personal"]
            scopes = ["global"]
            output = "{}"
            install_target = "{}"
            install_mode = "include"
            "#,
            data.display(),
            personal.display(),
            output.display(),
            target.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut render = cargo_bin_cmd!("hm");
    render
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "render",
            "--configured",
            "--install",
            "--quiet",
        ])
        .assert()
        .success();

    let mut doctor = cargo_bin_cmd!("hm");
    doctor
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "doctor",
            "--quick",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("doctor: ok (errors=0 warnings=0)"));
}

#[test]
fn doctor_warns_for_unknown_project_binding_store() {
    let dir = temp_dir("doctor-project-binding");
    let config = dir.join("config.toml");
    let data = dir.join("data");
    let personal = dir.join("personal");
    fs::create_dir_all(data.join("projects")).expect("project binding dir");
    fs::write(
        data.join("projects/bound-project.toml"),
        "project_id = \"bound-project\"\nstore = \"missing\"\n",
    )
    .expect("binding");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            data.display(),
            personal.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut doctor = cargo_bin_cmd!("hm");
    doctor
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "doctor",
            "--quick",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"warnings\": 1"))
        .stdout(predicate::str::contains(
            "project bound-project is bound to unknown store missing",
        ));
}

#[test]
fn doctor_full_warns_for_likely_secret_in_private_note_without_echoing_value() {
    let dir = temp_dir("doctor-note-secret");
    let config = dir.join("config.toml");
    let data = dir.join("data");
    let personal = dir.join("personal");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            data.display(),
            personal.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut remember = cargo_bin_cmd!("hm");
    let output = remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            "normal durable memory",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8 stdout");
    let note_path = PathBuf::from(stdout_value(&stdout, "note:"));
    let secret_value = "localvalueforsecretdetection";
    let note = fs::read_to_string(&note_path).expect("read note");
    fs::write(
        &note_path,
        format!("{note}\napi_key = \"{secret_value}\"\n"),
    )
    .expect("append fixture secret");

    let mut doctor = cargo_bin_cmd!("hm");
    doctor
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "doctor",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("detectors: secret-assignment"))
        .stdout(predicate::str::contains(secret_value).not());
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
fn remember_json_reports_stable_write_fields() {
    let dir = temp_dir("remember-json");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "remember",
            "--text",
            "Chris prefers JSON write output.",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"id\": \""))
        .stdout(predicate::str::contains("\"store\": \"personal\""))
        .stdout(predicate::str::contains("\"store_id\": \""))
        .stdout(predicate::str::contains(
            "\"store_source\": \"agent-default\"",
        ))
        .stdout(predicate::str::contains("\"scope\": \"global\""))
        .stdout(predicate::str::contains("\"project_id\": null"))
        .stdout(predicate::str::contains("\"audience\": []"))
        .stdout(predicate::str::contains("\"note_path\": \""))
        .stdout(predicate::str::contains("\"event_path\": \""))
        .stdout(predicate::str::contains("\"created\": true"))
        .stdout(predicate::str::contains("\"duplicate_of\": null"));
}

#[test]
fn remember_refuses_likely_secret_without_echoing_value() {
    let dir = temp_dir("remember-secret-refusal");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");
    let secret_value = "localvalueforsecretdetection";

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            &format!("api_key = \"{secret_value}\""),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("detectors: secret-assignment"))
        .stderr(predicate::str::contains(secret_value).not());
}

#[test]
fn allow_secret_write_requires_secret_store() {
    let dir = temp_dir("remember-secret-private-store");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"

            [privacy]
            allow_secret_writes = true

            [stores.personal]
            root = "{}"

            [stores.work]
            root = "{}"
            "#,
            personal.display(),
            work.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--allow-secret-write",
            "--text",
            "api_key = \"localvalueforsecretdetection\"",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--allow-secret-write requires a resolved secret store",
        ));
}

#[test]
fn allow_secret_write_requires_config_opt_in() {
    let dir = temp_dir("remember-secret-config-opt-in");
    let config = dir.join("config.toml");
    let secret = dir.join("secret-store");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "secret"

            [stores.secret]
            root = "{}"
            sensitivity = "secret"
            "#,
            secret.display()
        ),
    )
    .expect("write config");
    init_store_with_sensitivity(&secret, "secret", "secret");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--allow-secret-write",
            "--text",
            "api_key = \"localvalueforsecretdetection\"",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--allow-secret-write requires privacy.allow_secret_writes = true",
        ));
}

#[test]
fn allow_secret_write_succeeds_in_opted_in_secret_store() {
    let dir = temp_dir("remember-secret-store");
    let config = dir.join("config.toml");
    let secret = dir.join("secret-store");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "secret"

            [privacy]
            allow_secret_writes = true

            [stores.secret]
            root = "{}"
            sensitivity = "secret"
            "#,
            secret.display()
        ),
    )
    .expect("write config");
    init_store_with_sensitivity(&secret, "secret", "secret");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--allow-secret-write",
            "--text",
            "api_key = \"localvalueforsecretdetection\"",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("store: secret"));
}

#[test]
fn hook_mode_secret_write_requires_extra_opt_in() {
    let dir = temp_dir("remember-secret-hook");
    let config = dir.join("config.toml");
    let secret = dir.join("secret-store");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "secret"

            [privacy]
            allow_secret_writes = true

            [stores.secret]
            root = "{}"
            sensitivity = "secret"
            "#,
            secret.display()
        ),
    )
    .expect("write config");
    init_store_with_sensitivity(&secret, "secret", "secret");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .env("HIVE_MEMORY_HOOK_ACTIVE", "1")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--allow-secret-write",
            "--text",
            "api_key = \"localvalueforsecretdetection\"",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "hook secret writes require privacy.allow_hook_secret_writes = true",
        ));
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

#[test]
fn flush_writes_pending_outbox_item_and_archives_payload() {
    let dir = temp_dir("flush-pending");
    let config = dir.join("config.toml");
    let data = dir.join("data");
    let personal = dir.join("personal");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            data.display(),
            personal.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");
    let manifest = store::read_manifest(&personal).expect("read manifest");
    let final_note_path = "inbox/notes/2026/05/16/offline-note.md";
    let note_body = b"offline note body\n";
    write_outbox_note_item(
        &data,
        "personal",
        "offline-note",
        Some(manifest.store.id),
        final_note_path,
        note_body,
        outbox::OutboxState::Pending,
    );

    let mut flush = cargo_bin_cmd!("hm");
    flush
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "flush",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"flushed\": 1"))
        .stdout(predicate::str::contains("\"failed\": 0"));

    assert_eq!(
        fs::read(personal.join(final_note_path)).expect("read flushed note"),
        note_body
    );
    assert!(!data.join("outbox/personal/offline-note").exists());
    let archive_root = personal.join(".outbox-archive");
    let archive_note = fs::read_dir(&archive_root)
        .expect("archive host dirs")
        .flat_map(|host| fs::read_dir(host.expect("host dir").path()).expect("archive dates"))
        .map(|date| {
            date.expect("date dir")
                .path()
                .join("offline-note")
                .join("note.md")
        })
        .find(|path| path.is_file())
        .expect("archive note");
    assert_eq!(
        fs::read(archive_note).expect("read archive note"),
        note_body
    );
}

#[test]
fn flush_skips_unbound_outbox_items() {
    let dir = temp_dir("flush-unbound");
    let config = dir.join("config.toml");
    let data = dir.join("data");
    let personal = dir.join("personal");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            data.display(),
            personal.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");
    write_outbox_note_item(
        &data,
        "personal",
        "unbound-note",
        None,
        "inbox/notes/2026/05/16/unbound-note.md",
        b"unbound note body\n",
        outbox::OutboxState::Unbound,
    );

    let mut flush = cargo_bin_cmd!("hm");
    flush
        .args(["--config", config.to_str().expect("utf8 config"), "flush"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unbound=1"));

    assert!(data.join("outbox/personal/unbound-note").is_dir());
    let final_note = personal.join("inbox/notes/2026/05/16/unbound-note.md");
    assert!(!final_note.exists());
}

#[test]
fn outbox_flush_marks_same_hash_collision_as_skipped() {
    let dir = temp_dir("flush-same-hash");
    let config = dir.join("config.toml");
    let data = dir.join("data");
    let personal = dir.join("personal");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            data.display(),
            personal.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");
    let manifest = store::read_manifest(&personal).expect("read manifest");
    let final_note_path = "inbox/notes/2026/05/16/same-note.md";
    let note_body = b"same note body\n";
    let final_path = personal.join(final_note_path);
    fs::create_dir_all(final_path.parent().expect("final parent")).expect("final dirs");
    fs::write(&final_path, note_body).expect("write existing final");
    write_outbox_note_item(
        &data,
        "personal",
        "same-note",
        Some(manifest.store.id),
        final_note_path,
        note_body,
        outbox::OutboxState::Pending,
    );

    let mut flush = cargo_bin_cmd!("hm");
    flush
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "outbox",
            "flush",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"flushed\": 0"))
        .stdout(predicate::str::contains("\"skipped\": 1"))
        .stdout(predicate::str::contains("\"failed\": 0"));

    assert!(!data.join("outbox/personal/same-note").exists());
    assert_eq!(fs::read(final_path).expect("read final note"), note_body);
}

#[test]
fn flush_refuses_different_hash_collision() {
    let dir = temp_dir("flush-conflict");
    let config = dir.join("config.toml");
    let data = dir.join("data");
    let personal = dir.join("personal");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            data.display(),
            personal.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");
    let manifest = store::read_manifest(&personal).expect("read manifest");
    let final_note_path = "inbox/notes/2026/05/16/conflict-note.md";
    let final_path = personal.join(final_note_path);
    fs::create_dir_all(final_path.parent().expect("final parent")).expect("final dirs");
    fs::write(&final_path, "different body\n").expect("write conflicting final");
    write_outbox_note_item(
        &data,
        "personal",
        "conflict-note",
        Some(manifest.store.id),
        final_note_path,
        b"outbox body\n",
        outbox::OutboxState::Pending,
    );

    let mut flush = cargo_bin_cmd!("hm");
    flush
        .args(["--config", config.to_str().expect("utf8 config"), "flush"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("failed=1"))
        .stderr(predicate::str::contains("flush failed for 1 item"));
}

#[test]
fn inbox_lists_shows_and_promotes_raw_notes_idempotently() {
    let dir = temp_dir("inbox-promote");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");

    let mut note = cargo_bin_cmd!("hm");
    let output = note
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "note",
            "--text",
            "Raw curation note for later promotion.",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8 stdout");
    let note_id = stdout_value(&stdout, "id:");

    let mut list = cargo_bin_cmd!("hm");
    list.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "inbox",
        "list",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("items: 1"))
    .stdout(predicate::str::contains(&note_id))
    .stdout(predicate::str::contains("pending"));

    let mut show = cargo_bin_cmd!("hm");
    show.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "inbox",
        "show",
        &note_id,
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains(
        "Raw curation note for later promotion.",
    ));

    let mut promote = cargo_bin_cmd!("hm");
    let output = promote
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "promote",
            &note_id,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("promoted: true"))
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8 stdout");
    let target = PathBuf::from(stdout_value(&stdout, "target:"));
    let event = PathBuf::from(stdout_value(&stdout, "event:"));
    let curated = fs::read_to_string(&target).expect("read curated target");
    let promotion_event = fs::read_to_string(&event).expect("read promotion event");
    assert!(curated.contains("- Raw curation note for later promotion."));
    assert!(curated.contains(&format!("hive-memory:promoted source=\"{note_id}\"")));
    assert!(promotion_event.contains("\"type\": \"memory.promotion\""));
    assert!(promotion_event.contains(&format!("\"ref\": \"{note_id}\"")));

    fs::remove_file(&event).expect("remove promotion event to simulate interrupted run");
    let mut heal_retry = cargo_bin_cmd!("hm");
    heal_retry
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "promote",
            &note_id,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("promoted: false"))
        .stdout(predicate::str::contains("event:"));

    let mut default_list = cargo_bin_cmd!("hm");
    default_list
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "inbox",
            "list",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("items: 0"));

    let mut all_list = cargo_bin_cmd!("hm");
    all_list
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "inbox",
            "list",
            "--all",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("items: 1"))
        .stdout(predicate::str::contains("promoted"));

    let mut context = cargo_bin_cmd!("hm");
    context
        .args(["--config", config.to_str().expect("utf8 config"), "context"])
        .assert()
        .success()
        .stdout(predicate::str::contains("trust=\"curated\""))
        .stdout(predicate::str::contains(
            "Raw curation note for later promotion.",
        ))
        .stdout(predicate::str::contains("trust=\"raw\"").not());

    let mut retry = cargo_bin_cmd!("hm");
    retry
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "promote",
            &note_id,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("promoted: false"))
        .stdout(predicate::str::contains("event:").not());
    let curated_after_retry = fs::read_to_string(target).expect("read curated target again");
    assert_eq!(
        curated_after_retry.matches("hive-memory:promoted").count(),
        1
    );
}

#[test]
fn promote_rejects_targets_outside_curated_areas() {
    let dir = temp_dir("promote-invalid-target");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");

    let mut note = cargo_bin_cmd!("hm");
    let output = note
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "note",
            "--text",
            "Raw note for invalid target.",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8 stdout");
    let note_id = stdout_value(&stdout, "id:");

    let mut promote = cargo_bin_cmd!("hm");
    promote
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "promote",
            &note_id,
            "--to",
            "../outside.md",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid promotion target"));
}

#[test]
fn search_finds_remembered_note() {
    let dir = temp_dir("search");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            "Search should find TOML preferences.",
        ])
        .assert()
        .success();

    let mut search = cargo_bin_cmd!("hm");
    search
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "search",
            "toml",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hits: 1"))
        .stdout(predicate::str::contains(
            "snippet: Search should find TOML preferences.",
        ));
}

#[test]
fn search_finds_curated_memory_from_default_sources() {
    let dir = temp_dir("search-curated-default");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");
    fs::create_dir_all(personal.join("rules")).expect("rules dir");
    fs::write(
        personal.join("rules/preferences.md"),
        "Search should find curated TOML preferences.\n",
    )
    .expect("curated memory");

    let mut search = cargo_bin_cmd!("hm");
    search
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "search",
            "toml",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hits: 1"))
        .stdout(predicate::str::contains("id: curated:rules/preferences.md"))
        .stdout(predicate::str::contains(
            "snippet: Search should find curated TOML preferences.",
        ));
}

#[test]
fn search_json_reports_stable_hit_fields() {
    let dir = temp_dir("search-json");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");
    fs::create_dir_all(personal.join("rules")).expect("rules dir");
    fs::write(
        personal.join("rules/preferences.md"),
        "JSON search should find curated TOML preferences.\n",
    )
    .expect("curated memory");

    let mut search = cargo_bin_cmd!("hm");
    search
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "search",
            "toml",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "\"id\": \"curated:rules/preferences.md\"",
        ))
        .stdout(predicate::str::contains("\"store\": \"personal\""))
        .stdout(predicate::str::contains("\"store_id\": \""))
        .stdout(predicate::str::contains("\"scope\": \"global\""))
        .stdout(predicate::str::contains("\"trust\": \"curated\""))
        .stdout(predicate::str::contains("\"audience\": []"))
        .stdout(predicate::str::contains(
            "\"path\": \"rules/preferences.md\"",
        ))
        .stdout(predicate::str::contains(
            "\"title\": \"rules/preferences.md\"",
        ))
        .stdout(predicate::str::contains(
            "\"snippet\": \"JSON search should find curated TOML preferences.\"",
        ))
        .stdout(predicate::str::contains("\"score\": 1"))
        .stdout(predicate::str::contains("\"created_at\": \"\""));
}

#[test]
fn search_requires_include_inbox_for_raw_note() {
    let dir = temp_dir("search-include-inbox");
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
    note.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "note",
        "--text",
        "Raw note mentions TOML.",
    ])
    .assert()
    .success();

    let mut default_search = cargo_bin_cmd!("hm");
    default_search
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "search",
            "toml",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hits: 0"));

    let mut inbox_search = cargo_bin_cmd!("hm");
    inbox_search
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "search",
            "toml",
            "--include-inbox",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hits: 1"))
        .stdout(predicate::str::contains("snippet: Raw note mentions TOML."));
}

#[test]
fn search_uses_configured_default_scopes() {
    let dir = temp_dir("search-default-scopes");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--scope",
            "scratch",
            "--text",
            "Scratch TOML memory.",
        ])
        .assert()
        .success();

    let mut default_search = cargo_bin_cmd!("hm");
    default_search
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "search",
            "toml",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hits: 0"));

    let mut scoped_search = cargo_bin_cmd!("hm");
    scoped_search
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "search",
            "toml",
            "--scope",
            "scratch",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hits: 1"));
}

#[test]
fn search_enforces_agent_read_store_policy() {
    let dir = temp_dir("search-agent-read-policy");
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

            [agents.codex]
            default_store = "personal"
            read_stores = ["personal"]
            write_stores = ["personal"]
            "#,
            personal.display(),
            work.display()
        ),
    )
    .expect("write config");
    init_store(&work, "work");

    let mut search = cargo_bin_cmd!("hm");
    search
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "--store",
            "work",
            "search",
            "toml",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "agent codex may not read store work",
        ));
}

#[test]
fn search_allows_agent_with_all_store_affinity() {
    let dir = temp_dir("search-agent-all-stores");
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

            [agents.codex]
            default_store = "personal"
            read_stores = ["personal"]
            write_stores = ["personal"]
            allow_all_stores = true
            "#,
            personal.display(),
            work.display()
        ),
    )
    .expect("write config");
    init_store(&work, "work");

    let mut search = cargo_bin_cmd!("hm");
    search
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "--store",
            "work",
            "search",
            "toml",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("store: work"));
}

#[test]
fn context_renders_remembered_memory() {
    let dir = temp_dir("context");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "remember",
            "--text",
            "Context should include TOML preferences.",
        ])
        .assert()
        .success();

    let mut context = cargo_bin_cmd!("hm");
    context
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "context",
            "--path",
            "/repo/src/main.rs",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Hive Memory Context"))
        .stdout(predicate::str::contains("store: personal"))
        .stdout(predicate::str::contains("agent: codex"))
        .stdout(predicate::str::contains("<memory id=\""))
        .stdout(predicate::str::contains(
            "Context should include TOML preferences.",
        ));
}

#[test]
fn context_requires_include_inbox_for_raw_note() {
    let dir = temp_dir("context-include-inbox");
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
    note.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "note",
        "--text",
        "Raw context note.",
    ])
    .assert()
    .success();

    let mut default_context = cargo_bin_cmd!("hm");
    default_context
        .args(["--config", config.to_str().expect("utf8 config"), "context"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Raw context note.").not());

    let mut inbox_context = cargo_bin_cmd!("hm");
    inbox_context
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "context",
            "--include-inbox",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("trust=\"raw\""))
        .stdout(predicate::str::contains("Raw context note."));
}

#[test]
fn remember_project_hint_feeds_project_context() {
    let dir = temp_dir("remember-project-hint");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    let repo = dir.join("repo");
    let file = repo.join("src/lib.rs");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");
    fs::create_dir_all(file.parent().expect("file parent")).expect("repo src");
    fs::write(&file, "// source\n").expect("source file");
    let init = Command::new("git")
        .args(["-C", repo.to_str().expect("utf8 repo"), "init"])
        .output()
        .expect("git init");
    assert!(init.status.success());
    let remote = Command::new("git")
        .args([
            "-C",
            repo.to_str().expect("utf8 repo"),
            "remote",
            "add",
            "origin",
            "https://github.com/cgraf78/hive-memory.git",
        ])
        .output()
        .expect("git remote");
    assert!(remote.status.success());

    let mut remember = cargo_bin_cmd!("hm");
    let output = remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--scope",
            "project",
            "--project",
            file.to_str().expect("utf8 file"),
            "--text",
            "Project hints derive memory identity.",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8 stdout");
    let note_path = stdout_value(&stdout, "note:");
    let note = fs::read_to_string(note_path).expect("read note");
    assert!(note.contains("project_id = \"github-com-cgraf78-hive-memory-"));

    let mut context = cargo_bin_cmd!("hm");
    context
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "context",
            "--project",
            file.to_str().expect("utf8 file"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Project hints derive memory identity.",
        ));
}

#[test]
fn render_writes_adapter_output() {
    let dir = temp_dir("render");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let output = dir.join("generated").join("codex.md");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "{}"

            [adapters.codex]
            enabled = true
            stores = ["personal"]
            scopes = ["global"]
            output = "{}"
            "#,
            personal.display(),
            output.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            "Rendered context includes TOML memory.",
        ])
        .assert()
        .success();

    let mut render = cargo_bin_cmd!("hm");
    render
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "render",
            "codex",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("adapter: codex"))
        .stdout(predicate::str::contains("written: true"));

    let rendered = fs::read_to_string(output).expect("read render output");
    assert!(rendered.starts_with("<!-- hive-memory:generated v=1 sha256="));
    assert!(rendered.contains("Rendered context includes TOML memory."));
}

#[test]
fn render_refuses_drifted_output_without_force_backup() {
    let dir = temp_dir("render-drift");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let output = dir.join("generated").join("codex.md");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "{}"

            [adapters.codex]
            enabled = true
            stores = ["personal"]
            scopes = ["global"]
            output = "{}"
            "#,
            personal.display(),
            output.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            "Rendered drift memory.",
        ])
        .assert()
        .success();

    let mut first_render = cargo_bin_cmd!("hm");
    first_render
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "render",
            "codex",
        ])
        .assert()
        .success();
    fs::write(
        &output,
        fs::read_to_string(&output).expect("read render") + "manual edit\n",
    )
    .expect("drift render output");

    let mut second_render = cargo_bin_cmd!("hm");
    second_render
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "render",
            "codex",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing to overwrite edited render file",
        ));
}

#[test]
fn render_install_adds_instruction_markers() {
    let dir = temp_dir("render-install");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let output = dir.join("generated").join("codex.md");
    let install_target = dir.join("AGENTS.md");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "{}"

            [adapters.codex]
            enabled = true
            stores = ["personal"]
            scopes = ["global"]
            output = "{}"
            install_target = "{}"
            install_mode = "include"
            "#,
            personal.display(),
            output.display(),
            install_target.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            "Installed render memory.",
        ])
        .assert()
        .success();

    let mut render = cargo_bin_cmd!("hm");
    render
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "render",
            "codex",
            "--install",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("install_target: "))
        .stdout(predicate::str::contains("installed: true"));

    let instructions = fs::read_to_string(install_target).expect("read install target");
    assert!(instructions.contains("# BEGIN hive-memory:policy"));
    assert!(instructions.contains("# BEGIN hive-memory:codex"));
    assert!(instructions.contains(&format!("@{}", output.display())));
    assert!(!instructions.contains("Installed render memory."));
}

#[test]
fn render_uninstall_removes_adapter_marker() {
    let dir = temp_dir("render-uninstall");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let output = dir.join("generated").join("codex.md");
    let install_target = dir.join("AGENTS.md");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "{}"

            [adapters.codex]
            enabled = true
            stores = ["personal"]
            scopes = ["global"]
            output = "{}"
            install_target = "{}"
            install_mode = "include"
            "#,
            personal.display(),
            output.display(),
            install_target.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut install = cargo_bin_cmd!("hm");
    install
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "render",
            "codex",
            "--install",
        ])
        .assert()
        .success();

    let mut uninstall = cargo_bin_cmd!("hm");
    uninstall
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "render",
            "codex",
            "--uninstall",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("uninstalled: true"))
        .stdout(predicate::str::contains("output:").not());

    let instructions = fs::read_to_string(install_target).expect("read install target");
    assert!(instructions.contains("# BEGIN hive-memory:policy"));
    assert!(!instructions.contains("# BEGIN hive-memory:codex"));
}

#[test]
fn refresh_rebuilds_indexes_and_renders_enabled_adapters() {
    let dir = temp_dir("refresh");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let output = dir.join("generated").join("codex.md");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "{}"

            [adapters.codex]
            enabled = true
            stores = ["personal"]
            scopes = ["global"]
            output = "{}"
            "#,
            personal.display(),
            output.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            "Refresh renders this memory.",
        ])
        .assert()
        .success();

    let mut refresh = cargo_bin_cmd!("hm");
    refresh
        .args(["--config", config.to_str().expect("utf8 config"), "refresh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("refresh: indexes=1"))
        .stdout(predicate::str::contains("rendered=1"))
        .stdout(predicate::str::contains("written=1"));

    let rendered = fs::read_to_string(output).expect("read render output");
    assert!(rendered.contains("Refresh renders this memory."));
}

#[test]
fn refresh_json_reports_maintenance_summary() {
    let dir = temp_dir("refresh-json");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let output = dir.join("generated").join("codex.md");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "{}"

            [adapters.codex]
            enabled = true
            stores = ["personal"]
            scopes = ["global"]
            output = "{}"
            "#,
            personal.display(),
            output.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut refresh = cargo_bin_cmd!("hm");
    refresh
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "refresh",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"indexes\": 1"))
        .stdout(predicate::str::contains("\"flushed\": 0"))
        .stdout(predicate::str::contains("\"skipped\": 0"))
        .stdout(predicate::str::contains("\"failed\": 0"))
        .stdout(predicate::str::contains("\"unbound\": 0"))
        .stdout(predicate::str::contains("\"pending\": 0"))
        .stdout(predicate::str::contains("\"rendered\": 1"))
        .stdout(predicate::str::contains("\"written\": 1"))
        .stdout(predicate::str::contains("\"render_skipped\": false"))
        .stdout(predicate::str::contains("\"forced\": false"))
        .stdout(predicate::str::contains("\"refreshed\": true"));
}

#[test]
fn refresh_honors_no_render_env() {
    let dir = temp_dir("refresh-no-render");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let output = dir.join("generated").join("codex.md");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "{}"

            [adapters.codex]
            enabled = true
            stores = ["personal"]
            scopes = ["global"]
            output = "{}"
            "#,
            personal.display(),
            output.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut refresh = cargo_bin_cmd!("hm");
    refresh
        .env("HIVE_MEMORY_NO_RENDER", "1")
        .args(["--config", config.to_str().expect("utf8 config"), "refresh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rendered=0"))
        .stdout(predicate::str::contains("render_skipped=true"));

    assert!(!output.exists());
}

#[test]
fn hook_session_start_emits_context_action_json() {
    let dir = temp_dir("hook-session-start");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            "Hook context includes durable memory.",
        ])
        .assert()
        .success();

    let mut hook = cargo_bin_cmd!("hm");
    hook.env("HIVE_MEMORY_SESSION_ID", "hook-binding")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "hook",
            "session-start",
            "--project",
            "/repo/src/main.rs",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"event\": \"session-start\""))
        .stdout(predicate::str::contains("\"kind\": \"inject_context\""))
        .stdout(predicate::str::contains(
            "Hook context includes durable memory.",
        ))
        .stdout(predicate::str::contains("\"context_emitted\": true"));
}

#[test]
fn hook_session_start_resolves_project_binding_from_path_hint() {
    let dir = temp_dir("hook-project-binding");
    let config = dir.join("config.toml");
    let data = dir.join("data");
    let personal = dir.join("personal");
    let work = dir.join("work");
    let repo = dir.join("repo");
    fs::create_dir_all(repo.join("src")).expect("repo");
    fs::write(
        repo.join(".hive-memory-project"),
        "id = \"hook-bound-project\"\n",
    )
    .expect("marker");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"

            [stores.personal]
            root = "{}"

            [stores.work]
            root = "{}"

            [agents.codex]
            default_store = "personal"
            read_stores = ["personal", "work"]
            write_stores = ["personal", "work"]
            "#,
            data.display(),
            personal.display(),
            work.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");
    init_store(&work, "work");

    let mut bind = cargo_bin_cmd!("hm");
    bind.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "projects",
        "bind",
        repo.to_str().expect("utf8 repo"),
        "--store",
        "work",
    ])
    .assert()
    .success();

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "remember",
            "--project",
            repo.to_str().expect("utf8 repo"),
            "--scope",
            "project",
            "--text",
            "Hook path hints should use the bound work store.",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("store: work"));

    let mut hook = cargo_bin_cmd!("hm");
    hook.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "--as-agent",
        "codex",
        "hook",
        "session-start",
        "--project",
        repo.join("src/main.rs").to_str().expect("utf8 project"),
        "--json",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("store: work"))
    .stdout(predicate::str::contains(
        "Hook path hints should use the bound work store.",
    ));
}

#[test]
fn hook_session_start_human_output_is_context() {
    let dir = temp_dir("hook-session-human");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_config(&config, &personal, &work);
    init_store(&personal, "personal");

    let mut hook = cargo_bin_cmd!("hm");
    hook.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "--as-agent",
        "codex",
        "hook",
        "session-start",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("Hive Memory Context"))
    .stdout(predicate::str::contains("agent: codex"));
}

#[test]
fn hook_prompt_submit_records_memory_pending() {
    let dir = temp_dir("hook-prompt-pending");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let state = dir.join("state");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            state_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            state.display(),
            personal.display()
        ),
    )
    .expect("write config");

    let mut prompt = cargo_bin_cmd!("hm");
    prompt
        .env("HIVE_MEMORY_SESSION_ID", "session-1")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "hook",
            "prompt-submit",
            "--text",
            "Please remember this repo prefers cargo-dist releases.",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"event\": \"prompt-submit\""))
        .stdout(predicate::str::contains("\"kind\": \"remind\""))
        .stdout(predicate::str::contains("\"memory_pending\": true"));

    let state_file = state.join("runs/session-1/hook-state.json");
    let state_json = fs::read_to_string(state_file).expect("hook state");
    assert!(state_json.contains("\"memory_pending\": true"));
}

#[test]
fn hook_stop_reminds_when_memory_pending_remains() {
    let dir = temp_dir("hook-stop-pending");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let state = dir.join("state");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            state_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            state.display(),
            personal.display()
        ),
    )
    .expect("write config");

    let mut prompt = cargo_bin_cmd!("hm");
    prompt
        .env("HIVE_MEMORY_SESSION_ID", "session-2")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "hook",
            "prompt-submit",
            "--text",
            "For future reference, this project uses snapshot tests.",
        ])
        .assert()
        .success();

    let mut stop = cargo_bin_cmd!("hm");
    stop.env("HIVE_MEMORY_SESSION_ID", "session-2")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "hook",
            "stop",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"event\": \"stop\""))
        .stdout(predicate::str::contains("\"kind\": \"remind\""))
        .stdout(predicate::str::contains(
            "durable memory intent is still pending",
        ))
        .stdout(predicate::str::contains("\"memory_pending\": true"));
}

#[test]
fn hook_prompt_submit_emits_context_only_when_selection_changes() {
    let dir = temp_dir("hook-context-change");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let state = dir.join("state");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            state_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            state.display(),
            personal.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut start = cargo_bin_cmd!("hm");
    start
        .env("HIVE_MEMORY_SESSION_ID", "session-context")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "hook",
            "session-start",
            "--project",
            "/repo-a/src/main.rs",
            "--json",
        ])
        .assert()
        .success();

    let mut same = cargo_bin_cmd!("hm");
    let same_output = same
        .env("HIVE_MEMORY_SESSION_ID", "session-context")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "hook",
            "prompt-submit",
            "--project",
            "/repo-a/src/main.rs",
            "--text",
            "Please inspect the tests.",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let same_stdout = String::from_utf8(same_output).expect("utf8 stdout");
    assert!(same_stdout.contains("\"context_emitted\": false"));
    assert!(!same_stdout.contains("\"kind\": \"inject_context\""));

    let mut changed = cargo_bin_cmd!("hm");
    changed
        .env("HIVE_MEMORY_SESSION_ID", "session-context")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "hook",
            "prompt-submit",
            "--project",
            "/repo-b/src/main.rs",
            "--text",
            "Please inspect the tests.",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"context_emitted\": true"))
        .stdout(predicate::str::contains("\"kind\": \"inject_context\""));
}

#[test]
fn hook_tool_complete_clears_pending_after_session_write() {
    let dir = temp_dir("hook-tool-complete");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let state = dir.join("state");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            state_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            state.display(),
            personal.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut prompt = cargo_bin_cmd!("hm");
    prompt
        .env("HIVE_MEMORY_SESSION_ID", "session-3")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "hook",
            "prompt-submit",
            "--text",
            "Please remember this project uses release trains.",
        ])
        .assert()
        .success();

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .env("HIVE_MEMORY_SESSION_ID", "session-3")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            "This project uses release trains.",
        ])
        .assert()
        .success();

    let mut tool = cargo_bin_cmd!("hm");
    tool.env("HIVE_MEMORY_SESSION_ID", "session-3")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "hook",
            "tool-complete",
            "--status",
            "0",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"event\": \"tool-complete\""))
        .stdout(predicate::str::contains("\"memory_pending\": false"))
        .stdout(predicate::str::contains("\"write_receipts\": 1"))
        .stdout(predicate::str::contains("\"refreshed\": true"));

    let state_file = state.join("runs/session-3/hook-state.json");
    let state_json = fs::read_to_string(state_file).expect("hook state");
    assert!(state_json.contains("\"memory_pending\": false"));
    assert!(state_json.contains("\"refreshed_receipts\": 1"));
}

#[test]
fn hook_tool_complete_nonzero_status_does_not_clear_pending() {
    let dir = temp_dir("hook-tool-complete-fail");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let state = dir.join("state");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            state_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            state.display(),
            personal.display()
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let mut prompt = cargo_bin_cmd!("hm");
    prompt
        .env("HIVE_MEMORY_SESSION_ID", "session-4")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "hook",
            "prompt-submit",
            "--text",
            "For future reference, failing tools should not clear memory debt.",
        ])
        .assert()
        .success();

    let mut remember = cargo_bin_cmd!("hm");
    remember
        .env("HIVE_MEMORY_SESSION_ID", "session-4")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            "A failing tool wrote this, but completion status failed.",
        ])
        .assert()
        .success();

    let mut tool = cargo_bin_cmd!("hm");
    tool.env("HIVE_MEMORY_SESSION_ID", "session-4")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "hook",
            "tool-complete",
            "--status",
            "1",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"memory_pending\": true"))
        .stdout(predicate::str::contains("\"refresh\": null"));
}

#[test]
fn projects_resolve_uses_git_root_from_file_hint() {
    let dir = temp_dir("projects-resolve-git");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    let repo = dir.join("repo");
    let file = repo.join("src/app.rs");
    write_config(&config, &personal, &work);
    fs::create_dir_all(file.parent().expect("file parent")).expect("repo src");
    fs::write(&file, "fn main() {}\n").expect("source file");
    let init = Command::new("git")
        .args(["-C", repo.to_str().expect("utf8 repo"), "init"])
        .output()
        .expect("git init");
    assert!(init.status.success());
    let remote = Command::new("git")
        .args([
            "-C",
            repo.to_str().expect("utf8 repo"),
            "remote",
            "add",
            "origin",
            "git@github.com:cgraf78/hive-memory.git",
        ])
        .output()
        .expect("git remote");
    assert!(remote.status.success());

    let mut resolve = cargo_bin_cmd!("hm");
    resolve
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "projects",
            "resolve",
            file.to_str().expect("utf8 file"),
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "\"project_source\": \"git-remote\"",
        ))
        .stdout(predicate::str::contains(
            "\"project_id\": \"github-com-cgraf78-hive-memory-",
        ))
        .stdout(predicate::str::contains(format!(
            "\"project_root\": \"{}\"",
            repo.display()
        )))
        .stdout(predicate::str::contains(
            "\"store_source\": \"global-default\"",
        ));
}

#[test]
fn projects_bind_and_unbind_local_store_affinity() {
    let dir = temp_dir("projects-bind");
    let config = dir.join("config.toml");
    let data = dir.join("data");
    let personal = dir.join("personal");
    let work = dir.join("work");
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::write(
        repo.join(".hive-memory-project"),
        "id = \"bound-project\"\n",
    )
    .expect("marker");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"

            [stores.personal]
            root = "{}"

            [stores.work]
            root = "{}"
            "#,
            data.display(),
            personal.display(),
            work.display()
        ),
    )
    .expect("write config");

    let mut bind = cargo_bin_cmd!("hm");
    bind.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "projects",
        "bind",
        repo.to_str().expect("utf8 repo"),
        "--store",
        "work",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("project_id: bound-project"))
    .stdout(predicate::str::contains("store: work"));

    let binding = fs::read_to_string(data.join("projects/bound-project.toml")).expect("binding");
    assert!(binding.contains("store = \"work\""));

    let mut resolve = cargo_bin_cmd!("hm");
    resolve
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "projects",
            "resolve",
            repo.to_str().expect("utf8 repo"),
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"store\": \"work\""))
        .stdout(predicate::str::contains(
            "\"store_source\": \"project-binding\"",
        ));

    let mut unbind = cargo_bin_cmd!("hm");
    unbind
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "projects",
            "unbind",
            repo.to_str().expect("utf8 repo"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed: true"));

    let mut resolve_after = cargo_bin_cmd!("hm");
    resolve_after
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "projects",
            "resolve",
            repo.to_str().expect("utf8 repo"),
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"store\": \"personal\""))
        .stdout(predicate::str::contains(
            "\"store_source\": \"global-default\"",
        ));
}

#[test]
fn project_binding_cannot_bypass_agent_affinity() {
    let dir = temp_dir("projects-bind-affinity");
    let config = dir.join("config.toml");
    let data = dir.join("data");
    let personal = dir.join("personal");
    let work = dir.join("work");
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::write(
        repo.join(".hive-memory-project"),
        "id = \"bound-project\"\n",
    )
    .expect("marker");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"

            [stores.personal]
            root = "{}"

            [stores.work]
            root = "{}"

            [agents.codex]
            default_store = "personal"
            read_stores = ["personal"]
            write_stores = ["personal"]
            "#,
            data.display(),
            personal.display(),
            work.display()
        ),
    )
    .expect("write config");

    let mut bind = cargo_bin_cmd!("hm");
    bind.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "projects",
        "bind",
        repo.to_str().expect("utf8 repo"),
        "--store",
        "work",
    ])
    .assert()
    .success();

    let mut resolve = cargo_bin_cmd!("hm");
    resolve
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "projects",
            "resolve",
            repo.to_str().expect("utf8 repo"),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "agent codex may not read store work",
        ));
}

#[test]
fn projects_bind_validates_active_agent_read_and_write_affinity() {
    let dir = temp_dir("projects-bind-agent-affinity");
    let config = dir.join("config.toml");
    let data = dir.join("data");
    let personal = dir.join("personal");
    let work = dir.join("work");
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::write(
        repo.join(".hive-memory-project"),
        "id = \"bound-project\"\n",
    )
    .expect("marker");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"

            [stores.personal]
            root = "{}"

            [stores.work]
            root = "{}"

            [agents.codex]
            default_store = "personal"
            read_stores = ["personal", "work"]
            write_stores = ["personal"]
            "#,
            data.display(),
            personal.display(),
            work.display()
        ),
    )
    .expect("write config");

    let mut bind = cargo_bin_cmd!("hm");
    bind.args([
        "--config",
        config.to_str().expect("utf8 config"),
        "--as-agent",
        "codex",
        "projects",
        "bind",
        repo.to_str().expect("utf8 repo"),
        "--store",
        "work",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "agent codex may not write store work",
    ));
}
