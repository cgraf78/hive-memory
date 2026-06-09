# Tests

This directory contains Rust integration tests for Hive Memory.

- `cli.rs` covers the command-line surface and common user workflows.
- `perf_budget.rs` tracks search/context performance budgets and is run as an
  ignored release-mode test in CI.
- `cloud_sync_sim.rs` simulates cloud sync behavior without requiring live
  credentials.

Use temporary stores and explicit environment overrides in tests. Do not depend
on the developer's real `hm` database, project state, or cloud credentials.
