# Hive Memory Design

## Goal

`hive-memory` is generic, vendor-neutral shared memory infrastructure for AI agents.
It provides a durable shared memory substrate that works across agents, hosts,
models, and storage backends, while staying ergonomic for both agents and humans.

The project should avoid assumptions about one person, one agent, Claude Code,
Google Drive, or any specific machine layout. Those are adapters/config, not core
architecture.

The primary binary is `hm` for agent/human ergonomics. `hive-memory` may be
installed as a compatibility alias/wrapper for discoverability, but docs and
hooks should prefer `hm`. Local checks found no obvious `hm` binary collision on
the current Linux host; release planning should still check
Homebrew/Apt/common CLI namespaces before the first public release.

## License

Use the MIT license.

Why MIT: `hive-memory` is intended as small infrastructure that should be easy to
adopt, fork, package, embed in dotfiles, or use from other agent ecosystems. MIT
keeps contribution and distribution friction low while still preserving copyright
and warranty disclaimers.

## Design Principles

These are intentionally aligned with the design principles in Chris's dotfiles
`CLAUDE.md`: clean/elegant design, single-source shared knowledge, clean
interfaces, single-purpose composition, consolidation after repeated use, async
boundary guards, re-entrancy prevention, and isolation by separation rather than
crippling.

- **Favor clean, elegant designs**: keep the system cohesive, readable, and
  nicely componentized. Prefer small, well-named pieces with clear boundaries
  over tangled or overly clever implementation.
- **Single-source shared knowledge**: within a memory store, canonical memory has
  one neutral home. Agent-specific files are rendered views, not competing
  sources of truth. Multiple stores are allowed for intentional segmentation,
  but each store remains internally single-source.
- **Expose clean interfaces**: agents call `hm context`, `remember`,
  `note`, `render`, and `doctor`; they do not reimplement path mapping, locking,
  indexing, or scope filtering.
- **Compose from single-purpose parts**: storage backend, note writer, indexer,
  renderer, compactor, and agent adapters are separate modules with narrow APIs.
- **Consolidate after the second use**: start with Claude/Codex adapters, then
  extract shared adapter helpers before adding more hosts. Avoid premature
  framework abstractions, but do not tolerate three copies of the same logic.
- **Guard at async boundaries**: hooks, delayed compaction, background sync, and
  cloud-drive refresh all revalidate paths, locks, file hashes, and config before
  touching state.
- **Prevent re-entrancy in polled loops**: sync, render, and compaction use local
  run locks so overlapping hooks skip or coalesce instead of stacking work.
- **Isolate by separation, not by crippling**: use scoped render outputs and
  per-agent adapters instead of weakening the canonical store or stripping useful
  capabilities from every consumer.
- **Storage is configurable**: the backend memory root is a config value, not a
  hard-coded path. It may live in Google Drive, Dropbox, Syncthing, a network
  mount, a plain local directory, or a repo checkout.
- **Plain files are the source of truth**: canonical memory is Markdown plus
  small TOML/JSON metadata files in a normal directory tree. Indexes and
  generated views are rebuildable.
- **Append-only writes first**: agents write new immutable event/note files rather
  than editing shared hot files. Curated memory is updated by explicit compaction.
- **Adapters are edges**: Claude, Codex, OpenClaw, Gemini, etc. consume rendered
  views. No agent owns the canonical memory format.
- **Small sharp CLI**: agents should not reimplement filesystem rules. They call
  `hm` for reads, writes, rendering, locking, and diagnostics.
- **Human-legible by default**: a human can browse the memory root and understand
  it without running the CLI.
- **Generated means disposable**: rendered files, search indexes, caches, and lock
  state are local or rebuildable unless explicitly marked canonical.
- **Scope and privacy are first-class**: personal/work/project/agent-private scopes
  are metadata, not naming conventions only.
- **Minimal required human maintenance**: agents should handle routine capture,
  sync, indexing, compaction proposals, and rendering. Humans should review or
  steer important memory changes, not babysit the system daily.

## Non-Goals

- Not a vector database as the canonical store.
- Not a Claude plugin as the architectural center.
- Not a Git workflow requirement for memory writes.
- Not a transcript-hoarding system. Transcripts can be imported/summarized, but
  long-term memory is curated and scoped.

## Platform and Distribution Requirements

Supported platforms for v1:

- Linux
- macOS
- WSL

The implementation and installer must support the same practical platform matrix
as Chris's personal dotfiles/shdeps environment. The project should be
installable as a normal `shdeps` dependency from personal dotfiles.

Why this matters: `hm` runs from agent lifecycle hooks, so installation must be
boring and reliable on every host where agents run. Precompiled binaries avoid
requiring a Rust toolchain during dotfiles bootstrap and make hook startup fast
and predictable.

Distribution requirements:

- Publish precompiled release binaries for every supported platform.
- Provide a `shdeps`-friendly install flow that can download the correct binary
  for the current OS/architecture.
- Keep source builds possible for contributors, but do not require Rust tooling
  on every machine just to install/use `hm`.
- CI should build/test/release the supported targets.

Likely release artifacts use the versioned release format:

```text
hm-vX.Y.Z-aarch64-apple-darwin.tar.gz
hm-vX.Y.Z-x86_64-apple-darwin.tar.gz
hm-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
hm-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz
```

WSL should use the Linux binaries. Musl Linux binaries are a deferred release
decision. If they are practical, add `x86_64-unknown-linux-musl` for broad Linux
compatibility; otherwise document the glibc expectation clearly.

## Implementation Language

Rust is the planned implementation language for this project.

Why Rust fits:

- fast startup and low runtime overhead for lifecycle hooks
- single static-ish binary ergonomics for shdeps installation
- strong cross-platform filesystem/path handling
- good libraries for CLI parsing, TOML, Markdown/front matter, JSON, locking,
  SQLite/FTS, and release automation
- memory safety and explicit error handling for a tool that will touch lots of
  user data
- good learning opportunity without forcing a large app/runtime framework

Planned Rust stack:

- `clap` for CLI
- `serde`, `serde_json`, `toml` for config/events
- `anyhow` for app errors; consider `thiserror` for library modules
- `time` or `chrono` for timestamps
- `uuid`/random suffix or ULID-style IDs for write paths
- plain text/front-matter handling initially; add Markdown parsing only when needed
- simple text search first; `rusqlite`/SQLite FTS later for local indexing
- `assert_cmd`, `predicates`, `tempfile` for CLI tests
- Rust edition 2024 unless target constraints require 2021; document MSRV before
  first release
- Prefer `cargo-dist` for release archives/checksums unless it blocks target needs

Keep the architecture modular but not framework-heavy: storage, writer, index,
renderers, and compactor as clean modules under one binary crate at first. Split
crates only after a second real consumer needs it.

## Configuration

Default config path:

```toml
# ~/.config/hive-memory/config.toml
# TOML is the v1 config format because it is human-editable, comment-friendly,
# and Rust-native via serde. JSON is reserved for structured events/metadata.

default_store = "personal"
state_dir = "${XDG_STATE_HOME:-${HOME}/.local/state}/hive-memory"
cache_dir = "${XDG_CACHE_HOME:-${HOME}/.cache}/hive-memory"

host_id = "auto"      # auto = stable machine id derived by CLI
user_id = "default"   # namespace within each memory store

[stores.personal]
root = "${HOME}/gdrive/hive-memory/personal"
description = "Default personal hive memory"

[stores.work]
root = "${HOME}/gdrive/hive-memory/work"
description = "Optional segmented work memory"

[storage]
kind = "filesystem"   # filesystem first; future: s3, webdav, sqlite, postgres
case_sensitive = "auto"
atomic_rename = "auto"

[defaults]
write_scope = "global"
search_scopes = ["global", "project"]
render_scopes = ["global", "project"]
event_sidecar = "always" # never|always

[privacy]
default_render_policy = "conservative"
allow_all_stores_flag = true
warn_sensitive_broad_render = true

[adapters.claude]
enabled = true
stores = ["personal"]
scopes = ["global", "project"]
output = "${HOME}/.claude/hive-memory.generated.md"

[adapters.codex]
enabled = true
stores = ["personal"]
scopes = ["global", "project"]
output = "${HOME}/.codex/hive-memory.generated.md"
```

Important: store roots are always configurable. Google Drive is just one good
backend for Chris, not a baked-in assumption. A config always has one
`default_store`; additional named stores are optional and are used for memory
segmentation.

Why this matters: config files capture durable human intent, while environment
variables let wrappers, hooks, and agent launchers select the right store/scope
for the current context without editing files. This keeps the system flexible
without making agents guess.

### Configuration Precedence

Use a layered config model so humans can keep durable defaults in files while
agents/hooks can force the correct environment without editing config.

Precedence, highest wins:

1. CLI flags, e.g. `hm --config ... --store work`.
2. Environment variables, e.g. `HIVE_MEMORY_STORE=work`.
3. Local config override file, e.g. `~/.config/hive-memory/config.local.toml`.
4. Main config file, e.g. `~/.config/hive-memory/config.toml`.
5. Built-in defaults.

This gives agents deterministic behavior via env vars and keeps human config
readable. Hooks should generally set env vars for active agent/store/session
identity instead of rewriting config files.

### Environment Variables

Core env vars:

```bash
HIVE_MEMORY_CONFIG=/path/to/config.toml       # config file path
HIVE_MEMORY_ROOT=/path/to/root                # shorthand root override for active/default store
HIVE_MEMORY_STORE=personal                    # active store if --store omitted
HIVE_MEMORY_STORE_PERSONAL_ROOT=/path/to/root # per-store root override
HIVE_MEMORY_STATE_DIR=/path/to/state
HIVE_MEMORY_CACHE_DIR=/path/to/cache
HIVE_MEMORY_HOST_ID=taylor
HIVE_MEMORY_USER_ID=chris
HIVE_MEMORY_AGENT_ID=codex
HIVE_MEMORY_SESSION_ID=<session-id>
HIVE_MEMORY_PROJECT=/path/to/project
HIVE_MEMORY_SCOPE=global
```

Adapter/render env vars:

```bash
HIVE_MEMORY_ADAPTER=codex                     # active adapter hint
HIVE_MEMORY_RENDER_STORES=personal,work       # adapter store allowlist
HIVE_MEMORY_INCLUDE_SCOPES=global,project
HIVE_MEMORY_EXCLUDE_SCOPES=agent-private
```

Behavior toggles:

```bash
HIVE_MEMORY_OFFLINE=1                         # write to local outbox only
HIVE_MEMORY_NO_RENDER=1                       # skip render from hooks
HIVE_MEMORY_NO_COMPACT=1                      # skip compaction/proposals
HIVE_MEMORY_LOG=warn                          # error|warn|info|debug|trace
```

Examples:

```bash
HIVE_MEMORY_ROOT=/path/to/memory hm search "..."
HIVE_MEMORY_CONFIG=/path/to/config.toml HIVE_MEMORY_STORE=work hm doctor
HIVE_MEMORY_AGENT_ID=codex HIVE_MEMORY_SESSION_ID=abc123 hm remember --text "..."
```

## Multiple Stores

`hive-memory` supports multiple named memory stores. There is always one
configured default store, and commands can target another store explicitly.

Use cases:

- personal vs work memory
- client/project segmentation
- experimental/private agent stores
- shared family/team store vs private store
- high-trust local store vs lower-trust shared store

Command model:

```bash
hm search "workflow preference"              # uses default_store
hm --store work search "release checklist"   # explicit store
hm --store personal remember --text "..."
hm stores list
hm stores doctor
```

Rules:

- Every store has its own root, manifest, inbox, memories, locks, and generated
  views.
- Config store keys and CLI store names are local aliases. Prefer lowercase
  `[a-z0-9][a-z0-9_-]*`. The manifest `store.id` UUID is the durable store
  identity; `store.name` is the preferred human-readable name and should usually
  match the configured alias.
- The default store is used when no `--store` is provided.
- Adapters declare which stores they include, and the default should be
  conservative.
- Cross-store search/context is opt-in via `--all-stores` or explicit
  `--stores a,b`. The singular `--store` selects the active write/render store.
- Notes/events record their `store_id` in front matter/JSON metadata.
- Compaction and locks are per-store unless a future cross-store operation is
  explicitly requested.

Avoid accidental leakage: rendering a work store into a personal agent config, or
personal store into a work/client context, must require explicit config.

Benefit: segmentation keeps unrelated memories from contaminating each other and
lets users decide which hives should be available in which environments. A single
default store preserves simple ergonomics for common use, while named stores
support work/client/family/private boundaries without forking the tool.

### Store vs Scope Model

Stores are privacy/trust boundaries: personal, work, client-specific, family, or
team hives. Scopes are categories inside one selected store. V1 built-in scopes
are:

- `global`: broadly relevant within the selected store
- `project`: tied to a project path or project ID
- `agent-private`: available only to an explicitly selected adapter/agent

Custom scopes may be configured later, but `personal` and `work` should normally
be stores, not scopes. This avoids confusing commands like “write personal scope
inside work store” and keeps privacy boundaries enforceable.

## Canonical Directory Layout

```text
<root>/
  manifest.toml
  README.md

  people/
    index.md
    <person-id>.md

  rules/
    personal.md
    coding.md
    work/
      index.md

  memories/
    global/
      MEMORY.md
      PREFERENCES.md
    agents/
      <agent-id>/
        MEMORY.md
    projects/
      <project-id>/
        PROJECT.md
        MEMORY.md
        aliases.toml

  inbox/
    events/
      YYYY/MM/DD/<event-id>.json
    notes/
      YYYY/MM/DD/<note-id>.md

  compactions/
    YYYY/MM/<compaction-id>.md

  generated/
    .gitignore
    # only explicit shared generated artifacts live here; default generated
    # adapter output stays local and rebuildable
```

Why this layout: curated memory, raw inbox entries, generated adapter output, and
local indexes have different lifecycles. Keeping them separate makes it obvious
what humans edit, what agents append, what compaction produces, and what can be
rebuilt or deleted safely.

## Canonical Data Format

Markdown is the canonical durable human-readable memory format. JSON is used for
structured machine events/metadata when it adds value, but should not become the
only understandable source of truth.

V1 format decisions:

- Canonical human memory: `.md` files with small front matter.
- Config and manifests: `.toml`.
- Structured machine events: `.json` files when useful for reliable processing,
  dedupe, and future indexing.
- Local indexes: SQLite/FTS or other index files in local state/cache only,
  rebuildable from canonical Markdown/events.

This gives humans easy browsing/editing while giving agents enough structure to
manage the hive safely.

Raw memory must be durable and non-lossy: compaction creates summaries and
curated updates, but does not delete raw notes/events by default. Retention or
archival policy is explicit and opt-in.

### Why JSON Events Exist

Markdown is great for humans, but agents also need predictable structured data.
JSON event files provide that structure without making the human-readable note
format carry every machine concern.

JSON events are useful for:

- **Reliable indexing**: an indexer can read timestamps, store IDs, scopes,
  project IDs, tags, confidence, and source metadata without scraping prose.
- **Dedupe and idempotency**: event IDs let agents retry writes or imports without
  accidentally recording the same observation multiple times.
- **Audit trails**: structured source fields can record which agent/session/tool
  produced a memory, which helps debug bad memories later.
- **Compaction input**: compactors can select events by type, scope, confidence,
  age, or project before asking a model to summarize.
- **Future integrations**: other tools can consume events without parsing
  Markdown front matter conventions.

JSON events should not replace Markdown. The Markdown note remains the canonical
record a human can read. JSON is a sidecar/event stream for operations that
benefit from strict structure.

### Manifest

```toml
schema_version = 1
created_by = "hive-memory"

[store]
id = "<uuid>"
name = "personal"

[policies]
allow_direct_curated_edits = false
append_only_inbox = true
```

## Event and Note IDs

Every agent write creates a unique file path. IDs must be collision-resistant and
sortable enough for human browsing.

Recommended ID format:

```text
YYYYMMDDTHHMMSS.ffffffZ_<host-id>_<pid>_<agent-id>_<random>
```

Example:

```text
20260516T154233.184921Z_taylor_12345_codex_a8f31c.md
```

This avoids collisions even when many sessions on the same host write at the
same time.

Benefit: unique, sortable filenames make cloud-drive sync safe. Agents do not
need a central coordinator to write memories; they only need to choose a unique
path and atomically publish it.

V1 ID/path rules:

- IDs are extensionless; filenames are `<id>.md` and `<id>.json`.
- Markdown notes and JSON sidecars from the same write share the same ID.
- Random suffix is at least 8 lowercase hex/base32 characters.
- `host_id`, `agent_id`, and similar filename components are sanitized to
  `[a-zA-Z0-9_-]` with other characters replaced by `-`.
- Collision retry limit: at least 5 attempts before returning a write error.

## Concurrency Model

The concurrency model optimizes for cloud-synced folders and many independent
agent sessions. It avoids shared hot files on the write path because cloud sync
systems are good at moving distinct files and bad at merging concurrent edits to
the same file.

### Rule 1: agents do not append to shared hot files

No agent should directly append to:

- `MEMORY.md`
- `PREFERENCES.md`
- project `MEMORY.md`
- daily shared logs

Instead they write a new note/event file under `inbox/`.

### Rule 2: write temp then atomic rename

For each write:

1. create parent directory
2. write to local temp file in the target directory:
   `.tmp.<event-id>.<pid>`
3. fsync when practical
4. rename to final path
5. if final path somehow exists, generate a new ID and retry

On cloud-sync folders, atomic rename is not a global distributed lock, but it is
sufficient when final filenames are unique. Conflict files may still appear if a
backend is strange; `doctor` should detect them.

### Rule 3: compaction uses short-lived lock files

Curated files require coordination. The compactor uses advisory lock directories:

```text
<root>/.locks/compact-global.lock/
  owner.json
```

Acquire by `mkdir`, release by removing. If lock owner heartbeat is stale, the
next compactor may recover after a configurable TTL.

Lock owner metadata:

```json
{
  "host_id": "taylor",
  "pid": 12345,
  "agent_id": "codex",
  "started_at": "2026-05-16T15:42:33Z",
  "expires_at": "2026-05-16T15:47:33Z"
}
```

### Rule 4: local indexes are never shared locks

Search indexes live in `state_dir` or `cache_dir`, not the shared root. Multiple
agents on the same host can coordinate with normal local file locks. Indexes can
always be rebuilt from canonical files.

## Write Types

Writes separate human-readable content from machine-oriented metadata. The goal
is not to duplicate everything forever; it is to preserve a durable note while
providing enough structure for indexing, compaction, and diagnostics.

### Raw note

Human-readable markdown with front matter:

```markdown
---
type: note
entry_kind: remember # remember|note
id: 20260516T154233.184921Z_taylor_12345_codex_a8f31c
created_at: 2026-05-16T15:42:33.184921Z
agent_id: codex
host_id: taylor
session_id: abc123
scope: global
project_id: ds
tags: [preference, workflow]
confidence: high
---

Chris prefers ...
```

### Structured event

JSON for reliable machine processing:

```json
{
  "type": "memory.observation",
  "id": "...",
  "created_at": "...",
  "agent_id": "codex",
  "host_id": "taylor",
  "scope": "global",
  "subject": "workflow.preference",
  "body": "...",
  "source": {
    "kind": "session",
    "session_id": "..."
  }
}
```

The CLI writes the Markdown note as the canonical record. It may also write a
JSON sidecar/event from the same operation when structured processing needs it.

## CLI Surface

The CLI is the stable contract for agents. Hooks and prompts should call `hm`
instead of reimplementing path rules, store selection, locking, or metadata
formatting. Human commands should make inspection and correction easy when agent
automation gets something wrong.

Agent-optimized commands:

```bash
hm context [--agent codex] [--project PATH] [--max-tokens N]
hm remember --scope global --text "..."
hm note --scope project --project PATH --text "..."
hm search "query" [--scope global,project] [--stores personal,work]
hm render [claude|codex|openclaw|gemini|all]
hm sync --quiet
hm compact [--scope global|project] [--dry-run]
hm stores list
hm doctor
```

Human-optimized commands:

```bash
hm open
hm inbox
hm promote <note-id>
hm edit global/MEMORY.md
hm status
```

## Adapter Model

Adapters render canonical memory into the format each agent expects.

Benefit: each agent gets native-feeling context files, but the canonical store
stays vendor-neutral. Adding a new agent should mean writing a renderer, not
changing how memory is stored.

V1 renderers write generated include files only. They do not overwrite canonical
agent config files such as `CLAUDE.md` or `AGENTS.md` unless a future
adapter-specific migration command is added.

### Claude

- Render a generated include file such as `~/.claude/hive-memory.generated.md`.
- Optionally render project memories for Claude project directories.
- Claude hooks call generic `hm`, not Claude-only sync code.

### Codex

- Render a generated include file such as `~/.codex/hive-memory.generated.md`.
- Use existing Codex lifecycle hooks to refresh context and write notes.

### OpenClaw

- Render/symlink OpenClaw workspace files from selected profiles:
  `AGENTS.md`, `SOUL.md`, `USER.md`, `TOOLS.md`, `MEMORY.md`, `memory/`.
- Must respect channel privacy. Do not expose all personal memory to group/chat
  contexts automatically.

### Gemini / future agents

- Add adapters without changing canonical memory.

## Dotfiles Integration

Dotfiles should own installation and bootstrap, not memory content.

Benefit: dotfiles make `hm` appear consistently on every machine, while the
memory store remains configurable and private. This keeps bootstrap reproducible
without committing personal memory into dotfiles.

Tracked in dotfiles:

```text
~/.config/hive-memory/config.toml.template
~/.config/dot/merge-hooks.d/60-hive-memory.sh
~/.local/bin/agent-hook-session-start-{claude,codex,gemini}
~/.local/bin/agent-hook-stop-{claude,codex,gemini}
```

Untracked / machine-local:

```text
~/.config/hive-memory/config.local.toml
```

Merge hook behavior:

1. ensure `hm` CLI is installed or available
2. materialize config from template + local overrides
3. run `hm doctor --quick`
4. run `hm render --configured --quiet`

## Backend Flexibility

Initial backend: `filesystem`.

Why filesystem first: it matches GDrive/Dropbox/Syncthing/local-directory use
cases, keeps the canonical store inspectable, and avoids requiring a server. The
backend boundary still leaves room for future storage implementations if a real
need appears.

Filesystem backend requirements:

- create directories
- write temp file
- rename temp to final
- list files recursively
- read small text files

This works for:

- Google Drive mount
- Dropbox
- iCloud Drive
- Syncthing
- NFS/SMB
- local directory
- repo checkout

Future backends can implement the same operations:

- `s3`
- `webdav`
- `postgres`
- `sqlite-bundle`

But the CLI and adapters should not know which backend is underneath.

## Edge Cases

These cases are part of the core design, not afterthoughts. The system should be
safe under concurrent agents, flaky cloud sync, offline laptops, and accidental
misconfiguration.

### Multiple sessions on same host write simultaneously

Safe because each write uses a unique file name and atomic rename. Shared files
are not modified in hot path.

### Multiple hosts write simultaneously

Safe for the same reason. Cloud sync may deliver files later, but does not need
to merge shared hot files.

### Same generated ID somehow collides

If final path exists, generate a new random suffix and retry. Log a warning.

### Cloud provider creates conflict copies

`hm doctor` detects names like:

- `conflicted copy`
- `Conflict`
- `sync-conflict`
- duplicate temp files older than TTL

It reports and can quarantine them.

### Curated memory edited while compactor runs

Compactor reads current file hash before edit. Before writing, it re-reads and
verifies hash. If changed, abort or rebase.

### Agent crashes mid-write

Temp file remains. `doctor` removes/quarantines stale `.tmp.*` files older than
TTL.

### Agent crashes holding compaction lock

Lock has `expires_at`. Later compactor may recover after TTL if process is gone
or host is unreachable and timeout passed.

### Backend unavailable

CLI writes to local outbox in `state_dir/outbox/`, then flushes when the active
store root returns. This is optional but important for laptop/offline ergonomics.
`hm sync` means flush/reconcile local hive-memory state; filesystem/cloud-drive
backends still rely on their own sync engine for cross-machine transport.

### Wrong-store writes

If a session is in a context configured for a non-default store, hooks should pass
`--store <name>` explicitly. `hm context` output should include the active store
name so agents can notice if they are about to write to the wrong hive.

### Cross-store leakage

Adapters must not render all stores by default. Store inclusion is explicit in
config, and `doctor` should warn when a sensitive store is included in a broad
adapter target.

### Privacy leak risk

Every note has scope metadata. Adapter render commands require explicit scope
selection. Default should be conservative.

## MVP Cut Line

The v1 MVP should be intentionally useful but not magical. It should prove the
core contract: durable plain-file memory, safe multi-agent writes, configurable
stores, conservative rendering, and a CLI that agents can depend on.

In scope for v1:

- filesystem backend
- TOML config with CLI/env/local/main/default precedence
- one required default store plus optional named stores
- append-only Markdown notes
- optional JSON sidecar/event files for structured processing
- simple text search over canonical files
- context rendering for Claude and Codex first
- doctor diagnostics for config, roots, temp files, conflicts, and permissions
- local outbox plus `hm sync` flush for offline writes
- GitHub Actions release binaries for supported platforms
- shdeps-friendly install snippet/artifacts

Explicitly out of scope for v1:

- vector/semantic search as a required dependency
- non-filesystem backends
- automatic model-driven compaction without review
- cross-store writes in a single command
- background daemon/service
- encrypted-at-rest store format
- Git as a required backend or write path
- full transcript ingestion as default behavior
- every possible agent adapter

Why this cut line: the risky parts are filesystem safety, store selection,
rendering boundaries, and install ergonomics. Those should be solved before
adding smarter search, compaction, or remote backends.

## V1 Specification

The normative v1 implementation specification lives in [SPEC.md](SPEC.md). It
covers the required/deferred feature matrix, config schema, store manifest,
Markdown note schema, JSON event schema, local state/outbox layout, command
contracts, security/privacy model, release plan, and implementation issue order.

Keep this plan focused on goals, rationale, and sequencing. If PLAN.md and
SPEC.md conflict, prefer SPEC.md for v1 behavior and update both deliberately.

## MVP Plan

The MVP should validate the hardest architectural bets first: configurable
stores, safe concurrent writes, readable canonical memory, and useful rendered
context for the two primary coding agents. Smarter compaction and richer search
can build on that foundation.

1. Build `hive-memory` as a standalone repo.
2. Implement filesystem backend with configurable named stores and a required
   default store.
3. Implement collision-safe `note`, `remember`, `search`, `context`, `stores`,
   and `doctor`.
4. Implement Claude and Codex render adapters first.
5. Wire through dotfiles hooks.
6. Add local outbox and `hm sync` flush behavior.
7. Add release artifacts and shdeps install support for v1 release.
8. Defer `hm import claude-memory`, compaction proposals, and OpenClaw adapter until core v1 is stable.

## Settled Decisions

- Repo name: `hive-memory`.
- Primary binary: `hm`.
- Implementation language: Rust.
- Config format: TOML.
- Canonical human memory format: Markdown.
- Structured machine event format: JSON where useful.
- Local indexes/caches are rebuildable and not canonical.
- Multiple named stores are supported, with one required default store.

## Deferred Decisions

- Whether `hive-memory` should be a symlink to `hm`, a wrapper, or omitted after
  v1. `hm` remains the primary documented command either way.
- Whether v1 releases should include musl Linux binaries in addition to glibc
  binaries.
- Exact local search/index implementation after the simple text-search MVP.
- Default compaction policy: manual approval only, trusted-agent proposals, or
  trusted-agent automatic compaction with review logs.
