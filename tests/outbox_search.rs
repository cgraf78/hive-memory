//! Queued outbox payloads must stay visible to `hm search` until flushed.
//!
//! A pending write is acknowledged user data: hiding it from reads loses memory
//! the user already saved. These tests cover the search half of that contract:
//! pending items appear in text and JSON output marked `pending`, project-only
//! search includes them, unbound items warn without failing, and an empty
//! outbox keeps output byte-identical.

use hive_memory::{note, outbox, store, write};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

macro_rules! cargo_bin_cmd {
    ("hm") => {{
        let mut command = assert_cmd::cargo::cargo_bin_cmd!("hm");
        let thread = std::thread::current();
        let test_name = thread.name().unwrap_or("unnamed-outbox-search-test");
        let sandbox = std::env::temp_dir().join(format!(
            "hive-memory-outbox-search-xdg-{}-{}",
            std::process::id(),
            hive_memory::hash::sha256_hex(test_name.as_bytes())
        ));
        command
            .env("XDG_DATA_HOME", sandbox.join("data"))
            .env("XDG_STATE_HOME", sandbox.join("state"))
            .env("XDG_CACHE_HOME", sandbox.join("cache"));
        command
    }};
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "hive-memory-outbox-search-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

struct Fixture {
    config: PathBuf,
    data: PathBuf,
    store_id: String,
}

/// Single-store config with state/cache isolated under the temp dir, and an
/// initialized reachable store.
fn fixture(name: &str) -> Fixture {
    let dir = temp_dir(name);
    let config = dir.join("config.toml");
    let data = dir.join("data");
    let store_root = dir.join("store");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"
            state_dir = "{}"
            cache_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            data.display(),
            dir.join("state").display(),
            dir.join("cache").display(),
            store_root.display()
        ),
    )
    .expect("write config");
    cargo_bin_cmd!("hm")
        .args([
            "stores",
            "init",
            "personal",
            "--root",
            store_root.to_str().expect("utf8 store root"),
        ])
        .assert()
        .success();
    let store_id = store::read_manifest(&store_root)
        .expect("read manifest")
        .store
        .id;
    Fixture {
        config,
        data,
        store_id,
    }
}

/// Render a valid note payload through the production front-matter builder so
/// queued fixtures parse exactly like real writes.
fn render_note(
    id: &str,
    store_id: &str,
    scope: &str,
    project_id: Option<&str>,
    body: &str,
) -> Vec<u8> {
    let input = note::NoteWriteInput {
        entry_kind: note::EntryKind::Remember,
        store_id: store_id.to_owned(),
        store_name: "personal".to_owned(),
        created_at: time::OffsetDateTime::now_utc(),
        agent_id: "test-agent".to_owned(),
        host_id: "test-host".to_owned(),
        scope: scope.to_owned(),
        confidence: note::Confidence::High,
        body: body.to_owned(),
        user_id: None,
        session_id: None,
        project_id: project_id.map(str::to_owned),
        subject: Some(format!("probe {id}")),
        tags: vec!["probe".to_owned()],
        source_kind: None,
        source_ref: None,
        related_event_id: None,
        expires_at: None,
        valid_from: None,
        valid_to: None,
        supersedes: Vec::new(),
        kind: None,
        classified: None,
        audience: Vec::new(),
    };
    let front_matter = input
        .front_matter(id.to_owned())
        .expect("valid test front matter");
    note::render_note(&note::MarkdownNote {
        front_matter,
        body: body.to_owned(),
    })
    .expect("render test note")
    .into_bytes()
}

/// Queue one note-only item through the real outbox envelope.
fn enqueue(
    data_dir: &Path,
    store: &str,
    id: &str,
    expected_store_id: Option<String>,
    state: outbox::OutboxState,
    note: Vec<u8>,
) {
    outbox::enqueue(outbox::EnqueueInput {
        data_dir,
        store,
        id,
        expected_store_id,
        final_note_path: format!("inbox/notes/2026/09/09/{id}.md"),
        note,
        final_event_path: None,
        event: None,
        state,
        options: write::AtomicWriteOptions::default(),
    })
    .expect("enqueue fixture item");
}

/// Run `hm search` and return (success, stdout, stderr).
fn run_search(config: &Path, extra: &[&str], query: &str) -> (bool, String, String) {
    let mut args = vec!["--config", config.to_str().expect("utf8 config"), "search"];
    args.extend(extra.iter().copied());
    args.push(query);
    let output = cargo_bin_cmd!("hm")
        .args(&args)
        .output()
        .expect("run hm search");
    (
        output.status.success(),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8(output.stderr).expect("utf8 stderr"),
    )
}

fn remember(config: &Path, text: &str) {
    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            text,
        ])
        .assert()
        .success();
}

/// A pending item for a reachable store appears in text search output marked
/// `pending`, while canonical hits stay unmarked.
#[test]
fn pending_item_appears_in_text_search_marked_pending() {
    let fixture = fixture("text-pending");
    remember(
        &fixture.config,
        "canonical memory about parallactic-canonical",
    );
    enqueue(
        &fixture.data,
        "personal",
        "pending-probe-1",
        Some(fixture.store_id.clone()),
        outbox::OutboxState::Pending,
        render_note(
            "pending-probe-1",
            &fixture.store_id,
            "global",
            None,
            "queued memory about parallactic-pending",
        ),
    );

    let (success, stdout, stderr) = run_search(&fixture.config, &[], "parallactic-pending");
    assert!(success, "search failed: {stderr}");
    assert!(
        stdout.contains("id: pending-probe-1"),
        "pending hit missing:\n{stdout}"
    );
    assert!(
        stdout.contains("pending: true"),
        "pending marker missing:\n{stdout}"
    );

    // Canonical hits must not gain a pending marker.
    let (success, stdout, stderr) = run_search(&fixture.config, &[], "parallactic-canonical");
    assert!(success, "search failed: {stderr}");
    assert!(
        stdout.contains("parallactic-canonical"),
        "canonical hit missing:\n{stdout}"
    );
    assert!(
        !stdout.contains("pending: true"),
        "canonical hit wrongly marked pending:\n{stdout}"
    );
}

/// JSON output marks only the pending hit; canonical hits keep their shape.
#[test]
fn pending_item_appears_in_json_search_marked_pending() {
    let fixture = fixture("json-pending");
    remember(&fixture.config, "canonical memory about sonorous-canonical");
    enqueue(
        &fixture.data,
        "personal",
        "pending-probe-2",
        Some(fixture.store_id.clone()),
        outbox::OutboxState::Pending,
        render_note(
            "pending-probe-2",
            &fixture.store_id,
            "global",
            None,
            "queued memory about sonorous-pending",
        ),
    );

    let (success, stdout, stderr) = run_search(&fixture.config, &["--json"], "sonorous-pending");
    assert!(success, "search failed: {stderr}");
    assert!(
        stdout.contains("\"id\": \"pending-probe-2\""),
        "pending hit missing:\n{stdout}"
    );
    assert!(
        stdout.contains("\"pending\": true"),
        "pending marker missing:\n{stdout}"
    );

    let (success, stdout, stderr) = run_search(&fixture.config, &["--json"], "sonorous-canonical");
    assert!(success, "search failed: {stderr}");
    assert!(
        !stdout.contains("\"pending\""),
        "canonical hit gained a pending key:\n{stdout}"
    );
}

/// Project-only search includes queued project-scoped payloads and still
/// excludes queued payloads from other scopes.
#[test]
fn project_only_search_includes_pending_project_item() {
    let fixture = fixture("project-pending");
    enqueue(
        &fixture.data,
        "personal",
        "pending-probe-project",
        Some(fixture.store_id.clone()),
        outbox::OutboxState::Pending,
        render_note(
            "pending-probe-project",
            &fixture.store_id,
            "project",
            Some("probe-project"),
            "queued project memory about tesseractic-project",
        ),
    );
    enqueue(
        &fixture.data,
        "personal",
        "pending-probe-global",
        Some(fixture.store_id.clone()),
        outbox::OutboxState::Pending,
        render_note(
            "pending-probe-global",
            &fixture.store_id,
            "global",
            None,
            "queued global memory about tesseractic-project",
        ),
    );

    let (success, stdout, stderr) = run_search(
        &fixture.config,
        &["--project-only", "--project-id", "probe-project"],
        "tesseractic-project",
    );
    assert!(success, "search failed: {stderr}");
    assert!(
        stdout.contains("id: pending-probe-project"),
        "project pending hit missing:\n{stdout}"
    );
    assert!(
        stdout.contains("pending: true"),
        "pending marker missing:\n{stdout}"
    );
    assert!(
        !stdout.contains("pending-probe-global"),
        "global pending item leaked into project-only search:\n{stdout}"
    );
}

/// Unbound items cannot flush, but they must neither fail the read nor hide:
/// search succeeds, the item is marked pending, and the binding repair stays
/// surfaced as a warning.
#[test]
fn unbound_item_warns_without_failing_search() {
    let fixture = fixture("unbound");
    enqueue(
        &fixture.data,
        "personal",
        "unbound-probe-1",
        None,
        outbox::OutboxState::Unbound,
        render_note(
            "unbound-probe-1",
            "placeholder-store-id",
            "global",
            None,
            "queued unbound memory about crepuscular-unbound",
        ),
    );

    let (success, stdout, stderr) = run_search(&fixture.config, &[], "crepuscular-unbound");
    assert!(success, "search failed: {stderr}");
    assert!(
        stdout.contains("id: unbound-probe-1"),
        "unbound hit missing:\n{stdout}"
    );
    assert!(
        stdout.contains("pending: true"),
        "pending marker missing:\n{stdout}"
    );
    assert!(
        stderr.contains("1 outbox item(s) require explicit store binding"),
        "unbound warning missing:\n{stderr}"
    );
}

/// An empty outbox keeps search output byte-identical: queuing an item for a
/// different store must not change this store's stdout or stderr at all.
#[test]
fn empty_outbox_keeps_search_output_identical() {
    let fixture = fixture("empty-identical");
    remember(&fixture.config, "canonical memory about velutinous-stable");

    let (first_success, first_out, first_err) =
        run_search(&fixture.config, &[], "velutinous-stable");
    assert!(first_success, "first search failed: {first_err}");

    // An item queued for another store alias is filtered from this store's
    // read, exercising the scan-runs-but-nothing-applies path.
    enqueue(
        &fixture.data,
        "other",
        "other-store-item",
        Some("other-store-id".to_owned()),
        outbox::OutboxState::Pending,
        render_note(
            "other-store-item",
            "other-store-id",
            "global",
            None,
            "queued memory about velutinous-stable for another store",
        ),
    );

    let (second_success, second_out, second_err) =
        run_search(&fixture.config, &[], "velutinous-stable");
    assert!(second_success, "second search failed: {second_err}");
    assert_eq!(
        second_out, first_out,
        "stdout changed with no applicable outbox items"
    );
    assert_eq!(
        second_err, first_err,
        "stderr changed with no applicable outbox items"
    );
    assert!(
        !second_out.contains("pending"),
        "unexpected pending marker:\n{second_out}"
    );
}
