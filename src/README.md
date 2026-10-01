# Hive Memory Rust Core

This directory owns the `hm` CLI and library implementation.

## Module Ownership

- `main.rs` and `lib.rs` are entrypoints. `main.rs` owns top-level parsing,
  shared CLI context, dispatch, error rendering, and exit status policy, and
  still hosts the remember/note, retag, classify, capture, reconcile, and
  doctor command handlers.
- `cli/*.rs` modules own one command family's argument types, structured output
  models, and handlers. Reusable behavior still belongs in library modules.
- `config.rs` loads, layers, and validates configuration and agent store policy.
- `project.rs` resolves project identity (explicit ids, markers, VCS remotes,
  path keys), aliases, and local project bindings.
- `path.rs` owns store-relative path normalization for metadata.
- `memory.rs` composes the logical record write; `note.rs` (Markdown front
  matter and inbox layout), `event.rs` (JSON sidecars), `write.rs` (atomic
  file publishing), and `id.rs` (sortable write ids) own the underlying formats
  and primitives. `hash.rs` holds shared stable hash formatting.
- `visibility.rs`, `validity.rs`, and `supersession.rs` own read-time audience,
  currentness, and stale-memory suppression rules.
- `store.rs` owns store manifests and initialization; `index.rs` owns the
  rebuildable local triage index.
- `search.rs` owns deterministic lexical search; `retrieval.rs` owns the
  rebuildable Tantivy BM25 backend; `entity.rs` owns deterministic entity
  alias extraction for recall.
- `context.rs` assembles trust-labeled agent context; `inject.rs` owns
  read-time session-start selection; `curated.rs` discovers curated memory
  files; `curation.rs` owns inbox triage and `hm promote`.
- `write_classify.rs` owns deterministic write-time kind inference;
  `signals.rs` holds text-shape signals shared with `inject.rs`.
- `llm.rs` owns model backend detection, prompt construction, subprocess
  deadlines, and structured verdict parsing.
  `classify.rs` is the background classifier worker, `capture.rs` extracts
  candidate facts for staging, and `reconcile.rs` decides mem0-style
  ADD/UPDATE/DELETE/NOOP operations; the command layer applies them.
- `hook.rs` owns agent lifecycle hook session state and the prompt heuristic.
- `outbox.rs` owns the durable local offline outbox and `hm flush`.
- `doctor.rs` and `secret.rs` own operational diagnostics/repairs and write-path
  secret detection.
- `eval.rs` owns retrieval eval corpus and fixture helpers; `version.rs`
  reports embedded build version metadata.
- `test_support.rs` (unit tests only) owns self-cleaning scratch directories.

## Design Notes

Keep durable vocabulary centralized in the module that owns the persisted data.
CLI output can change more freely than stored schema, event names, IDs, or
visibility semantics.

Tests should prefer library calls for core behavior and CLI tests for argument
parsing, output contracts, and integration flows.
