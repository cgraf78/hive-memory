//! Contract tests for the shared `tests/common` helpers.
//!
//! Every other integration crate trusts `common::hermetic_hm()` to keep
//! spawned `hm` processes away from the developer's real XDG state and from
//! each other, and `common::temp_dir()` to clean up after each test, so those
//! isolation and cleanup guarantees are pinned here rather than inferred from
//! the suites that use them.

use std::fs;
use std::path::{Path, PathBuf};

mod common;

/// The sandbox root a `hermetic_hm()` command points its XDG dirs into.
fn sandbox(command: &assert_cmd::Command) -> PathBuf {
    let data_home = command
        .get_envs()
        .find(|(key, _)| *key == "XDG_DATA_HOME")
        .and_then(|(_, value)| value)
        .expect("hermetic_hm sets XDG_DATA_HOME");
    Path::new(data_home)
        .parent()
        .expect("XDG_DATA_HOME has a sandbox parent")
        .to_path_buf()
}

/// Every XDG dir must hang off one sandbox root, so the root is the unit that
/// gets created, shared within a test, and deleted.
#[test]
fn xdg_dirs_share_one_sandbox_root() {
    let command = common::hermetic_hm();
    let root = sandbox(&command);
    for (key, leaf) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        let value = command
            .get_envs()
            .find(|(name, _)| *name == key)
            .and_then(|(_, value)| value)
            .unwrap_or_else(|| panic!("hermetic_hm sets {key}"));
        assert_eq!(Path::new(value), root.join(leaf), "{key}");
    }
}

/// A run must never inherit a sandbox an earlier run left behind (keying on
/// PID alone made that possible after PID reuse), so the first spawn in a test
/// sees a directory that exists and is empty.
#[test]
fn sandbox_starts_empty() {
    let root = sandbox(&common::hermetic_hm());
    let entries = fs::read_dir(&root)
        .unwrap_or_else(|error| panic!("sandbox {} must exist: {error}", root.display()))
        .count();
    assert_eq!(entries, 0, "fresh sandbox {} is not empty", root.display());
}

/// Spawns within one test share state: a test that runs `hm` twice against a
/// config without `data_dir` relies on the second spawn seeing the first's
/// XDG writes.
#[test]
fn sandbox_is_stable_within_a_test() {
    let first = sandbox(&common::hermetic_hm());
    fs::create_dir_all(first.join("data/hive-memory")).expect("write into sandbox");
    let second = sandbox(&common::hermetic_hm());
    assert_eq!(first, second);
    assert!(second.join("data/hive-memory").is_dir());
}

/// libtest runs each test on its own named thread and joins it, so removing
/// the sandbox at thread exit is what keeps the temp dir from accumulating
/// one directory per test per run.
#[test]
fn sandbox_is_removed_when_its_thread_exits() {
    let root = std::thread::Builder::new()
        .name("hermetic_hm::cleanup_probe".to_owned())
        .spawn(|| {
            let root = sandbox(&common::hermetic_hm());
            fs::create_dir_all(root.join("state/hive-memory")).expect("write into sandbox");
            root
        })
        .expect("spawn probe thread")
        .join()
        .expect("probe thread");
    assert!(
        !root.exists(),
        "sandbox {} outlived its thread",
        root.display()
    );
}

/// Threads outside libtest have no test name. They must still get a private
/// sandbox instead of silently sharing one fallback directory.
#[test]
fn unnamed_threads_get_distinct_sandboxes() {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    // Hold both threads alive until each has resolved its sandbox, so the
    // comparison cannot pass merely because the first one was already gone.
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                let root = sandbox(&common::hermetic_hm());
                barrier.wait();
                root
            })
        })
        .collect();
    let roots: Vec<PathBuf> = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker thread"))
        .collect();
    assert_ne!(roots[0], roots[1]);
    let own = sandbox(&common::hermetic_hm());
    assert!(!roots.contains(&own), "worker shared the test's sandbox");
}

/// `clippy.toml` bans assert_cmd's binary-path helpers outside
/// `common::hermetic_hm()`, but Clippy cannot see a raw `std::process::Command`
/// built from Cargo's binary-path variable, or one that runs whatever `hm` is
/// first on `PATH` (the developer's install, with their live config). Catch
/// both bypasses by source scan.
#[test]
fn only_the_hermetic_builder_spawns_hm() {
    // Assembled at runtime so this file does not match its own needles.
    let needles = [
        ["CARGO_BIN", "_EXE_"].concat(),
        ["Command::new(", "\"hm\")"].concat(),
    ];
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut offenders = Vec::new();
    let mut pending = vec![tests_dir];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read tests dir") {
            let path = entry.expect("tests dir entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = fs::read_to_string(&path).expect("read test source");
                for needle in &needles {
                    if source.contains(needle.as_str()) {
                        offenders.push(format!("{}: {needle}", path.display()));
                    }
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "spawn hm through common::hermetic_hm(): {offenders:?}"
    );
}

/// The sandbox is keyed to the calling thread, which is only per-test because
/// libtest gives each test its own thread named after the test.
#[test]
fn each_test_runs_on_its_own_named_thread() {
    assert_eq!(
        std::thread::current().name(),
        Some("each_test_runs_on_its_own_named_thread")
    );
}

#[test]
fn temp_dirs_are_distinct_and_empty() {
    let first = common::temp_dir("same");
    let second = common::temp_dir("same");
    assert_ne!(first, second);
    for dir in [&first, &second] {
        assert_eq!(fs::read_dir(dir).expect("scratch dir exists").count(), 0);
    }
}

/// Store and config fixtures used to be left in the temp dir by every run.
#[test]
fn temp_dirs_are_removed_when_their_thread_exits() {
    let dir = std::thread::spawn(|| {
        let dir = common::temp_dir("cleanup-probe");
        fs::write(dir.join("config.toml"), "").expect("write into scratch dir");
        dir
    })
    .join()
    .expect("probe thread");
    assert!(!dir.exists(), "{} outlived its thread", dir.display());
}

/// Set only on the child copy of this binary that
/// `hostile_caller_environment_cannot_redirect_hm` spawns; it turns
/// `hostile_env_probe` from a no-op into the real check.
const PROBE_HOME: &str = "HM_HERMETIC_PROBE_HOME";

/// Ordinary CI runs with a clean environment, so nothing else would notice if
/// `hermetic_hm()` stopped scrubbing `HIVE_MEMORY_*` or sandboxing XDG: the
/// only symptom would be writes into a developer's real home. Re-run this
/// binary's probe with a caller environment aimed at a sentinel home and
/// require that nothing lands there. Changing the process's own environment
/// instead would race every other test in this binary.
#[test]
fn hostile_caller_environment_cannot_redirect_hm() {
    let home = common::temp_dir("hostile-home");
    // A live config where the caller's XDG_CONFIG_HOME says one is. A spawn
    // that forgets `--config` must not find it.
    let xdg_trap_config = home.join(".config/hive-memory/config.toml");
    fs::create_dir_all(xdg_trap_config.parent().expect("trap config dir"))
        .expect("create trap config dir");
    fs::write(
        &xdg_trap_config,
        format!(
            "default_store = \"trap\"\n\n[stores.trap]\nroot = \"{}\"\n",
            home.join("xdg-trap-store").display()
        ),
    )
    .expect("write XDG trap config");
    let trap_config = home.join("trap-config.toml");
    fs::write(
        &trap_config,
        format!(
            "default_store = \"trap\"\n\n[stores.trap]\nroot = \"{}\"\n",
            home.join("trap-store").display()
        ),
    )
    .expect("write trap config");

    let output = std::process::Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--exact", "hostile_env_probe", "--test-threads=1"])
        .env(PROBE_HOME, &home)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("HIVE_MEMORY_CONFIG", &trap_config)
        .env("HIVE_MEMORY_STORE", "trap")
        .output()
        .expect("run probe");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "probe failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Only the traps planted above may exist; anything else was written by a
    // spawned process.
    let planted = [
        trap_config.clone(),
        home.join(".config"),
        home.join(".config/hive-memory"),
        xdg_trap_config.clone(),
    ];
    let mut leaked = Vec::new();
    let mut pending = vec![home.clone()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read sentinel home") {
            let path = entry.expect("sentinel home entry").path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            if !planted.contains(&path) {
                leaked.push(path);
            }
        }
    }
    assert!(
        leaked.is_empty(),
        "a spawned process wrote into the caller's home: {leaked:?}"
    );
}

/// The child half of `hostile_caller_environment_cannot_redirect_hm`: run
/// commands that write the store manifest, a record, and the XDG-fallback
/// data, state, and cache, from a config that sets none of those directories,
/// then one that forgets `--config`.
#[test]
fn hostile_env_probe() {
    if std::env::var_os(PROBE_HOME).is_none() {
        return;
    }
    let dir = common::temp_dir("probe");
    let store = dir.join("store");
    let config = dir.join("config.toml");
    fs::write(
        &config,
        format!(
            "default_store = \"personal\"\n\n[stores.personal]\nroot = \"{}\"\n",
            store.display()
        ),
    )
    .expect("write probe config");
    let config = config.to_str().expect("utf8 config");
    let store = store.to_str().expect("utf8 store");
    for args in [
        vec!["stores", "init", "personal", "--root", store],
        vec!["--config", config, "remember", "--text", "probe fact"],
        vec!["--config", config, "search", "probe"],
        vec!["--config", config, "refresh", "--force"],
    ] {
        common::hermetic_hm().args(&args).assert().success();
    }

    // With `HIVE_MEMORY_CONFIG` scrubbed and XDG sandboxed, a forgotten
    // `--config` finds neither the caller's env-selected config nor their XDG
    // one, and fails instead of reading or writing a trap store.
    let home = std::env::var_os(PROBE_HOME).expect("probe home");
    let forgot = common::hermetic_hm()
        .args(["stores", "list"])
        .output()
        .expect("run hm without --config");
    let stderr = String::from_utf8_lossy(&forgot.stderr);
    assert!(
        !forgot.status.success() && stderr.contains("failed to read config"),
        "a spawn without --config must find no config: {stderr}"
    );
    assert!(
        !stderr.contains(&*Path::new(&home).to_string_lossy()),
        "a spawn without --config looked in the caller's home: {stderr}"
    );
}
