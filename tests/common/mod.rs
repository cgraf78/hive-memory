//! Support shared by the integration-test crates that spawn the `hm` binary.
//!
//! Each `tests/*.rs` file is its own crate; they pull this in with
//! `mod common;`. Living in a subdirectory keeps Cargo from compiling it as a
//! test target of its own. Every crate compiles its own copy and uses only
//! some of the helpers, so each public helper allows `dead_code`.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

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

/// Names of the caller's environment variables that start with `prefix`.
fn inherited_vars(prefix: &str) -> Vec<OsString> {
    std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| key.to_string_lossy().starts_with(prefix))
        .collect()
}

/// Build a `git` command for test fixtures, isolated from the caller's
/// repository and Git config.
///
/// Git exports `GIT_DIR`, `GIT_INDEX_FILE`, and friends to hooks, and agent
/// shells may export them too. Inherited, they make `git -C <fixture> init`
/// reinitialize the caller's repository and `remote add` write the fixture's
/// remote into its config. The whole `GIT_*` namespace is dropped, then global
/// and system config are ignored so a developer's `init.defaultBranch`, hooks
/// path, or URL rewrites cannot change fixtures (`GIT_CONFIG_GLOBAL` needs Git
/// 2.32, which every CI platform has). Ignoring global config also drops
/// `user.name`, so a fixed identity keeps future commit-based fixtures working
/// on any host. The XDG dirs share the calling test's `hermetic_hm()` sandbox,
/// which also hides Git's default `$XDG_CONFIG_HOME/git/ignore` and
/// `attributes` and keeps a `git` wrapper script on `PATH` from writing into
/// the developer's real XDG dirs; `BASH_ENV` is dropped so such a wrapper
/// cannot source the caller's startup file.
#[allow(dead_code)]
pub fn git() -> Command {
    let mut command = Command::new("git");
    for key in inherited_vars("GIT_") {
        command.env_remove(key);
    }
    command.env_remove("BASH_ENV").envs(xdg_sandbox_env());
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Hive Memory Tests")
        .env("GIT_AUTHOR_EMAIL", "tests@hive-memory.invalid")
        .env("GIT_COMMITTER_NAME", "Hive Memory Tests")
        .env("GIT_COMMITTER_EMAIL", "tests@hive-memory.invalid");
    command
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
/// - Inherited `GIT_*` variables are dropped for the same reason as in
///   [`git`].
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
    for key in inherited_vars("HIVE_MEMORY_") {
        command.env_remove(key);
    }
    // Defensive and not exercised by any test: `hm` reads `.git/config` itself
    // and shells out to `git remote get-url` only when that parse fails, but an
    // inherited `GIT_DIR` would then answer with the caller's remote.
    for key in inherited_vars("GIT_") {
        command.env_remove(key);
    }
    command.envs(xdg_sandbox_env());
    command
}

/// The XDG base-directory variables pointed into this thread's sandbox.
fn xdg_sandbox_env() -> [(&'static str, PathBuf); 4] {
    let sandbox = XDG_SANDBOX.with(|dir| dir.path().to_path_buf());
    [
        ("XDG_CONFIG_HOME", sandbox.join("config")),
        ("XDG_DATA_HOME", sandbox.join("data")),
        ("XDG_STATE_HOME", sandbox.join("state")),
        ("XDG_CACHE_HOME", sandbox.join("cache")),
    ]
}
