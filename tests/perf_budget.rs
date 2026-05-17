use assert_cmd::cargo::cargo_bin_cmd;
use hive_memory::config::Sensitivity;
use hive_memory::note::{self, Confidence};
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

    assert!(
        context_p95 <= CONTEXT_WARM_BUDGET_MS,
        "hm context p95 {context_p95}ms exceeded {CONTEXT_WARM_BUDGET_MS}ms"
    );
    assert!(
        search_p95 <= SEARCH_WARM_BUDGET_MS,
        "hm search p95 {search_p95}ms exceeded {SEARCH_WARM_BUDGET_MS}ms"
    );
}

struct SyntheticStore {
    config: PathBuf,
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
        Self { config }
    }

    fn refresh_index(&self) {
        self.hm(["refresh", "--force", "--quiet"]);
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
                audience: Vec::new(),
            },
            &options,
            || format!("synthetic-{index:05}"),
        )
        .expect("write synthetic note");
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
