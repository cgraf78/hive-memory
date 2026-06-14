use assert_cmd::cargo::cargo_bin_cmd;
use hive_memory::config::Sensitivity;
use hive_memory::note::{self, Confidence};
use hive_memory::outbox;
use hive_memory::store::{self, StoreInitOptions};
use hive_memory::write::{AtomicWriteOptions, FsyncPolicy};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

const SYNTHETIC_NOTES: usize = 5_000;
const RUNS: usize = 30;
// These are user-facing CLI budgets, not microbenchmarks. They intentionally
// include process startup and JSON serialization because agent hooks pay those
// costs every time they ask Hive Memory for context.
const CONTEXT_WARM_BUDGET_MS: u128 = 200;
const SEARCH_WARM_BUDGET_MS: u128 = 300;
const HOOK_PROMPT_BASELINE_WARM_BUDGET_MS: u128 = 300;
const HOOK_PROMPT_RECALL_WARM_BUDGET_MS: u128 = 350;
const HOOK_PROMPT_CACHED_OFFLINE_BUDGET_MS: u128 = 350;
const HOOK_TOOL_COMPLETE_NO_RECEIPT_WARM_BUDGET_MS: u128 = 200;
const SYNTHETIC_OUTBOX_ITEMS: usize = 100;
const FLUSH_RUNS: usize = 10;
const FLUSH_100_ITEM_BUDGET_MS: u128 = 2_000;
const PERF_BUDGET_MULTIPLIER_ENV: &str = "HIVE_MEMORY_PERF_BUDGET_MULTIPLIER";

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "hive-memory-perf-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

#[test]
#[ignore = "CI runs this explicitly because it creates a 5000-note synthetic store"]
fn context_and_search_stay_within_warm_budget() {
    let fixture = SyntheticStore::new();
    fixture.refresh_index();

    // Include process startup in the budget: hooks and agent launchers invoke
    // `hm` as a CLI, so an in-process microbenchmark would miss the latency the
    // user actually feels.
    let context_p95 = p95_ms(repeat(RUNS, || {
        fixture.hm(["context", "--json", "--max-tokens", "4000"])
    }));
    let search_p95 = p95_ms(repeat(RUNS, || {
        fixture.hm(["search", "needle-4999", "--json"])
    }));
    eprintln!("hm context warm p95: {context_p95}ms");
    eprintln!("hm search warm p95: {search_p95}ms");

    let context_budget = budget_ms(CONTEXT_WARM_BUDGET_MS);
    let search_budget = budget_ms(SEARCH_WARM_BUDGET_MS);
    assert!(
        context_p95 <= context_budget,
        "hm context p95 {context_p95}ms exceeded {context_budget}ms"
    );
    assert!(
        search_p95 <= search_budget,
        "hm search p95 {search_p95}ms exceeded {search_budget}ms"
    );
}

#[test]
#[ignore = "CI runs this explicitly because it creates a 5000-note synthetic store"]
fn semantic_and_supersession_search_stay_within_warm_budget() {
    let fixture = SyntheticStore::new();
    fixture.refresh_index();

    let semantic_p95 = p95_ms(repeat(RUNS, || {
        fixture.hm([
            "search",
            "where are coding agent rules documented",
            "--json",
        ])
    }));
    let supersession_p95 = p95_ms(repeat(RUNS, || {
        fixture.hm(["search", "before committing", "--json"])
    }));
    eprintln!("hm semantic search warm p95: {semantic_p95}ms");
    eprintln!("hm supersession search warm p95: {supersession_p95}ms");

    let search_budget = budget_ms(SEARCH_WARM_BUDGET_MS);
    assert!(
        semantic_p95 <= search_budget,
        "hm semantic search p95 {semantic_p95}ms exceeded {search_budget}ms"
    );
    assert!(
        supersession_p95 <= search_budget,
        "hm supersession search p95 {supersession_p95}ms exceeded {search_budget}ms"
    );
}

#[test]
#[ignore = "CI runs this explicitly because it creates synthetic outbox stores"]
fn flush_100_item_outbox_stays_within_budget() {
    let flush_p95 = p95_ms(
        (0..FLUSH_RUNS)
            .map(|run| {
                let fixture = FlushFixture::new(run);
                let start = Instant::now();
                fixture.hm(["flush", "--json"]);
                start.elapsed()
            })
            .collect(),
    );
    eprintln!("hm flush 100-item p95: {flush_p95}ms");

    let flush_budget = budget_ms(FLUSH_100_ITEM_BUDGET_MS);
    assert!(
        flush_p95 <= flush_budget,
        "hm flush 100-item p95 {flush_p95}ms exceeded {flush_budget}ms"
    );
}

#[test]
#[ignore = "CI runs this explicitly because it measures full hook CLI latency"]
fn hook_prompt_submit_baseline_stays_within_warm_budget() {
    let fixture = SyntheticStore::new();
    fixture.refresh_index();

    let prompt_p95 = p95_ms(repeat(RUNS, || {
        hm_command(
            &fixture.config,
            [
                "--as-agent",
                "codex",
                "hook",
                "prompt-submit",
                "--project",
                "/tmp/hive-memory-perf-project/src/main.rs",
                "--text",
                "Please inspect the project tests.",
                "--json",
            ],
        )
        .env("HIVE_MEMORY_SESSION_ID", "perf-prompt-baseline")
        .assert()
        .success();
    }));
    eprintln!("hm hook prompt-submit baseline warm p95: {prompt_p95}ms");

    let prompt_budget = budget_ms(HOOK_PROMPT_BASELINE_WARM_BUDGET_MS);
    assert!(
        prompt_p95 <= prompt_budget,
        "hm hook prompt-submit baseline p95 {prompt_p95}ms exceeded {prompt_budget}ms"
    );
}

#[test]
#[ignore = "CI runs this explicitly because it measures full hook recall latency"]
fn hook_prompt_submit_recall_stays_within_warm_budget() {
    let fixture = SyntheticStore::new();
    fixture.refresh_index();

    let prompt_p95 = p95_ms(repeat(RUNS, || {
        hm_command(
            &fixture.config,
            [
                "--as-agent",
                "codex",
                "hook",
                "prompt-submit",
                "--project",
                "/tmp/hive-memory-perf-project/src/main.rs",
                "--text",
                "needle-4999",
                "--json",
            ],
        )
        .env("HIVE_MEMORY_SESSION_ID", "perf-prompt-recall")
        .assert()
        .success();
    }));
    eprintln!("hm hook prompt-submit recall warm p95: {prompt_p95}ms");

    let prompt_budget = budget_ms(HOOK_PROMPT_RECALL_WARM_BUDGET_MS);
    assert!(
        prompt_p95 <= prompt_budget,
        "hm hook prompt-submit recall p95 {prompt_p95}ms exceeded {prompt_budget}ms"
    );
}

#[test]
#[ignore = "CI runs this explicitly because it measures full hook recall latency"]
fn hook_prompt_submit_cached_recall_stays_fast_when_store_root_is_unavailable() {
    let fixture = SyntheticStore::new();
    fixture.refresh_index();
    let offline_root = fixture.root.with_extension("offline");
    fs::rename(&fixture.root, &offline_root).expect("move synthetic store offline");

    let prompt_p95 = p95_ms(repeat(RUNS, || {
        hm_command(
            &fixture.config,
            [
                "--as-agent",
                "codex",
                "hook",
                "prompt-submit",
                "--project",
                "/tmp/hive-memory-perf-project/src/main.rs",
                "--text",
                "needle-4999",
                "--json",
            ],
        )
        .env("HIVE_MEMORY_SESSION_ID", "perf-prompt-cached-offline")
        .assert()
        .success();
    }));
    eprintln!("hm hook prompt-submit cached-offline p95: {prompt_p95}ms");

    let prompt_budget = budget_ms(HOOK_PROMPT_CACHED_OFFLINE_BUDGET_MS);
    assert!(
        prompt_p95 <= prompt_budget,
        "hm hook prompt-submit cached-offline p95 {prompt_p95}ms exceeded {prompt_budget}ms"
    );
}

#[test]
#[ignore = "CI runs this explicitly because it measures full hook CLI latency"]
fn hook_tool_complete_without_receipts_stays_within_warm_budget() {
    let fixture = SyntheticStore::new();
    fixture.refresh_index();

    hm_command(
        &fixture.config,
        [
            "--as-agent",
            "codex",
            "hook",
            "session-start",
            "--project",
            "/tmp/hive-memory-perf-project/src/main.rs",
            "--json",
        ],
    )
    .env("HIVE_MEMORY_SESSION_ID", "perf-tool-complete")
    .assert()
    .success();

    let tool_p95 = p95_ms(repeat(RUNS, || {
        hm_command(
            &fixture.config,
            [
                "--as-agent",
                "codex",
                "hook",
                "tool-complete",
                "--project",
                "/tmp",
                "--status",
                "0",
                "--json",
            ],
        )
        .env("HIVE_MEMORY_SESSION_ID", "perf-tool-complete")
        .env("HIVE_MEMORY_PROJECT", "/tmp")
        .assert()
        .success();
    }));
    eprintln!("hm hook tool-complete no-receipt warm p95: {tool_p95}ms");

    let tool_budget = budget_ms(HOOK_TOOL_COMPLETE_NO_RECEIPT_WARM_BUDGET_MS);
    assert!(
        tool_p95 <= tool_budget,
        "hm hook tool-complete no-receipt p95 {tool_p95}ms exceeded {tool_budget}ms"
    );
}

fn budget_ms(base_ms: u128) -> u128 {
    let multiplier = std::env::var(PERF_BUDGET_MULTIPLIER_ENV)
        .ok()
        .and_then(|value| value.parse::<u128>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1);
    base_ms * multiplier
}

struct SyntheticStore {
    config: PathBuf,
    root: PathBuf,
}

impl SyntheticStore {
    fn new() -> Self {
        let dir = temp_dir("warm-budget");
        let config = dir.join("config.toml");
        let root = dir.join("personal");
        let cache = dir.join("cache");
        let state = dir.join("state");
        store::init_store(&StoreInitOptions {
            name: "personal".to_owned(),
            root: root.clone(),
            description: Some("Synthetic performance budget store".to_owned()),
            sensitivity: Sensitivity::Private,
        })
        .expect("init synthetic store");
        write_config(&config, &root, &cache, &state);
        write_notes(&root);
        Self { config, root }
    }

    fn refresh_index(&self) {
        self.hm(["refresh", "--force", "--quiet"]);
    }

    fn hm<const N: usize>(&self, args: [&str; N]) {
        hm_command(&self.config, args).assert().success();
    }
}

struct FlushFixture {
    config: PathBuf,
}

impl FlushFixture {
    fn new(run: usize) -> Self {
        let dir = temp_dir(&format!("flush-budget-{run}"));
        let config = dir.join("config.toml");
        let root = dir.join("personal");
        let data = dir.join("data");
        store::init_store(&StoreInitOptions {
            name: "personal".to_owned(),
            root: root.clone(),
            description: Some("Synthetic flush performance budget store".to_owned()),
            sensitivity: Sensitivity::Private,
        })
        .expect("init flush store");
        let manifest = store::read_manifest(&root).expect("read manifest");
        write_flush_config(&config, &root, &data);
        for index in 0..SYNTHETIC_OUTBOX_ITEMS {
            write_outbox_note_item(
                &data,
                "personal",
                &format!("flush-item-{index:03}"),
                Some(manifest.store.id.clone()),
                &format!("inbox/notes/2026/05/17/flush-item-{index:03}.md"),
                format!("flush budget note {index}\n").as_bytes(),
            );
        }
        Self { config }
    }

    fn hm<const N: usize>(&self, args: [&str; N]) {
        hm_command(&self.config, args).assert().success();
    }
}

fn hm_command<const N: usize>(config: &Path, args: [&str; N]) -> assert_cmd::Command {
    let mut command = cargo_bin_cmd!("hm");
    command.arg("--config").arg(config).args(args);
    command
}

fn write_flush_config(config: &Path, root: &Path, data: &Path) {
    fs::write(
        config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            data.display(),
            root.display()
        ),
    )
    .expect("write flush config");
}

fn write_config(config: &Path, root: &Path, cache: &Path, state: &Path) {
    fs::write(
        config,
        format!(
            r#"
            default_store = "personal"
            cache_dir = "{}"
            state_dir = "{}"

            [stores.personal]
            root = "{}"

            [performance]
            context_warm_p95_ms = {}
            context_store_size_target = {}
            "#,
            cache.display(),
            state.display(),
            root.display(),
            CONTEXT_WARM_BUDGET_MS,
            SYNTHETIC_NOTES
        ),
    )
    .expect("write config");
}

fn write_outbox_note_item(
    data_dir: &Path,
    store_name: &str,
    item_id: &str,
    expected_store_id: Option<String>,
    final_note_path: &str,
    note_body: &[u8],
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
        created_at: "2026-05-17T00:00:00Z".to_owned(),
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

fn write_notes(root: &Path) {
    let options = AtomicWriteOptions {
        fsync: FsyncPolicy::Never,
        ..AtomicWriteOptions::default()
    };
    for index in 0..SYNTHETIC_NOTES {
        let created_at =
            OffsetDateTime::from_unix_timestamp(1_778_946_153 + i64::try_from(index).unwrap())
                .expect("timestamp");
        note::write_note_with_id_generator(
            root,
            &note::NoteWriteInput {
                entry_kind: note::EntryKind::Remember,
                store_id: "synthetic-store-id".to_owned(),
                store_name: "personal".to_owned(),
                created_at,
                agent_id: "perf".to_owned(),
                host_id: "ci".to_owned(),
                scope: "global".to_owned(),
                confidence: Confidence::High,
                body: format!("Synthetic memory {index} contains needle-{index}."),
                user_id: None,
                session_id: None,
                project_id: None,
                subject: Some(format!("synthetic.{index}")),
                tags: vec!["perf".to_owned()],
                source_kind: Some("benchmark".to_owned()),
                source_ref: None,
                related_event_id: None,
                expires_at: None,
                valid_from: None,
                valid_to: None,
                supersedes: Vec::new(),
                kind: None,
                classified: None,
                audience: Vec::new(),
            },
            &options,
            || format!("synthetic-{index:05}"),
        )
        .expect("write synthetic note");
    }

    for (offset, id, body) in [
        (
            0_i64,
            "feature-agent-rules",
            "Coding agent instructions live in AGENTS.md and define checkrun rules.",
        ),
        (
            1_i64,
            "feature-old-cargo-fmt",
            "Project alpha used to run cargo fmt before committing.",
        ),
        (
            2_i64,
            "feature-new-checkrun",
            "Project alpha now uses checkrun format and checkrun lint before committing.",
        ),
    ] {
        let created_at = OffsetDateTime::from_unix_timestamp(
            1_778_946_153 + i64::try_from(SYNTHETIC_NOTES).unwrap() + offset,
        )
        .expect("timestamp");
        note::write_note_with_id_generator(
            root,
            &note::NoteWriteInput {
                entry_kind: note::EntryKind::Remember,
                store_id: "synthetic-store-id".to_owned(),
                store_name: "personal".to_owned(),
                created_at,
                agent_id: "perf".to_owned(),
                host_id: "ci".to_owned(),
                scope: "global".to_owned(),
                confidence: Confidence::High,
                body: body.to_owned(),
                user_id: None,
                session_id: None,
                project_id: None,
                subject: Some(id.to_owned()),
                tags: vec!["perf".to_owned(), "feature-perf".to_owned()],
                source_kind: Some("benchmark".to_owned()),
                source_ref: None,
                related_event_id: None,
                expires_at: None,
                valid_from: None,
                valid_to: None,
                supersedes: Vec::new(),
                kind: None,
                classified: None,
                audience: Vec::new(),
            },
            &options,
            || id.to_owned(),
        )
        .expect("write feature perf note");
    }
}

fn repeat<F>(runs: usize, mut f: F) -> Vec<Duration>
where
    F: FnMut(),
{
    (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .collect()
}

fn p95_ms(mut durations: Vec<Duration>) -> u128 {
    durations.sort();
    durations[((durations.len() * 95).div_ceil(100)).saturating_sub(1)].as_millis()
}
