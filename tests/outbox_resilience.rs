//! Black-box resilience tests for `hm flush`.
//!
//! These exercise the durability contract of the outbox: a single corrupt or
//! unflushable item must never strand the rest of the queued offline writes, and
//! the per-item safety gates (manifest identity, path traversal, payload hash,
//! event completeness) must keep refusing unsafe writes while the batch as a
//! whole keeps making progress.
//!
//! The suite is intentionally separate from `tests/cli.rs` to avoid the conflict
//! hotspot there; it re-implements the minimal config/store/outbox fixtures it
//! needs against the public `hive_memory` API.

use assert_cmd::cargo::cargo_bin_cmd;
use hive_memory::{outbox, store};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Create a unique temp directory for one test.
fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "hive-memory-outbox-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

/// Layout shared by every test: a config file, a data dir, and a store root.
struct Fixture {
    config: PathBuf,
    data: PathBuf,
    store_root: PathBuf,
}

/// Write a single-store config and return the paths it references.
///
/// The store root is NOT initialized here so callers can choose between a
/// reachable store (init it) and an unreachable one (leave it missing).
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

            [stores.personal]
            root = "{}"
            "#,
            data.display(),
            store_root.display()
        ),
    )
    .expect("write config");
    Fixture {
        config,
        data,
        store_root,
    }
}

/// Initialize the store root via `hm stores init` and return its manifest id.
fn init_store(store_root: &Path) -> String {
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
    store::read_manifest(store_root)
        .expect("read manifest")
        .store
        .id
}

/// Enqueue a pending note-only outbox item by writing files directly.
fn write_note_item(
    data_dir: &Path,
    item_id: &str,
    expected_store_id: Option<String>,
    final_note_path: &str,
    note_body: &[u8],
) {
    let item_dir = data_dir.join("outbox").join("personal").join(item_id);
    fs::create_dir_all(&item_dir).expect("create outbox item");
    fs::write(item_dir.join("note.md"), note_body).expect("write outbox note");
    let meta = outbox::OutboxMeta {
        schema_version: outbox::OUTBOX_SCHEMA_VERSION,
        id: item_id.to_owned(),
        store: "personal".to_owned(),
        expected_store_id,
        final_note_path: final_note_path.to_owned(),
        note_sha256: outbox::payload_sha256(note_body),
        final_event_path: None,
        event_sha256: None,
        created_at: "2999-01-01T00:00:00Z".to_owned(),
        attempt_count: 0,
        last_error: None,
        state: outbox::OutboxState::Pending,
    };
    fs::write(
        item_dir.join("meta.toml"),
        outbox::render_meta(&meta).expect("render outbox meta"),
    )
    .expect("write outbox meta");
}

/// Run `hm flush --json` and return (success, stdout).
fn run_flush(config: &Path) -> (bool, String) {
    let output = cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "flush",
            "--json",
        ])
        .output()
        .expect("run hm flush");
    (
        output.status.success(),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
    )
}

/// Locate one item's result block in the pretty-printed JSON report.
///
/// The report serializes items in scan order with `id`/`result` keys; we assert
/// on the textual association rather than parsing JSON to keep the test free of
/// extra deps, matching how `tests/cli.rs` checks flush output.
fn item_result(stdout: &str, id: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();
    let id_line = lines
        .iter()
        .position(|line| line.contains(&format!("\"id\": \"{id}\"")))
        .unwrap_or_else(|| panic!("item id {id} not found in report:\n{stdout}"));
    // `result` follows `id` within the same item object in pretty output.
    lines[id_line..]
        .iter()
        .find_map(|line| {
            line.trim()
                .strip_prefix("\"result\": \"")
                .and_then(|rest| rest.strip_suffix("\","))
        })
        .unwrap_or_else(|| panic!("no result after id {id} in report:\n{stdout}"))
        .to_owned()
}

/// Primary fix: one corrupt item must not strand a healthy sibling.
///
/// Before the fix, the corrupt `meta.toml` propagated as a hard `Err` from the
/// per-item loop, so the whole batch aborted and the reachable, valid item B was
/// never flushed -- the exact durability hole the outbox exists to prevent.
/// After the fix, A is bucketed as `failed` and B still lands in the store. The
/// process still exits non-zero because `failed > 0` is the existing CLI
/// contract (owned by `main.rs`, out of scope here); the property under test is
/// that B survived A.
#[test]
fn corrupt_item_does_not_strand_healthy_sibling() {
    let fx = fixture("corrupt-sibling");
    let store_id = init_store(&fx.store_root);

    let a_final = "inbox/notes/2026/05/16/item-a.md";
    let b_final = "inbox/notes/2026/05/16/item-b.md";
    let b_body = b"healthy note body\n";
    write_note_item(
        &fx.data,
        "item-a",
        Some(store_id.clone()),
        a_final,
        b"corrupt sibling body\n",
    );
    write_note_item(&fx.data, "item-b", Some(store_id), b_final, b_body);

    // Corrupt A's metadata so it cannot be parsed.
    fs::write(
        fx.data.join("outbox/personal/item-a/meta.toml"),
        b"this is not valid toml = = =\n",
    )
    .expect("corrupt meta");

    let (success, stdout) = run_flush(&fx.config);
    assert!(!success, "exit is non-zero while a failed item remains");
    assert!(stdout.contains("\"flushed\": 1"), "report:\n{stdout}");
    assert!(stdout.contains("\"failed\": 1"), "report:\n{stdout}");
    assert_eq!(item_result(&stdout, "item-a"), "failed");
    assert_eq!(item_result(&stdout, "item-b"), "flushed");

    // The healthy item B reached the store despite A being unparseable.
    assert_eq!(
        fs::read(fx.store_root.join(b_final)).expect("read flushed B"),
        b_body
    );
    // A is left in place for human repair; nothing was written for it.
    assert!(fx.data.join("outbox/personal/item-a").is_dir());
    assert!(!fx.store_root.join(a_final).exists());
}

/// Identity-mismatch rejection: a wrong manifest id must refuse to flush.
#[test]
fn identity_mismatch_refuses_and_retains_item() {
    let fx = fixture("identity-mismatch");
    init_store(&fx.store_root);

    let final_note = "inbox/notes/2026/05/16/mismatch.md";
    write_note_item(
        &fx.data,
        "mismatch",
        Some("store-id-that-does-not-match".to_owned()),
        final_note,
        b"mismatch body\n",
    );

    let (success, stdout) = run_flush(&fx.config);
    assert!(!success, "identity mismatch is a failure");
    assert!(stdout.contains("\"failed\": 1"), "report:\n{stdout}");
    assert!(
        stdout.contains("manifest id does not match"),
        "report:\n{stdout}"
    );
    assert_eq!(item_result(&stdout, "mismatch"), "failed");

    // Nothing written, item retained for inspection.
    assert!(!fx.store_root.join(final_note).exists());
    assert!(fx.data.join("outbox/personal/mismatch").is_dir());
}

/// Path-traversal guard: a `..` final path must be refused and write nothing
/// outside the store root.
#[test]
fn path_traversal_is_refused() {
    let fx = fixture("path-traversal");
    let store_id = init_store(&fx.store_root);

    // `../escape.md` would land a sibling of the store root if unguarded.
    write_note_item(
        &fx.data,
        "traversal",
        Some(store_id),
        "../escape.md",
        b"escape attempt\n",
    );

    let (success, stdout) = run_flush(&fx.config);
    assert!(!success, "path traversal is a failure");
    assert!(stdout.contains("\"failed\": 1"), "report:\n{stdout}");
    assert_eq!(item_result(&stdout, "traversal"), "failed");

    // The escape target, computed relative to the store root, must not exist.
    let escape_target = fx
        .store_root
        .parent()
        .expect("store parent")
        .join("escape.md");
    assert!(!escape_target.exists(), "traversal escaped the store root");
    assert!(fx.data.join("outbox/personal/traversal").is_dir());
}

/// Payload hash mismatch: tampering with the queued note bytes must be caught.
#[test]
fn payload_hash_mismatch_is_refused() {
    let fx = fixture("hash-mismatch");
    let store_id = init_store(&fx.store_root);

    let final_note = "inbox/notes/2026/05/16/tampered.md";
    write_note_item(
        &fx.data,
        "tampered",
        Some(store_id),
        final_note,
        b"original body\n",
    );
    // Mutate the queued payload so its hash no longer matches the metadata.
    fs::write(
        fx.data.join("outbox/personal/tampered/note.md"),
        b"tampered body\n",
    )
    .expect("tamper note");

    let (success, stdout) = run_flush(&fx.config);
    assert!(!success, "hash mismatch is a failure");
    assert!(stdout.contains("\"failed\": 1"), "report:\n{stdout}");
    assert!(
        stdout.contains("payload hash does not match"),
        "report:\n{stdout}"
    );
    assert_eq!(item_result(&stdout, "tampered"), "failed");

    // Destination untouched.
    assert!(!fx.store_root.join(final_note).exists());
    assert!(fx.data.join("outbox/personal/tampered").is_dir());
}

/// Event payload happy path: a note + event item writes and archives both.
#[test]
fn event_payload_is_written_and_archived() {
    let fx = fixture("event-ok");
    let store_id = init_store(&fx.store_root);

    let final_note = "inbox/notes/2026/05/16/with-event.md";
    let final_event = "inbox/events/2026/05/16/with-event.json";
    let note_body = b"note with event\n";
    let event_body = b"{\"kind\":\"test\"}\n";

    let item_dir = fx.data.join("outbox/personal/with-event");
    fs::create_dir_all(&item_dir).expect("create item");
    fs::write(item_dir.join("note.md"), note_body).expect("write note");
    fs::write(item_dir.join("event.json"), event_body).expect("write event");
    let meta = outbox::OutboxMeta {
        schema_version: outbox::OUTBOX_SCHEMA_VERSION,
        id: "with-event".to_owned(),
        store: "personal".to_owned(),
        expected_store_id: Some(store_id),
        final_note_path: final_note.to_owned(),
        note_sha256: outbox::payload_sha256(note_body),
        final_event_path: Some(final_event.to_owned()),
        event_sha256: Some(outbox::payload_sha256(event_body)),
        created_at: "2999-01-01T00:00:00Z".to_owned(),
        attempt_count: 0,
        last_error: None,
        state: outbox::OutboxState::Pending,
    };
    fs::write(
        item_dir.join("meta.toml"),
        outbox::render_meta(&meta).expect("render meta"),
    )
    .expect("write meta");

    let (success, stdout) = run_flush(&fx.config);
    assert!(success, "clean event flush should succeed:\n{stdout}");
    assert!(stdout.contains("\"flushed\": 1"), "report:\n{stdout}");
    assert!(stdout.contains("\"failed\": 0"), "report:\n{stdout}");
    assert_eq!(item_result(&stdout, "with-event"), "flushed");

    assert_eq!(
        fs::read(fx.store_root.join(final_note)).expect("read note"),
        note_body
    );
    assert_eq!(
        fs::read(fx.store_root.join(final_event)).expect("read event"),
        event_body
    );

    // Both payloads were archived under the per-host snapshot tree.
    let archive_root = fx.store_root.join(".outbox-archive");
    let archived_event = fs::read_dir(&archive_root)
        .expect("archive hosts")
        .flat_map(|host| fs::read_dir(host.expect("host").path()).expect("archive dates"))
        .map(|date| {
            date.expect("date")
                .path()
                .join("with-event")
                .join("event.json")
        })
        .find(|path| path.is_file())
        .expect("archived event");
    assert_eq!(
        fs::read(archived_event).expect("read archived event"),
        event_body
    );
}

/// Incomplete event metadata: an event path with no hash must be refused.
#[test]
fn incomplete_event_metadata_is_refused() {
    let fx = fixture("event-incomplete");
    let store_id = init_store(&fx.store_root);

    let final_note = "inbox/notes/2026/05/16/incomplete.md";
    let note_body = b"note body\n";
    let item_dir = fx.data.join("outbox/personal/incomplete");
    fs::create_dir_all(&item_dir).expect("create item");
    fs::write(item_dir.join("note.md"), note_body).expect("write note");
    fs::write(item_dir.join("event.json"), b"{}\n").expect("write event");
    let meta = outbox::OutboxMeta {
        schema_version: outbox::OUTBOX_SCHEMA_VERSION,
        id: "incomplete".to_owned(),
        store: "personal".to_owned(),
        expected_store_id: Some(store_id),
        final_note_path: final_note.to_owned(),
        note_sha256: outbox::payload_sha256(note_body),
        // Path present but hash missing: the (Some, None) incomplete case.
        final_event_path: Some("inbox/events/2026/05/16/incomplete.json".to_owned()),
        event_sha256: None,
        created_at: "2999-01-01T00:00:00Z".to_owned(),
        attempt_count: 0,
        last_error: None,
        state: outbox::OutboxState::Pending,
    };
    fs::write(
        item_dir.join("meta.toml"),
        outbox::render_meta(&meta).expect("render meta"),
    )
    .expect("write meta");

    let (success, stdout) = run_flush(&fx.config);
    assert!(!success, "incomplete event metadata is a failure");
    assert!(stdout.contains("\"failed\": 1"), "report:\n{stdout}");
    assert!(
        stdout.contains("event path/hash metadata is incomplete"),
        "report:\n{stdout}"
    );
    assert_eq!(item_result(&stdout, "incomplete"), "failed");
    assert!(!fx.store_root.join(final_note).exists());
    assert!(fx.data.join("outbox/personal/incomplete").is_dir());
}

/// Pending on unreachable store: a missing store root must leave the item
/// pending (not failed) and exit success so hook-time flush is safe.
#[test]
fn unreachable_store_leaves_item_pending() {
    // Do NOT init the store root: it stays missing/unreachable.
    let fx = fixture("unreachable");

    let final_note = "inbox/notes/2026/05/16/pending.md";
    write_note_item(
        &fx.data,
        "pending-item",
        // A known identity is required for a pending (vs unbound) item.
        Some("any-known-store-id".to_owned()),
        final_note,
        b"pending body\n",
    );

    let (success, stdout) = run_flush(&fx.config);
    assert!(success, "pending is non-fatal:\n{stdout}");
    assert!(stdout.contains("\"pending\": 1"), "report:\n{stdout}");
    assert!(stdout.contains("\"failed\": 0"), "report:\n{stdout}");
    assert_eq!(item_result(&stdout, "pending-item"), "pending");

    // Item retained unchanged for a later retry.
    assert!(fx.data.join("outbox/personal/pending-item").is_dir());
}
