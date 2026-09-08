# AGENTS.md

## About

`hive-memory` is vendor-neutral durable memory for AI agents. The CLI
is `hm`. Canonical data is files on disk: Markdown notes with TOML
front matter, JSON event sidecars, and curated Markdown. Indexes and
caches are rebuildable. [SPEC.md](SPEC.md) is normative for v1
behavior; [PLAN.md](PLAN.md) is design rationale.

## Architecture

- `src/lib.rs` owns reusable policy and data handling.
- `src/main.rs` and `src/cli/` are the CLI adapter: parsing, dispatch,
  output, and exit status.
- `src/README.md` lists module ownership. Persist vocabulary in the
  module that owns the stored schema; do not retype event names, IDs,
  or visibility semantics at call sites.

## Invariants

- Writes are append-only. Newer facts hide stale ones at query time
  via supersession; nothing is hard-deleted.
- Indexes are a recall optimization only. Store, scope, project,
  audience, and validity are mandatory post-filters.
- Secret-looking content is refused on the write path. Report detector
  IDs only; never echo matched secret text.
- Capture/reconcile must not silently change agent-visible memory.
- Tests use temporary stores and explicit env overrides. Do not touch
  a developer's real `hm` store, project state, or cloud credentials.

## Testing

CI Rust job (shared `cgraf78/actions` rust-ci):

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS='-D missing-docs' cargo doc --locked --no-deps
```

CI shell job:

```sh
tests/shell/install-test
tests/shell/release-scripts-test
tests/shell/automation-test
```

Ignored suites (separate CI jobs):

```sh
cargo test --release --locked --test perf_budget -- --ignored --nocapture --test-threads=1
cargo test --locked --test cloud_sync_sim -- --ignored --nocapture
```

Behavior or file-format changes need a SPEC.md update.
