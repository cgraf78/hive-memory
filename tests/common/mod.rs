//! Support shared by the integration-test crates that spawn the `hm` binary.
//!
//! Each `tests/*.rs` file is its own crate; they pull this in with
//! `mod common;`. Living in a subdirectory keeps Cargo from compiling it as a
//! test target of its own. Every crate compiles its own copy and uses only
//! some of the helpers, so each public helper allows `dead_code`.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

thread_local! {
    /// Root for this thread's `temp_dir()` scratch directories, removed with
    /// everything under it when the thread exits.
    static SCRATCH_ROOT: tempfile::TempDir = tempfile::Builder::new()
        .prefix(concat!("hive-memory-", env!("CARGO_CRATE_NAME"), "-"))
        .tempdir()
        .expect("create scratch root");
}

/// Create a new, empty directory labelled `name` for one test's stores and
/// configs, removed when the test ends.
///
/// libtest runs each test on its own thread and joins it before exiting, so
/// tying the directory to the thread deletes it on pass or fail without every
/// test holding a guard. Every call returns a distinct directory, even for a
/// repeated `name`. A detached background `hm` that writes after its test ends
/// can recreate part of one; the random names keep that harmless.
#[allow(dead_code)]
pub fn temp_dir(name: &str) -> PathBuf {
    SCRATCH_ROOT.with(|root| {
        tempfile::Builder::new()
            .prefix(&format!("{name}-"))
            // tempfile defaults to 0700. Keep the umask-style 0755 that the
            // old `create_dir_all` fixtures had, so store-permission checks see
            // an ordinary directory unless a test chmods it on purpose.
            .permissions(std::fs::Permissions::from_mode(0o755))
            .tempdir_in(root.path())
            .expect("create scratch dir")
            // The thread-scoped root owns cleanup, so the path can outlive this
            // handle for the rest of the test.
            .keep()
    })
}

thread_local! {
    /// This thread's XDG sandbox. `TempDir` creates a new, uniquely named
    /// directory, so a run never inherits state another run left behind (e.g.
    /// after PID reuse), and removes it when the thread exits.
    static XDG_SANDBOX: tempfile::TempDir = tempfile::Builder::new()
        .prefix(concat!("hive-memory-", env!("CARGO_CRATE_NAME"), "-xdg-"))
        .tempdir()
        .expect("create XDG sandbox");
}

/// Build an `hm` command isolated from the caller's hm environment and home.
///
/// - `hm` reads `HIVE_MEMORY_STORE`, `_CONFIG`, `_PROJECT`, `_PROJECT_ID`,
///   `_SESSION_ID`, `_AGENT_ID`, `_HOOK_ACTIVE`, and others at runtime, and
///   several of them override `--config` or the test's own flags. A developer
///   or agent shell that exports them (agent sessions usually do) would aim
///   tests at a store the temp config does not define, or flip hook and
///   project behavior. The whole `HIVE_MEMORY_*` namespace is scrubbed rather
///   than a fixed list so a newly added selector cannot reintroduce the leak.
/// - A config that omits `data_dir`, `state_dir`, or `cache_dir` falls back to
///   the XDG base directories, i.e. the developer's real `~/.local/share`,
///   `~/.local/state`, and `~/.cache`. The XDG dirs therefore point into a
///   sandbox that starts empty and belongs to the calling thread: every spawn
///   from one test shares it, no two tests or runs do, and it is deleted when
///   the thread exits. libtest runs each test on its own thread and joins it
///   before exiting, so a passing or failing test normally leaves nothing
///   behind. A killed run can leak its sandboxes, and a detached background
///   `hm` (hook-scheduled refresh or classify) that writes after its test ends
///   can recreate part of one. Neither is reused: names are random. If libtest
///   cannot spawn a test thread it runs the test on the main thread, where such
///   tests share one sandbox that may outlive the run.
/// - With `HIVE_MEMORY_CONFIG` scrubbed, a spawn that forgets `--config` would
///   resolve the developer's live `~/.config/hive-memory/config.toml`. The
///   sandboxed `XDG_CONFIG_HOME` makes it find no config and fail loudly.
///
/// Tests that need a selector or directory set it explicitly with `.env(..)`
/// afterwards, which takes precedence.
// `clippy.toml` disallows the raw constructors everywhere else so new tests
// cannot bypass this builder.
#[expect(clippy::disallowed_macros)]
#[allow(dead_code)]
pub fn hermetic_hm() -> assert_cmd::Command {
    let mut command = assert_cmd::cargo::cargo_bin_cmd!("hm");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("HIVE_MEMORY_") {
            command.env_remove(&key);
        }
    }
    let sandbox = XDG_SANDBOX.with(|dir| dir.path().to_path_buf());
    command
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("cache"));
    command
}
