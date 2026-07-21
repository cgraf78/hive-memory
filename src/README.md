# Hive Memory Rust Core

This directory owns the `hm` CLI and library implementation.

## Module Ownership

- `main.rs` and `lib.rs` are entrypoints. `main.rs` owns top-level parsing,
  shared CLI context, dispatch, error rendering, and exit status policy.
- `cli/*.rs` modules own one command family's argument types, structured output
  models, and handlers. Reusable behavior still belongs in library modules.
- `config.rs`, `path.rs`, and `context.rs` resolve runtime configuration and
  project-aware defaults.
- `memory.rs`, `note.rs`, `write.rs`, and `visibility.rs` own stored record
  semantics.
- `store.rs`, `index.rs`, `search.rs`, and `id.rs` own persistence and lookup.
- `curated.rs` and `curation.rs` own injected-context selection.
- `llm.rs` owns backend detection, prompt construction, subprocess deadlines,
  and structured verdict parsing for classifier workers.
- `classify.rs` owns the background classifier worker: pending selection,
  lock/stamp policy, backend failover, and provenance updates.
- `doctor.rs`, `secret.rs`, and `hook.rs` own operational checks and integration
  helpers.
- `outbox.rs` and cloud-sync related modules own sync state and deferred remote
  work.

## Design Notes

Keep durable vocabulary centralized in the module that owns the persisted data.
CLI output can change more freely than stored schema, event names, IDs, or
visibility semantics.

Tests should prefer library calls for core behavior and CLI tests for argument
parsing, output contracts, and integration flows.
