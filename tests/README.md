# Tests

Shell-level distribution contracts live under `tests/shell/`:

- `install-test` builds a schema-faithful synthetic archive and exercises the
  generated standalone installer, including payload activation, idempotent
  updates, checksum rollback, and user-owned destination preservation. The
  package-smoke CI job separately installs the real archive emitted by the
  release packager.
- `release-scripts-test` owns Hive Memory's release configuration and payload
  declarations; generic release machinery remains tested in `cgraf78/actions`.
- `automation-test` checks repository-owned dependency-automation documentation
  contracts, such as the README keeping locked source installs.

This directory contains Rust integration tests for Hive Memory.

- `cli.rs` covers the command-line surface and common user workflows.
- `perf_budget.rs` tracks search/context performance budgets and is run as an
  ignored release-mode test in CI.
- `cloud_sync_sim.rs` simulates cloud sync behavior without requiring live
  credentials.
- `hermetic_hm.rs` pins the isolation and cleanup contract of the shared
  `tests/common` helpers.

Use temporary stores and explicit environment overrides in tests. Do not depend
on the developer's real `hm` database, project state, or cloud credentials.
Create scratch directories with `common::temp_dir()` (unit tests in `src/` use
`crate::test_support::temp_dir()`), which removes them when the test ends.
Spawn the binary through `common::hermetic_hm()` (`tests/common/mod.rs`): it
drops inherited `HIVE_MEMORY_*` selectors and gives each test its own fresh XDG
config/data/state/cache sandbox, removed when the test finishes, so a spawn
that forgets `--config` cannot read the developer's live config. `clippy.toml`
rejects assert_cmd's raw binary constructors everywhere else. Run fixture
`git` through `common::git()`, which drops inherited `GIT_*` variables such as
a hook's `GIT_DIR` and ignores global and system Git config. The shell
suites unset `BASH_ENV` in `helpers.sh` so their bash stubs never source the
caller's startup file.
