# Tests

Shell-level distribution contracts live under `tests/shell/`:

- `install-test` builds a schema-faithful synthetic archive and exercises the
  generated standalone installer, including payload activation, idempotent
  updates, checksum rollback, and user-owned destination preservation. The
  package-smoke CI job separately installs the real archive emitted by the
  release packager.
- `release-scripts-test` owns Hive Memory's release configuration and payload
  declarations; generic release machinery remains tested in `cgraf78/actions`.

This directory contains Rust integration tests for Hive Memory.

- `cli.rs` covers the command-line surface and common user workflows.
- `perf_budget.rs` tracks search/context performance budgets and is run as an
  ignored release-mode test in CI.
- `cloud_sync_sim.rs` simulates cloud sync behavior without requiring live
  credentials.

Use temporary stores and explicit environment overrides in tests. Do not depend
on the developer's real `hm` database, project state, or cloud credentials.
