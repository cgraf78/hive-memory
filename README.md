# hive-memory

[![CI](https://github.com/cgraf78/hive-memory/actions/workflows/ci.yml/badge.svg)](https://github.com/cgraf78/hive-memory/actions/workflows/ci.yml)
[![Release](https://github.com/cgraf78/hive-memory/actions/workflows/release.yml/badge.svg)](https://github.com/cgraf78/hive-memory/actions/workflows/release.yml)

Hive Memory is a small command-line memory layer for AI agents. It gives agents
one durable place to remember useful facts, project context, preferences, and
follow-up notes without tying that memory to one vendor, model, editor, or chat
session.

The basic idea is simple:

1. You create one or more memory stores on disk.
2. Agents write memories with `hm remember` or lower-confidence notes with
   `hm note`.
3. Future sessions call `hm context` or `hm search` to recover the right memory
   for the current agent, project, and scope.

The canonical data is plain files: Markdown notes with TOML front matter, JSON
event sidecars, and curated Markdown files. Indexes, caches, hook state, and
generated context are rebuildable. A human can browse the store without running
the CLI.

## Status

The primary binary is `hm`. The crate is currently pre-1.0, and `Cargo.toml` is
the release-version source of truth. The implemented command surface follows
the v1 schema and behavior described in [SPEC.md](SPEC.md); broader design
rationale lives in [PLAN.md](PLAN.md).

Because the project is still `0.x`, storage schemas may change between releases.
If a change affects public command behavior, file formats, or hook contracts,
update [SPEC.md](SPEC.md) in the same commit.

## Quick Start

Install the CLI from a checkout:

```sh
cargo install --path .
```

Create a minimal config at `~/.config/hive-memory/config.toml`:

```toml
default_store = "personal"

[stores.personal]
root = "${HOME}/hive-memory/personal"
description = "Personal memory"
```

Initialize the store root:

```sh
hm stores init personal --root ~/hive-memory/personal --description "Personal memory"
```

Write a durable memory:

```sh
hm remember --text "Prefer small, focused patches with tests." --scope global
```

Search for it later:

```sh
hm search "focused patches"
```

Ask Hive Memory for agent-readable context:

```sh
hm context --max-tokens 1200
```

For project-specific memory, pass a project path. `hm` derives a stable project
id from an explicit id, environment, `.hive-memory/project.toml`, the git remote,
or finally the local path.

```sh
hm remember \
  --project ~/git/hive-memory \
  --scope project \
  --text "This repo keeps the v1 behavior contract in SPEC.md."

hm context --project ~/git/hive-memory
```

## Mental Model

Hive Memory separates four things that are easy to accidentally mix together.

**Stores** are durable memory roots. A store can live in a local directory, a
synced folder, a network mount, or any normal filesystem path. Each store has a
`manifest.toml` with a stable UUID identity, so the folder can move or be
renamed without changing what store it is.

**Records** are append-only inbox entries written by agents or humans. `hm
remember` writes durable remembered facts. `hm note` writes lower-confidence raw
notes that can be searched or promoted later.

**Curated memory** is human-readable Markdown under directories such as
`memories/global`, `memories/projects`, `people`, and `rules`. It is the place
for reviewed, stable knowledge. `hm promote` can turn a raw inbox note into
curated memory.

**Context** is a generated view for agents. It is assembled on demand from
curated files and remembered records, filtered by store, agent, project, scope,
source, audience, and token budget. Context output is not the source of truth.

## Store Layout

`hm stores init` creates this v1 skeleton:

```text
<store-root>/
  manifest.toml
  people/
  rules/
  memories/
    global/
    agents/
    projects/
  inbox/
    notes/
    events/
  generated/
    .gitignore
```

Canonical note files live under:

```text
inbox/notes/YYYY/MM/DD/<note-id>.md
```

JSON event sidecars live under:

```text
inbox/events/YYYY/MM/DD/<note-id>.json
```

The `generated/` directory is disposable by default. Store-local `.gitignore`
keeps generated artifacts out of version control unless a user intentionally
force-adds something.

## Configuration

The default config path is:

```text
~/.config/hive-memory/config.toml
```

An optional machine-local override can live beside it:

```text
~/.config/hive-memory/config.local.toml
```

`--config <path>` overrides the default path. `HIVE_MEMORY_CONFIG` is used when
`--config` is not supplied. The local override file is always named
`config.local.toml` next to the selected main config.

A fuller config looks like this:

```toml
schema_version = 1

default_store = "personal"
data_dir = "${XDG_DATA_HOME:-${HOME}/.local/share}/hive-memory"
state_dir = "${XDG_STATE_HOME:-${HOME}/.local/state}/hive-memory"
cache_dir = "${XDG_CACHE_HOME:-${HOME}/.cache}/hive-memory"
host_id = "auto"
user_id = "default"

[stores.personal]
root = "${HOME}/hive-memory/personal"
description = "Personal memory"
sensitivity = "private"

[stores.work]
root = "${HOME}/hive-memory/work"
description = "Work memory"
sensitivity = "private"

[storage]
kind = "filesystem"
case_sensitive = "auto"
atomic_rename = "auto"
fsync = "best-effort"

[defaults]
write_scope = "global"
search_scopes = ["global", "project"]
context_sources = ["curated", "remembered"]
event_sidecar = "always"
hook_context_max_tokens = 4000
context_cache_max_age = "7d"

[agents.codex]
default_store = "personal"
read_stores = ["personal"]
write_stores = ["personal"]
allow_all_stores = false

[privacy]
allow_all_stores_flag = true
secret_refuses_cloud_roots = true
allow_secret_writes = false
allow_hook_secret_writes = false

[offline]
enabled = true
mode = "auto"
archive_retention_days = 30

[performance]
context_warm_p95_ms = 200
context_cold_p95_ms = 500
context_store_size_target = 5000
```

Supported store sensitivity values are `public`, `internal`, `private`, and
`secret`. A `secret` store is a policy class, not encryption. By default, secret
stores are refused under common cloud-sync roots.

## Commands

All commands accept these global options:

```text
--config <CONFIG>      main config file to load
--store <STORE>        active store alias for one-store commands
--as-agent <AGENT>     agent identity for store-affinity policy
```

Most read/write commands also support `--json` for machine-readable output.

### Store Commands

Create, inspect, and diagnose store roots:

```sh
hm stores init personal --root ~/hive-memory/personal
hm stores list
hm stores show personal
hm stores doctor
hm stores migrate --dry-run
```

`hm stores init` writes a store manifest and canonical directories. `hm stores
doctor` checks manifest availability, schema support, alias drift, and required
layout for configured stores.

### Writing Memory

Use `remember` for durable facts and preferences:

```sh
hm remember --text "The release version source of truth is Cargo.toml."
```

Use `note` for lower-confidence observations or triage material:

```sh
hm note \
  --text "Investigate whether context output should include stale-cache age." \
  --confidence low \
  --tags follow-up,context
```

Useful write flags:

```text
--scope <SCOPE>              global, project, agent-private, or another policy scope
--confidence <LEVEL>         low, medium, or high
--project <PATH>             derive project identity from a path
--project-id <ID>            use an explicit project id
--subject <SUBJECT>          short grouping label
--tags <A,B>                 comma-separated tags
--audience <AGENT,...>       permitted agents for agent-private writes
--audience-writer-only       make the writer the only audience
--source-kind <KIND>         source category, such as session, hook, import
--source-ref <REF>           source locator or opaque reference
--event / --no-event         override `hm note` sidecar behavior
--allow-secret-write         allow secret-looking content only when policy permits it
```

`hm remember` always writes a JSON event sidecar. `hm note` follows
`defaults.event_sidecar`, unless `--event` or `--no-event` overrides it.

### Search and Context

Search is deterministic text search over curated files and the local triage
index:

```sh
hm search "release version" --limit 10
hm search "context cache" --project ~/git/hive-memory
hm search "triage" --include-inbox
```

Context assembles safe-to-inject Markdown for an agent:

```sh
hm context --project ~/git/hive-memory --max-tokens 4000
hm context --scope global,project --source curated,remembered
hm context --include-inbox
hm context --if-changed
```

By default, context includes curated memory and remembered records. Raw `hm
note` entries are excluded unless `--include-inbox`, `--source inbox`, or
`--source all` is used.

Each rendered memory block is labeled with a trust level:

```text
curated     human-reviewed curated Markdown
remembered  explicit durable memory from `hm remember`
raw         lower-confidence inbox note from `hm note`
```

### Project Commands

Project identity lets memory follow a repo or working directory without relying
on the agent process's current directory.

```sh
hm projects resolve ~/git/hive-memory
hm projects bind ~/git/hive-memory --store personal
hm projects show
hm projects list
hm projects unbind ~/git/hive-memory
hm projects alias old-project-id new-project-id
```

Resolution precedence is:

1. `--project-id`
2. `HIVE_MEMORY_PROJECT_ID`
3. `.hive-memory/project.toml`
4. normalized git origin URL
5. local filesystem path

Project bindings are local machine policy stored under `data_dir`. Project
aliases are shared curated metadata stored inside the memory store, so every
machine can understand a repo rename or remote migration.

### Inbox and Curation

Raw notes can be reviewed and promoted:

```sh
hm inbox list
hm inbox stale --days 14
hm inbox show <note-id>
hm promote <note-id> --to memories/global/MEMORY.md
hm promote <note-id> --to memories/projects/<project-id>/MEMORY.md --verbatim
```

By default, promotion converts the note body into a bullet. `--verbatim`
preserves the original body.

### Refresh, Outbox, and Offline Writes

`hm refresh` rebuilds local operational state:

```sh
hm refresh
hm refresh --force
```

When offline fallback is enabled and a selected store is temporarily
unavailable, writes are queued under `data_dir/outbox` instead of being lost.
Flush queued writes when the store is reachable again:

```sh
hm flush
hm flush --bind <outbox-item-id> --store personal
```

Outbox flushing checks the target store's manifest identity before publishing.
If a queued item was created before the store identity was known, it stays
unbound until explicitly bound.

### Hooks

Hook commands are for agent host integrations. They keep shell adapters thin:
the adapter passes the event shape, and `hm` returns context or actions.

```sh
hm hook session-start --project ~/git/hive-memory
hm hook prompt-submit --project ~/git/hive-memory --text "remember this preference"
hm hook tool-complete --project ~/git/hive-memory --status 0
hm hook stop
```

Hook behavior uses session-local state under `state_dir`. It can detect memory
intent in prompts, emit startup context, coalesce refresh work with a local lock,
and remind the agent at session end when a memory request was never satisfied.

### Doctor

Run top-level diagnostics:

```sh
hm doctor
hm doctor --quick
hm doctor --fix
hm doctor --json
```

`hm doctor` checks config, store availability, required layout, generated
gitignore files, sensitive-store permissions, cloud-root policy, project
bindings, agent policies, outbox state, event pairing, agent-private audience,
secret-looking note content, and prompt-risk patterns. `--quick` skips the
heavier note/content checks for hook-safe use. `--fix` performs safe layout
repairs, but does not initialize missing stores or rewrite user memory.

## Trust and Privacy

Hive Memory treats stored memory as data, not instructions. Context output wraps
memory in explicit source and trust-boundary blocks so agents can distinguish
curated memory from raw notes.

Store access is selected through config, project bindings, agent policy, and
explicit CLI flags. An agent can have its own default store plus separate
read/write allowlists:

```toml
[agents.codex]
default_store = "personal"
read_stores = ["personal"]
write_stores = ["personal"]
allow_all_stores = false
```

`agent-private` records require an explicit audience. Secret-looking writes are
refused unless all of these are true:

1. The command targets a `secret` store.
2. `privacy.allow_secret_writes = true`.
3. The write uses `--allow-secret-write`.
4. For hooks, `privacy.allow_hook_secret_writes = true`.

Hive Memory does not encrypt v1 stores at rest. Use filesystem, disk, vault, or
sync-provider encryption when the store contains sensitive data.

## File Formats

V1 uses:

```text
TOML      config, manifests, Markdown front matter, outbox metadata
Markdown  canonical human-readable notes and curated memory
JSON      event sidecars, indexes, hook state, machine output
```

Note front matter is TOML delimited by `+++`:

```markdown
+++
schema_version = 1
type = "note"
entry_kind = "remember"
id = "20260516T154233.184921Z_taylor_12345_codex_a8f31c"
store_id = "018f5f57-bd9b-7d33-9e21-1f44f0c5a013"
store_name = "personal"
created_at = "2026-05-16T15:42:33.184921Z"
agent_id = "codex"
host_id = "taylor"
scope = "global"
confidence = "medium"
+++

Human-readable memory text.
```

See [SPEC.md](SPEC.md) for the complete schema contract.

## Development

Run the local checks before sending a change:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

CI runs these checks through the shared `cgraf78/actions` Rust workflow:
`cargo test` runs on the core portability matrix for push and pull requests,
and on the full matrix for scheduled or manual runs. Formatting, clippy, and
public-doc checks run as the Ubuntu quality gate. If a change affects public
behavior, update [SPEC.md](SPEC.md). If it changes design rationale or
non-goals, update [PLAN.md](PLAN.md).

## Release

`Cargo.toml` is the release-version source of truth. To publish a release, bump
`package.version`, commit the change, then run:

```sh
scripts/release.sh --push
```

The script derives the `vX.Y.Z` tag from `Cargo.toml` and pushes it. GitHub
Actions uses the shared `cgraf78/actions` Rust release workflow to verify that
the tag still matches the Cargo version before creating the release draft,
building target archives, uploading assets, and publishing the release.

Linux release archives use musl targets to avoid requiring a specific distro
glibc version at runtime. Published archive labels use installer-facing platform
names instead of Rust target triples:

```text
hm-vX.Y.Z-linux-x86_64-musl.tar.gz
hm-vX.Y.Z-linux-aarch64-musl.tar.gz
hm-vX.Y.Z-macos-x86_64.tar.gz
hm-vX.Y.Z-macos-aarch64.tar.gz
```

## License

MIT. See [LICENSE](LICENSE).
