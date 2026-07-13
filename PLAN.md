# Hive Memory Design

## Goal

`hive-memory` is generic, vendor-neutral shared memory infrastructure for AI agents.
It provides a durable shared memory substrate that works across agents, hosts,
models, and storage backends, while staying ergonomic for both agents and humans.

The project should avoid assumptions about one person, one agent, Claude Code,
Google Drive, or any specific machine layout. Those belong in launchers, hooks,
or config, not core architecture.

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
  one neutral home. Agent-visible context is assembled on demand, not maintained
  as competing generated files. Multiple stores are allowed for intentional
  segmentation, but each store remains internally single-source.
- **Expose clean interfaces**: agents call `hm context`, `remember`, `note`, and
  `doctor`; they do not reimplement path mapping, locking, indexing, or scope
  filtering.
- **Compose from single-purpose parts**: storage backend, note writer, indexer,
  context assembler, compactor, and hook integrations are separate modules with
  narrow APIs.
- **Consolidate after the second use**: start with Claude/Codex hook use, then
  extract shared helpers before adding more hosts. Avoid premature framework
  abstractions, but do not tolerate three copies of the same logic.
- **Guard at async boundaries**: hooks, delayed compaction, background sync, and
  cloud-drive refresh all revalidate paths, locks, file hashes, and config before
  touching state.
- **Prevent re-entrancy in polled loops**: refresh and compaction use local run
  locks so overlapping hooks skip or coalesce instead of stacking work.
- **Isolate by separation, not by crippling**: use scoped context assembly and
  per-agent policy instead of weakening the canonical store or stripping useful
  capabilities from every consumer.
- **Storage is configurable**: the backend memory root is a config value, not a
  hard-coded path. It may live in Google Drive, Dropbox, Syncthing, a network
  mount, a plain local directory, or a repo checkout.
- **Plain files are the source of truth**: canonical memory is Markdown plus
  small TOML/JSON metadata files in a normal directory tree. Indexes and
  generated views are rebuildable.
- **Append-only writes first**: agents write new immutable remembered/note files
  rather than editing shared hot files. Curated memory is updated by explicit
  compaction.
- **Hook integrations are edges**: Claude, Codex, OpenClaw, Gemini, etc. consume
  context/actions from `hm`. No agent owns the canonical memory format.
- **Small sharp CLI**: agents should not reimplement filesystem rules. They call
  `hm` for reads, writes, locking, and diagnostics.
- **Human-legible by default**: a human can browse the memory root and understand
  it without running the CLI.
- **Generated means disposable**: search indexes, caches, and lock state are
  local or rebuildable unless explicitly marked canonical.
- **Scope and privacy are first-class**: personal/work/project/agent-private scopes
  are metadata, not naming conventions only.
- **Minimal required human maintenance**: agents should handle routine capture,
  flush, indexing, and compaction proposals. Humans should review or steer
  important memory changes, not babysit the system daily.

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

Release artifacts use the same generated release identity format as `shdeps`:

```text
hm-YYYYMMDD-HHMMSS-<8hex>-linux-x86_64-musl.tar.gz
hm-YYYYMMDD-HHMMSS-<8hex>-linux-aarch64-musl.tar.gz
hm-YYYYMMDD-HHMMSS-<8hex>-android-aarch64.tar.gz
hm-YYYYMMDD-HHMMSS-<8hex>-macos-x86_64.tar.gz
hm-YYYYMMDD-HHMMSS-<8hex>-macos-aarch64.tar.gz
```

WSL should use the Linux binaries. Linux release artifacts use musl targets to
avoid requiring the install host to provide the same or newer glibc as the build
runner. Archive names use installer-facing platform labels instead of raw Rust
target triples.
Termux and other aarch64 Android environments use the Android/Bionic artifact.

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
context assembly, and compactor as clean modules under one binary crate at
first. Split crates only after a second real consumer needs it.

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
context_sources = ["curated", "remembered"]
event_sidecar = "always" # never|always
hook_context_max_tokens = 4000
context_cache_max_age = "7d"

[agents.codex]
default_store = "personal"
read_stores = ["personal"]
write_stores = ["personal"]
allow_all_stores = false

[agents.claude]
default_store = "personal"
read_stores = ["personal"]
write_stores = ["personal"]
allow_all_stores = false

[privacy]
allow_all_stores_flag = true
secret_refuses_cloud_roots = true
allow_secret_writes = false
allow_hook_secret_writes = false
```

Important: store roots are always configurable. Google Drive is just one good
backend for Chris, not a baked-in assumption. A config always has one
`default_store`; additional named stores are optional and are used for memory
segmentation.

Why this matters: config files capture durable human intent, while environment
variables let launchers and hook adapters pass current agent/session/project
facts without editing files. `hm` interprets those facts and owns the policy
decisions, so agents and hooks do not guess.

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
readable. Launchers and hook adapters pass facts through env/flags; policy stays
inside `hm`.

### Environment Variables

Core env vars:

```bash
HIVE_MEMORY_CONFIG=/path/to/config.toml       # config file path
HIVE_MEMORY_ROOT=/path/to/root                # shorthand root override for active/default store
HIVE_MEMORY_STORE=personal                    # active store if --store omitted
HIVE_MEMORY_STORES=personal,work              # read-store default if --stores omitted
HIVE_MEMORY_STORE_PERSONAL_ROOT=/path/to/root # per-store root override
HIVE_MEMORY_STATE_DIR=/path/to/state
HIVE_MEMORY_CACHE_DIR=/path/to/cache
HIVE_MEMORY_HOST_ID=taylor
HIVE_MEMORY_USER_ID=chris
HIVE_MEMORY_AGENT_ID=codex                    # default --as-agent for hooks
HIVE_MEMORY_SESSION_ID=<session-id>
HIVE_MEMORY_PROJECT=/path/to/project-or-file  # default --project hint for hooks
HIVE_MEMORY_SCOPE=global
HIVE_MEMORY_HOOK_ACTIVE=1                     # hook-safe defaults/recursion guard
```

Behavior toggles:

```bash
HIVE_MEMORY_OFFLINE=1                         # write to local outbox only
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
HIVE_MEMORY_AGENT_ID=codex hm stores list     # shows codex-readable/writable stores
hm stores list
hm stores doctor
```

Rules:

- Every store has its own root, manifest, inbox, memories, and generated views.
  Local locks live in the user's state directory and are keyed by store identity;
  the shared store root does not provide distributed locks.
- Config store keys and CLI store names are local aliases. Prefer lowercase
  `[a-z0-9][a-z0-9_-]*`. The manifest `store.id` UUID is the durable store
  identity; `store.name` is the preferred human-readable name and should usually
  match the configured alias.
- The global default store is used for humans when no `--store` is provided.
  Agent/hook commands use `[agents.<id>].default_store` when
  `HIVE_MEMORY_AGENT_ID` is set.
- A local project-to-store binding can make a repo use the right memory store
  without hardcoding path rules into hooks. Hook adapters pass the best available
  path hint to `hm hook`; `hm` resolves CLI/env/project binding/agent default in
  the documented order.
- Agents declare `read_stores` and `write_stores`. Missing agent entries inherit
  a conservative default-store-only policy so single-store setups stay simple.
- Adapters declare which stores they render. Adapter render stores must be within
  the same-name agent's read policy when that agent policy exists.
- Cross-store search/context is opt-in via `--all-stores` or explicit
  `--stores a,b`. For agents, both forms are constrained by `read_stores`; the
  singular `--store` selects the active write/render store and is constrained by
  `write_stores` for writes.
- Notes/events record their `store_id` in front matter/JSON metadata.
- Curated-write lock keys are per-store unless a future cross-store operation is
  explicitly requested; the locks themselves remain local process locks.

Avoid accidental leakage: rendering a work store into a personal agent config,
letting an agent search a work store by default, or writing personal memory into
a work/client store must require explicit config.

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
    # only explicit shared generated artifacts live here; indexes and hook
    # runtime state stay local and rebuildable
```

Why this layout: curated memory, raw inbox entries, generated artifacts, and
local indexes have different lifecycles. `entry_kind = "remember"` distinguishes
remembered agent/user facts from lower-confidence notes inside the inbox. Keeping
lifecycles separate makes it obvious what humans edit, what agents append, what
compaction produces, and what can be rebuilt or deleted safely.

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

### Rule 3: curated writes use local process locks only

Curated files require coordination, but cloud-drive lock directories are not
reliable distributed locks. V1 therefore treats curated writes (`hm promote`,
`hm edit`, future compaction apply) as SINGLE-USER per store. The process takes a
local `fcntl`/`flock` lock in `state_dir`, reads the target hash before editing,
and re-checks the hash before writing. Cross-host curated coordination is a
post-v1 feature.

### Rule 4: local indexes are never shared locks

Search indexes live in `cache_dir`, not the shared root. Multiple
agents on the same host can coordinate with normal local file locks. Indexes can
always be rebuilt from canonical files.

## Write Types

Writes separate human-readable content from machine-oriented metadata. The goal
is not to duplicate everything forever; it is to preserve a durable note while
providing enough structure for indexing, compaction, and diagnostics.

### Raw note

Human-readable markdown with TOML front matter (delimited by `+++`). TOML
keeps the structured-metadata format consistent with config and manifests
and avoids the unmaintained `serde_yaml` dependency:

```markdown
+++
type = "note"
entry_kind = "remember"   # remember|note
id = "20260516T154233.184921Z_taylor_12345_codex_a8f31c"
created_at = "2026-05-16T15:42:33.184921Z"
agent_id = "codex"
host_id = "taylor"
session_id = "abc123"
scope = "global"
project_id = "github-com-cgraf78-hive-memory-018f5f57"
tags = ["preference", "workflow"]
confidence = "high"
audience = []             # non-empty for agent-private notes
+++

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
  "audience": [],
  "body": "...",
  "source": {
    "kind": "session",
    "session_id": "..."
  }
}
```

The CLI writes the Markdown note as the canonical record. It may also write a
JSON sidecar/event from the same operation when structured processing needs it.
When both files share an `id`, they form ONE logical record; search/context
collapse the pair into a single hit.

## CLI Surface

The CLI is the stable contract for agents, hook adapters, and humans. Adjacent
surfaces pass facts and display returned actions; they do not reimplement path
rules, store selection, locking, prompt heuristics, or metadata formatting.

Intended use by caller:

- **Agent tool commands** write and query memory directly. The launcher should
  provide `HIVE_MEMORY_AGENT_ID`, `HIVE_MEMORY_SESSION_ID`, and a best available
  `HIVE_MEMORY_PROJECT` path hint. Agents normally call:

  ```bash
  hm remember --scope global --text "..."
  hm remember --scope project --text "..."
  hm note --scope project --text "..."
  hm search "query" [--scope global,project]
  hm context [--project PATH] [--max-tokens N]
  ```

- **Hook adapters** call exactly one command per lifecycle event and translate
  the returned actions into the host's UI:

  ```bash
  hm hook session-start --project PATH --json
  hm hook prompt-submit --project PATH --text "$PROMPT" --json
  hm hook tool-complete --project PATH --status 0 --json
  hm hook stop --json
  ```

- **Humans** inspect, triage, curate, and debug:

  ```bash
  hm open
  hm inbox [list|stale|show]
  hm promote <note-id> [--to <curated-file>]
  hm projects resolve [PATH|--project PATH]
  hm projects bind PATH --store NAME
  hm projects unbind PATH
  hm projects alias <old-id> <new-id>
  hm stores list
  hm doctor
  hm status
  ```

- **Install/update automation** verifies the local setup. Agent-facing context is
  delivered by rules/hooks, so update automation should not maintain generated
  context files:

  ```bash
  hm doctor --quick
  ```

Low-level maintenance/debug commands remain available, but they are not the
normal hook workflow:

```bash
hm context --if-changed
hm refresh --quiet
hm flush [--quiet] [--bind <id> --store <name>]
hm stores migrate [--dry-run]
```

`hm sync` is renamed to `hm flush` to avoid confusion with cloud-drive sync.
`hm outbox flush` is an alias.

## Agent Integration Model

Agent hosts integrate with Hive Memory through static instructions plus lifecycle
hooks that call `hm`. This keeps `hm` vendor-neutral and avoids a parallel
generated-file system that can drift from the canonical store.

Dotfiles or another host integration owns agent config files such as `CLAUDE.md`
or `AGENTS.md`. Those files should teach the agent to use `hm`; the generic
binary should not edit host-specific instruction files.

### Claude and Codex

- Static top-level guidance tells the agent when to call `hm`.
- Session/prompt/tool/stop hooks call `hm hook ... --json` and inject returned
  context/actions.
- Agents call the same generic `hm remember`, `hm note`, `hm search`, and
  `hm context` commands humans can inspect.

### Runtime hooks

Agent hooks make Hive Memory feel automatic without making memory writes
magical:

- Add Hive Memory behavior to the existing dotfiles `agent-hook-*` scripts and
  shared hook helpers. Do not create a parallel Hive Memory hook stack.
- The agent launcher/session bootstrap exposes `HIVE_MEMORY_AGENT_ID`,
  `HIVE_MEMORY_SESSION_ID`, and a best-available `HIVE_MEMORY_PROJECT` hint to
  normal agent tool subprocesses so `hm remember --scope project --text "..."`
  works without the agent reconstructing context. It should not set
  `HIVE_MEMORY_PROJECT_ID` for general long-lived sessions because that pins the
  project and defeats path-hint based project switching.
- `hm remember`/`hm note` refuse likely secret material by default before writing
  canonical memory or durable outbox data.
- Hooks call `hm hook <event>` with the best available active file, buffer, tool
  working path, or launch path, then translate returned actions into the agent
  host's context/warning/reminder surface. They do not resolve project roots,
  project IDs, store bindings, cache paths, refresh locks, prompt intent, or
  memory-pending state themselves.
- `HIVE_MEMORY_HOOK_ACTIVE=1` is not exported into the long-lived agent process;
  it is only for low-level hook-launched maintenance/context commands.
- SessionStart injects the context action returned by `hm hook session-start`.
- Prompt/tool-boundary hooks call `hm hook prompt-submit` or
  `hm hook tool-complete` with the best active path hint. `hm hook` emits context
  only when the resolved project/store selection changed, so long-lived agents
  can move across repos without hooks doing their own project tracking.
- `hm hook prompt-submit` detects explicit memory intent and records
  session-local memory debt plus an advisory reminder.
- `hm remember`/`hm note` append session write receipts when
  `HIVE_MEMORY_SESSION_ID` is set.
- PostToolUse calls `hm hook tool-complete` after tool events. Refresh is a cheap
  no-op when no unrefreshed write receipts exist, so hooks do not need a perfect
  shell-command classifier to detect every memory write spelling.
- `hm hook tool-complete` clears memory debt when consumed write receipts prove a
  memory write occurred.
- `hm hook stop` reminds when memory debt remains, but never writes memory
  itself.
- Hook entry points skip Hive Memory behavior when already running under
  `HIVE_MEMORY_HOOK_ACTIVE=1`, and `hm refresh` coalesces overlapping hook
  refreshes for the same session.

The hooks are guardrails around agent judgment. Static dotfiles-owned guidance
tells the agent when to write; hooks catch obvious misses and keep the
store/render state fresh.

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
4. rely on dotfiles-managed instruction files and hooks for automatic agent
   access

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

### Curated memory edited while curator runs

V1 curated writes (`hm promote`, `hm edit`, future `hm compact --apply`) are
SINGLE-USER per store. The curator acquires a LOCAL fcntl/flock on the target
file, reads the current hash before edit, and re-checks the hash before
writing. Cross-host curated coordination is deferred to v2; cloud-drive lock
directories with TTL are NOT real distributed locks and are not used as such.
README and `hm doctor` warn: "Do not run `hm promote` on two hosts
simultaneously against the same store."

### Agent crashes mid-write

Temp file remains. `doctor` removes/quarantines stale `.tmp.*` files older than
TTL.

### Backend unavailable

CLI writes to local outbox in `data_dir/outbox/` (XDG_DATA_HOME, not
XDG_STATE_HOME — pending memory is durable user data, not ephemeral state).
`hm` also keeps a local last-seen store identity cache in `data_dir` so a laptop
can bind offline writes to a store it has previously opened even when the store
root is temporarily unavailable.
For reads, `hm context` keeps an ephemeral last-success cache in `state_dir`.
Agent hooks get that fallback automatically in hook mode, but only when the
selected backend is unavailable, the cache is within max age, and current agent
store policy still allows every cached store. Stale context is labeled clearly
in the injected header.
On successful flush, `hm flush` also writes a snapshot to
`<store-root>/.outbox-archive/<host-id>/<date>/` as a safety net that survives
local data-dir wipe. The filesystem/cloud-drive backend still relies on its
own sync engine for cross-machine transport; `hm flush` only handles
hive-memory's own outbox.

### Wrong-store writes

If a session is configured for a non-default store, launchers or hook adapters
may pass `HIVE_MEMORY_STORE=<name>`, or agents may pass `--store <name>`
explicitly. Agent store affinity still constrains the resolved store: writes outside
`write_stores` and reads outside `read_stores` fail with a privacy refusal
instead of silently falling back. `hm context` output includes the active store
name so agents can notice when a write is targeting the wrong store. Offline
writes whose target store manifest identity is unknown are enqueued with
`state = "unbound"` and NEVER auto-flush; they require explicit reconciliation
via `hm flush --bind <outbox-id> --store <name>`.
There is no `--force` escape hatch for unbound items: that's the point of the
unbound state.

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
- append-only Markdown notes with TOML front matter
- JSON sidecar/event files for structured processing (paired with Markdown by ID)
- simple text search over canonical files, backed by a local triage index
- context rendering for Claude and Codex, with dotfiles update keeping static
  guidance and hooks installed
- lifecycle hook workflow through `hm hook <event>` that injects fresh read
  context, tracks explicit memory intent, and refreshes after memory writes
- trust-boundary rendering: source-labeled blocks, escaped memory bodies,
  remembered facts visible by default, raw notes excluded by default
- `hm promote` + `hm inbox` curation workflow (single-user per store)
- doctor diagnostics for config, roots, temp files, conflicts, permissions,
  trust-boundary patterns, audience presence, secret-on-cloud refusal
- local outbox in XDG_DATA_HOME plus `hm flush` for offline writes, with
  per-store `.outbox-archive/` snapshot on flush
- performance budget (`hm context` p95 ≤ 200ms warm / ≤ 500ms cold on a
  5000-note store) enforced by CI integration tests
- cloud-sync simulation test harness as a dedicated CI job
- explicit schema-migration contract (`hm stores migrate` scaffolded; no
  migrators ship in v1)
- stable 1.0 contract surface (config/manifest/front-matter/event schemas,
  exit codes, `--json` shapes, marker syntax)
- GitHub Actions release binaries for supported platforms
- shdeps-friendly install snippet/artifacts

Explicitly out of scope for v1:

- vector/semantic search as a required dependency
- non-filesystem backends
- automatic model-driven compaction without review
- cross-host curated writes (compaction-apply, multi-host `hm promote`)
- cross-store writes in a single command
- background daemon/service
- encrypted-at-rest store format
- Git as a required backend or write path
- full transcript ingestion as default behavior
- every possible agent adapter
- trusted-writer enforcement (`[trust] allowed_writers` is post-v1)

Why this cut line: the risky parts are filesystem safety, store selection,
trust-boundary rendering, hook ergonomics, and the 1.0 stability surface. Those
should be solved before adding smarter search, compaction, or remote backends.

## V1 Specification

The normative v1 implementation specification lives in [SPEC.md](SPEC.md). It
covers the required/deferred feature matrix, config schema, store manifest,
Markdown note schema, JSON event schema, local state/outbox layout, command
contracts, security/privacy model, release plan, and implementation issue order.

Keep this plan focused on goals, rationale, and sequencing. If PLAN.md and
SPEC.md conflict, prefer SPEC.md for v1 behavior and update both deliberately.

## MVP Plan

The MVP validates the hardest architectural bets first: configurable stores,
safe concurrent writes, readable canonical memory, trust-boundary rendering,
and useful rendered context for the two primary coding agents. The detailed
issue order lives in SPEC.md; the broad sequencing here is:

1. Validate the `hm` binary-name namespace before cementing CLI examples.
2. Build `hive-memory` as a standalone Rust crate with CI scaffolding.
3. Implement config loader (with cloud-root secret refusal) and store
   initialization (manifest schema + `hm stores migrate` scaffold).
4. Implement collision-safe atomic writer, Markdown note writer (TOML front
   matter), JSON event sidecar with pairing.
5. Implement the local triage index in `cache/indexes/` and doctor diagnostics.
6. Implement `hm search` and `hm context` (curated+remembered defaults,
   data-boundary blocks, performance budget).
7. Implement agent runtime hook integration inside the existing dotfiles
   `agent-hook-*` scripts through `hm hook <event>`: SessionStart context
   injection, prompt memory-intent reminders, session write receipts,
   receipt-aware refresh, and Stop reminders.
8. Implement local outbox under XDG_DATA_HOME, `hm flush` with unbound-state
   handling, `hm promote`, and `hm inbox`.
9. Add trust-boundary doctor patterns, cloud-sync simulation harness, and
   performance benchmark suite to CI.
10. Wire dotfiles hooks; add release artifacts and shdeps install support.
11. Defer `hm import claude-memory`, compaction proposals, OpenClaw adapter,
    cross-host curated writes, and at-rest encryption until core v1 stabilizes.

## Settled Decisions

- Repo name: `hive-memory`.
- Primary binary: `hm`.
- Implementation language: Rust.
- Config format: TOML.
- Canonical human memory format: Markdown with TOML front matter (`+++` delimiters).
- Structured machine event format: JSON where useful.
- Note/event pairs (same `id`) collapse into one logical record.
- Local indexes/caches are rebuildable and not canonical; they live in
  `cache_dir` not `state_dir`.
- Outbox is durable user data: lives under `data_dir` (XDG_DATA_HOME) with a
  per-store `.outbox-archive/` snapshot for crash recovery.
- Multiple named stores are supported, with one required default store.
- Agents have explicit store affinity: default store, readable stores, writable
  stores, and `allow_all_stores`. Named-store requests outside that policy are
  privacy refusals.
- `hm sync` is renamed to `hm flush` (`hm outbox flush` alias). Flush is local;
  cloud-drive transport is the user's sync engine.
- Project identity derives from a normalized git remote URL hash with optional
  `.hive-memory-project` override and `aliases.toml` chain for rename survival.
- v1 curated writes are SINGLE-USER per store. Cross-host curated coordination
  is deferred.
- `hm context` default sources are `["curated", "remembered"]`; raw `hm note`
  inbox entries are opt-in (`--include-inbox`) under the trust-boundary model.
- Agent-private scope is enforced via an explicit `audience` field. Valid v1
  writes materialize the field; `--audience-writer-only` records the writer as
  the only audience.
- Runtime hooks provide the seamless path: SessionStart reads fresh context,
  `hm hook` refreshes context when long-lived sessions move across projects,
  prompt-submit records explicit memory intent, `hm remember`/`hm note` write
  session receipts, `hm hook tool-complete` owns receipt-aware flush/index
  maintenance, and Stop reminds without writing automatically.
- Hooks run simple `hm hook <event>` commands; `hm` owns project binding lookup,
  store affinity, context cache fallback, prompt intent, memory-pending,
  receipt-aware refresh, and coalescing.
- `secret`-sensitivity stores refuse cloud-synced root paths by default.
- Write-time secret detection backs up the agent policy: likely credentials are
  refused before canonical note/outbox writes unless an explicit secret-store
  write mode is configured.
- v1 ships a CI-enforced performance budget for `hm context`/`hm search`/`hm flush`.
- The 1.0 stability surface is explicitly scoped in SPEC.md "Stability Contracts"
  (schemas, exit codes, `--json` shapes, marker syntax); search ranking, context
  ordering, token heuristic, and human-text output are free to evolve in 1.x.

## Deferred Decisions

- Whether `hive-memory` should be a symlink to `hm`, a wrapper, or omitted after
  v1. `hm` remains the primary documented command either way.
- Whether v1 releases should include musl Linux binaries in addition to glibc
  binaries.
- Exact local FTS implementation when post-v1 demand exceeds the v1 triage
  index. SQLite/FTS remains the leading candidate.
- Default compaction policy: manual approval only, trusted-agent proposals, or
  trusted-agent automatic compaction with review logs.
- `[trust] allowed_writers` enforcement (restrict which `agent_id` values may
  write at all) — schema-prepared but not enforced in v1.
- Whether at-rest encryption is added in v2 for `secret`-sensitivity stores, or
  whether `secret` remains permissions+exclusion-based indefinitely.

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
|--------|---------|-----|------|--------|----------|
| CEO Review | `/plan-ceo-review` | Scope & strategy | 0 | — | — |
| Eng Review | `/plan-eng-review` | Architecture & tests (required) | 1 | CLEAR (PLAN) | 24 issues raised + resolved (8 in primary review, 16 from codex outside-voice) |
| Design Review | `/plan-design-review` | UI/UX gaps | 0 | — | — |
| Outside Voice | `/codex review` | Independent 2nd opinion | 1 | issues_found | 16 codex findings: 2 P0, 9 P1, 4 P2, 1 P3 — all addressed in this update |
| DX Review | `/plan-devex-review` | Developer experience gaps | 0 | — | — |

- **CODEX:** 16 substantive findings, all 16 addressed in spec updates. 2 cross-model tensions (perf budget D8 vs index-required-for-budget; project ID D2 vs URL stability) resolved by tightening D11 (lightweight local triage index) and D12 (URL normalization + opt-in override + alias chain).
- **CROSS-MODEL:** strong overlap on scope-and-safety boundaries. Primary review caught integration wiring + naming + format choices; codex caught threat-model + durability + audience-enforcement gaps. Net: no contradictions remain.
- **UNRESOLVED:** 0. Every finding has a recorded decision and a spec edit.
- **VERDICT:** ENG CLEARED — ready to begin issue filing per the updated SPEC.md "Recommended Implementation Issues" list. The 24 decisions are captured in SPEC.md (normative) and reflected in PLAN.md "Settled Decisions" (rationale). Implementation tasks artifact: `~/.gstack/projects/cgraf78-hive-memory/tasks-eng-review-*.jsonl`.
