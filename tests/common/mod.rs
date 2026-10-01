//! Support shared by the integration-test crates that spawn the `hm` binary.
//!
//! Each `tests/*.rs` file is its own crate; they pull this in with
//! `mod common;`. Living in a subdirectory keeps Cargo from compiling it as a
//! test target of its own.

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
///   `~/.local/state`, and `~/.cache`. Each test therefore gets its own XDG
///   sandbox, keyed by crate, process, and test name so parallel tests and
///   concurrent runs never share local state.
/// - With `HIVE_MEMORY_CONFIG` scrubbed, a spawn that forgets `--config` would
///   resolve the developer's live `~/.config/hive-memory/config.toml`. The
///   sandboxed `XDG_CONFIG_HOME` makes it find no config and fail loudly.
///
/// Tests that need a selector or directory set it explicitly with `.env(..)`
/// afterwards, which takes precedence.
pub fn hermetic_hm() -> assert_cmd::Command {
    let mut command = assert_cmd::cargo::cargo_bin_cmd!("hm");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("HIVE_MEMORY_") {
            command.env_remove(&key);
        }
    }
    let thread = std::thread::current();
    let test_name = thread.name().unwrap_or("unnamed-test");
    let sandbox = std::env::temp_dir().join(format!(
        "hive-memory-{}-xdg-{}-{}",
        env!("CARGO_CRATE_NAME"),
        std::process::id(),
        hive_memory::hash::sha256_hex(test_name.as_bytes())
    ));
    command
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("cache"));
    command
}
