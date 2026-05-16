# Hive Memory V1 Specification

This document is the normative implementation specification for `hive-memory` v1.

For broader design rationale, see [PLAN.md](PLAN.md). If this file and `PLAN.md`
conflict, prefer this file for v1 behavior and update both documents deliberately.

## V1 Required / Deferred Matrix

| Feature | V1 required? | Notes |
| --- | --- | --- |
| filesystem backend | yes | canonical backend |
| config loader | yes | CLI/env/local/main/default precedence |
| stores init/list/show/doctor | yes | minimum store lifecycle |
| remember/note | yes | append-only Markdown notes |
| JSON sidecars | yes | always for `remember`; configurable for `note` |
| search | yes | simple deterministic text search |
| context | yes | conservative scope/store filtering |
| Claude render | yes | generated file only |
| Codex render | yes | generated file only |
| local outbox/sync | yes | required for laptop/offline ergonomics |
| import claude-memory | deferred | useful migration, not core write path |
| compact proposals | deferred | proposal-only after core commands work |
| OpenClaw adapter | deferred | after Claude/Codex stabilize |
| release artifacts/shdeps | required for v1 release | not required for first code milestone |

## V1 Implementation Spec

This document is normative for v1. PLAN.md provides rationale and examples and
must be updated if it conflicts with this implementation spec. When code and docs
disagree, prefer this document for v1 behavior and update the docs deliberately.

### Config Schema

Primary config file: `~/.config/hive-memory/config.toml`.
Local override file: `~/.config/hive-memory/config.local.toml`.

Minimal valid config:

```toml
default_store = "personal"

[stores.personal]
root = "${HOME}/hive-memory/personal"
```

Recommended full v1 shape:

```toml
schema_version = 1

default_store = "personal"
state_dir = "${XDG_STATE_HOME:-${HOME}/.local/state}/hive-memory"
cache_dir = "${XDG_CACHE_HOME:-${HOME}/.cache}/hive-memory"
host_id = "auto"
user_id = "default"

[stores.personal]
root = "${HOME}/gdrive/hive-memory/personal"
expected_id = "018f5f57-bd9b-7d33-9e21-1f44f0c5a013" # optional manifest binding
description = "Personal memory"
sensitivity = "private" # public|internal|private|secret

[stores.work]
root = "${HOME}/gdrive/hive-memory/work"
description = "Work memory"
sensitivity = "private"

[storage]
kind = "filesystem"
case_sensitive = "auto" # auto|true|false
atomic_rename = "auto" # auto|true|false
fsync = "best-effort"   # never|best-effort|required

[defaults]
write_scope = "global"
search_scopes = ["global", "project"]
render_scopes = ["global", "project"]
event_sidecar = "always" # never|always

[privacy]
default_render_policy = "conservative" # conservative|configured-only
allow_all_stores_flag = true
warn_sensitive_broad_render = true

[offline]
enabled = true
mode = "auto" # auto|always|never

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

Validation rules:

- `schema_version` defaults to `1` when absent.
- `default_store` is required after all config layers are merged.
- `stores.<name>.root` is required for every configured store.
- `stores.<name>.expected_id` is optional but enables manifest identity checks.
- store names must match `[a-z0-9][a-z0-9_-]*`.
- `${VAR}` and `${VAR:-fallback}` expansion is supported in path-like fields.
- unknown top-level keys should warn in v1, not fail, unless they are dangerous.
- unknown subkeys under known tables should warn so typos are visible.
- local override config may replace scalar values and merge tables.
- CLI flags and environment variables override merged config, not source files.

Why this shape: humans get one readable TOML file; launchers get deterministic
overrides; adapters get explicit store/scope allowlists; and future schema
migration has an obvious version field.

### Store Manifest Schema

Each store root contains `manifest.toml`.

```toml
schema_version = 1
created_by = "hive-memory"
created_at = "2026-05-16T00:00:00Z"
updated_at = "2026-05-16T00:00:00Z"

[store]
id = "018f5f57-bd9b-7d33-9e21-1f44f0c5a013"
name = "personal"
description = "Personal memory"
sensitivity = "private"

[policies]
append_only_inbox = true
allow_direct_curated_edits = false
retention = { mode = "keep-raw" } # or { mode = "archive-after-days", days = 90 }

[capabilities]
json_events = true
local_outbox = true
compaction = "manual" # manual|propose|auto
```

Validation rules:

- `store.id` is generated once and remains stable even if the display name or
  configured alias changes. The UUID is authoritative; config store keys are
  local aliases.
- `store.name` should match the configured store name; mismatch is a doctor
  warning, not destructive repair.
- `schema_version > supported` is a hard error unless `--force` is used for
  read-only commands.
- missing manifest can be initialized by `hm stores init <name>` or repaired by
  `hm doctor --fix` only with an explicit target root.

Why a manifest: config says where a store is; the manifest says what the store
is. That distinction matters when folders are moved, synced, renamed, or mounted
on another machine.

### Markdown Note Schema

Canonical notes live under:

```text
<root>/inbox/notes/YYYY/MM/DD/<note-id>.md
```

V1 front matter uses YAML-compatible `---` delimiters with a constrained value
set: strings, booleans, RFC3339 timestamps, nulls, and string arrays. TOML front
matter can be considered later, but YAML-style front matter is more broadly
familiar to Markdown tools. The implementation should parse this with
`serde_yaml` or an equivalent constrained parser.

Required fields:

```markdown
---
schema_version: 1
type: note
entry_kind: remember # remember|note
id: 20260516T154233.184921Z_taylor_12345_codex_a8f31c
store_id: 018f5f57-bd9b-7d33-9e21-1f44f0c5a013
store_name: personal
created_at: 2026-05-16T15:42:33.184921Z
agent_id: codex
host_id: taylor
scope: global
confidence: medium
---

Human-readable memory text.
```

Optional fields:

```yaml
user_id: chris
session_id: abc123
project_id: ds
project_path: /path/to/project
subject: workflow.preference
tags: [preference, workflow]
source_kind: session
source_ref: abc123
related_event_id: 20260516T154233.184921Z_taylor_12345_codex_a8f31c
expires_at: null
```

Rules:

- The Markdown body is the durable human-readable record.
- Front matter must be parseable enough for `hm search`, `hm context`, and
  `hm compact` to filter by store/scope/project/tags.
- Agents should write concise, factual notes; compaction can later promote them
  into curated memory files.
- Notes are immutable by convention once written. Corrections should be new notes
  referencing the old ID, unless a human intentionally edits the file.

Why front matter: the human text remains clean Markdown, while metadata stays
machine-readable enough for filtering and auditing.

Project ID derivation: V1 project IDs default to a slug derived from the detected
repository root basename plus a short hash of the canonical path. Users may
override with `--project-id` or project aliases in `memories/projects/*/aliases.toml`.

### JSON Event Schema

Structured events live under:

```text
<root>/inbox/events/YYYY/MM/DD/<event-id>.json
```

JSON event files are for machine operations that benefit from strict structure:
indexing, dedupe, import, audit, compaction selection, and future integrations.
They are not meant to be the primary human reading experience.

V1 event shape:

```json
{
  "schema_version": 1,
  "type": "memory.observation",
  "id": "20260516T154233.184921Z_taylor_12345_codex_a8f31c",
  "store_id": "018f5f57-bd9b-7d33-9e21-1f44f0c5a013",
  "store_name": "personal",
  "created_at": "2026-05-16T15:42:33.184921Z",
  "agent_id": "codex",
  "host_id": "taylor",
  "user_id": "chris",
  "session_id": "abc123",
  "scope": "global",
  "project_id": "ds",
  "subject": "workflow.preference",
  "tags": ["preference", "workflow"],
  "confidence": "high",
  "body": "Chris prefers concise bullet summaries unless deeper detail is warranted.",
  "note_path": "inbox/notes/2026/05/16/20260516T154233.184921Z_taylor_12345_codex_a8f31c.md",
  "source": {
    "kind": "session",
    "ref": "abc123"
  }
}
```

Recommended event `type` values:

- `memory.observation`: a fact/preference/context worth remembering
- `memory.correction`: supersedes or corrects an earlier note/event
- `memory.task`: durable todo or follow-up
- `memory.decision`: explicit decision made by the user/project
- `memory.import`: imported legacy memory entry
- `memory.compaction`: summary/promote operation metadata

Rules:

- `id` is globally unique within practical bounds and matches the basename.
- `created_at` is UTC RFC3339 with subsecond precision when available.
- `note_path` is relative to store root when a Markdown note exists.
- `body` should be enough for indexing and dedupe, but the Markdown note remains
  the canonical human-readable record.
- `confidence` is one of `low`, `medium`, `high`.
- `scope` is one of `global`, `project`, `agent-private`, or a configured custom
  scope. `personal` and `work` should normally be stores, not scopes.
- JSON events may exist without Markdown only for purely operational events such
  as compaction metadata, but memory observations should have Markdown.

When to write JSON:

- `hm remember`: write Markdown and JSON event sidecar by default.
- `hm note`: write Markdown always; write JSON when `event_sidecar = "always"`.
- `hm import`: write JSON to preserve import provenance and dedupe IDs.
- `hm compact`: write JSON metadata describing inputs/outputs of compaction.
- `hm doctor`: may write JSON diagnostic reports only under local state/cache,
  not the canonical store, unless explicitly asked.

Benefit: JSON gives agents a durable event stream that is easy to process without
making Markdown ugly or forcing humans to maintain machine records manually.

### Curated Memory Files

Curated memory lives under `memories/`, `people/`, and `rules/`.

Rules:

- Curated files are human-readable Markdown.
- Compaction updates curated files only under lock.
- Compaction must preserve raw inbox notes/events unless an explicit retention
  policy says otherwise.
- Curated edits should include a short provenance comment or compaction record so
  bad summaries can be traced back.
- Humans may edit curated files directly when needed; `doctor` should detect
  edits by hash/mtime and avoid overwriting them during stale compactions.

Why curated files: raw notes are durable evidence, but agents need concise,
high-signal context. Curated memory is the promoted/summarized layer.

### Local State and Cache

Default state/cache locations:

```text
${XDG_STATE_HOME:-${HOME}/.local/state}/hive-memory/
${XDG_CACHE_HOME:-${HOME}/.cache}/hive-memory/
```

State contents:

```text
outbox/                 # offline writes waiting for store root
locks/                  # local process locks
runs/                   # last render/sync/doctor metadata
indexes/                # rebuildable local index if not treated as cache
quarantine/             # safe quarantine for stale temps/conflicts
```

Outbox item shape:

```text
state/outbox/<store-alias>/<id>/
  meta.toml
  note.md
  event.json
```

Outbox `meta.toml` records target store alias, expected store ID, final relative
paths, payload hashes, created_at, attempt count, and last_error. `hm sync` is
idempotent: if the final path already exists with the same hash, mark flushed; if
it exists with a different hash, refuse and report a conflict.

Cache contents:

```text
search/                 # generated search indexes
renders/                # temporary render assembly
```

Rules:

- State/cache are local to a machine and not canonical.
- Deleting cache must never lose memory.
- Deleting state may lose pending offline outbox writes, so `doctor` should warn
  when outbox is non-empty.
- Offline writes require a known store manifest identity. If the active store root
  is unavailable and the store's expected manifest ID is unknown, write commands
  refuse unless an explicit force flag is used.

## V1 Command Contracts

General CLI behavior:

- `--config PATH` selects config path.
- `--store NAME` selects one active store for write/render commands.
- `--stores a,b` selects multiple explicit store aliases for read/search/context
  commands.
- `--all-stores` is read-only and explicit.
- `--scope a,b` filters scopes for read/search/context commands.
- `--scope SCOPE` on write commands selects exactly one write scope.
- `--include-secret` allows read-only inclusion of `secret` stores only when
  config permits it.
- `--json` prints machine-readable output.
- `--quiet` suppresses non-error human chatter.
- `--dry-run` shows planned writes without changing files.
- exit code `0`: success.
- exit code `1`: operational/user error.
- exit code `2`: invalid CLI usage.
- exit code `3`: config/schema validation failure.
- exit code `4`: privacy/safety refusal.
- exit code `5`: backend unavailable and no outbox fallback.

JSON error shape:

```json
{
  "ok": false,
  "error": { "code": "privacy_refusal", "message": "...", "details": {} }
}
```

Input rules:

- If `--text` is provided, stdin is ignored unless a command explicitly supports
  combining them.
- If `--text` is absent and stdin is not a TTY, read stdin.
- If both text and stdin are absent for write commands, return CLI usage error.
- Comma-list flags trim whitespace and reject empty entries.
- `--force` must be narrowly scoped by command and should be disabled for
  non-interactive hooks unless config explicitly allows it.

### `hm remember`

Purpose: capture a durable memory observation.

Examples:

```bash
hm remember --text "Chris prefers TOML for hive-memory config" --tags preference,config
hm --store work remember --scope project --project /repo --text "Release uses cargo-dist"
```

Inputs:

- `--text TEXT` or stdin required.
- optional `--scope`, `--project`, `--subject`, `--tags`, `--confidence`.
- defaults: active store, configured default write scope `global`, confidence `medium`.

Writes:

- Markdown note under `inbox/notes/`.
- JSON event sidecar according to `event_sidecar` policy.

Output:

- human: created note ID and relative path.
- JSON: `{ "id", "store", "note_path", "event_path" }`; `event_path` is
  `null` when no sidecar is written.

Errors/refusals:

- refuse empty text.
- refuse broad/sensitive scope mismatch unless `--force` and config allows it.
- write to outbox when active store is unavailable and offline fallback is enabled.

### `hm note`

Purpose: capture a more freeform note, usually project/session scoped.

Differences from `remember`:

- accepts multiline stdin by default.
- may set `type: note` with less semantic `subject` structure.
- should not imply the content is already a stable preference/fact.

Use `remember` for high-signal memory; use `note` for raw observations or longer
session notes.

### `hm search`

Purpose: find memories in canonical notes/curated files.

Examples:

```bash
hm search "TOML config"
hm search "release" --stores personal,work --scope project
hm search "Chris prefers" --json
```

V1 behavior:

- simple case-insensitive text search over Markdown and selected JSON fields.
- default store only unless `--stores` or `--all-stores` is passed.
- default scopes from config.
- returns path, score/rank, title/snippet, store, scope, and timestamp when known.
- deterministic ordering: substring/exactness score, newer timestamp, then
  lexical path.
- default result limit: 20 unless `--limit` is provided.

Output:

- human: compact list of matches with snippets.
- JSON: array of match objects.

Future: local SQLite/FTS can replace the implementation without changing output
contracts.

### `hm context`

Purpose: assemble concise context for an agent/session.

Examples:

```bash
hm context --agent codex --project /repo --max-tokens 4000
hm context --stores personal,work --scope global,project
```

Behavior:

- reads selected stores/scopes only.
- includes active store name and render policy in a small header.
- prioritizes rules/preferences, project memory, recent high-confidence notes,
  and relevant search results.
- respects `--max-tokens` approximately; exact tokenizer matching is not required
  in v1. Default token-ish budget: 4000.
- refuses `--all-stores` when config disables broad reads.

Output:

- Markdown context block suitable for injecting into agent prompts/files.
- `--json` can return sections with source paths and estimated tokens.

### `hm render`

Purpose: render canonical memory into adapter-specific files.

Examples:

```bash
hm render codex
hm render claude --store personal
hm render --configured --quiet
```

Behavior:

- without adapter argument, render the active/default configured adapter only if
  an adapter hint is present; otherwise require explicit adapter or
  `--configured`.
- `--configured` renders all enabled adapters from config.
- render uses each adapter's explicit store/scope allowlist.
- writes to temp file then atomic rename.
- includes generated-file header warning humans not to edit rendered output.

Safety:

- refuses broad sensitive-store render unless explicitly configured.
- refuses to overwrite non-generated files unless `--force` and backup are used.
- validates a generated-file marker before replacing existing adapter output.

### `hm sync`

Purpose: flush and reconcile local hive-memory state.

Behavior:

- flush local outbox writes to available store roots.
- retry failed temp/atomic writes where safe.
- optionally refresh local search index if configured.
- does not replace Google Drive/Dropbox/Syncthing/iCloud transport.
- refuses to flush an outbox item if the destination manifest identity conflicts
  with recorded outbox metadata.

Output:

- counts of flushed, skipped, failed, and pending items.

### `hm stores`

Subcommands:

```bash
hm stores list
hm stores show [name]
hm stores init <name> --root PATH
hm stores doctor [name]
```

Behavior:

- `list`: show configured stores and root availability.
- `show`: print merged config + manifest summary.
- `init`: create manifest and canonical directories for a store root.
- `doctor`: run store-specific diagnostics.

### `hm doctor`

Purpose: detect unsafe or broken state before hooks rely on memory.

Checks:

- config parses and validates.
- default store exists.
- store roots are reachable or outbox is enabled.
- manifests are present and schema-compatible.
- required directories exist.
- temp files older than TTL exist.
- cloud conflict files exist.
- adapter outputs are generated-file safe.
- sensitive stores are not broadly rendered.
- local outbox has pending writes.
- store roots have suspicious permissions, e.g. world-readable private/secret
  stores.
- write targets resolve through symlinks or escape expected roots.
- private stores appear inside git repos without an explicit acknowledgement.

Modes:

- default: read-only diagnostics.
- `--quick`: config/root checks suitable for hooks.
- `--fix`: safe repairs only, e.g. create missing directories, create generated
  `.gitignore` files, quarantine stale temp files under `state/quarantine/`.
  Never delete canonical notes/events by default.
- `--json`: machine-readable diagnostic report.

### `hm compact`

Purpose: propose or perform promotion of raw notes into curated memory.

Deferred v1 recommendation: implement `--dry-run`/proposal flow after the core
write/search/render path works. Automatic writes to curated memory can come
later.

Behavior:

- selects candidate notes/events by store/scope/project/age/tags.
- acquires compaction lock.
- reads current curated file hash.
- writes proposal under `compactions/YYYY/MM/`.
- if apply mode exists, rechecks hash before modifying curated files.

### `hm import claude-memory`

Purpose: import existing Claude memory-sync content without making Claude the
canonical architecture. This is deferred until the core write/search/render path
is stable.

Behavior:

- reads configured source path(s).
- writes imported Markdown notes and JSON `memory.import` events.
- preserves provenance source path and import timestamp.
- dedupes by content hash + source path.
- dry-run default should show planned imports before first destructive-looking
  migration, even though raw import writes are append-only.

## Security and Privacy Model

Primary privacy risk: rendering or searching the wrong store/scope in the wrong
context. The system should prevent accidental leakage by default.

Principles:

- Create private stores with restrictive permissions where the platform supports
  it: directories `0700`, files `0600`.
- Reads across stores are opt-in.
- Renders are configured allowlists, not global dumps.
- Store sensitivity is metadata used for warnings/refusals.
- Agent-private scope is excluded from general rendering by default.
- Group/chat contexts should not receive personal memory unless explicitly
  configured for that surface.
- `doctor` warns about broad render policies and unknown adapter outputs.
- `hm context` should print active store/scope metadata so mistakes are visible.
- Absolute paths, host IDs, and session IDs can be sensitive; renderers should
  omit or relativize them unless needed for debugging/provenance.

Recommended sensitivity levels:

- `public`: safe to render broadly.
- `internal`: safe within a trusted team/family context.
- `private`: default personal/work private memory.
- `secret`: never render automatically; explicit search/read only.

Refusal cases:

- `--all-stores` with a `secret` store unless `--include-secret` is passed and
  config allows it.
- rendering a `private`/`secret` store to an adapter without explicit allowlist.
- writing to a store whose manifest identity conflicts with config unless forced.
- if config and manifest sensitivity disagree, use the stricter sensitivity and
  emit a doctor warning.
- overwriting a non-generated adapter output file.

Why this matters: memory tools are only helpful if users trust that context will
not bleed across personal/work/client/public boundaries.

## Release and CI Plan

Use GitHub Actions for tests and releases.

Recommended repository: `cgraf78/hive-memory`.

`hm --version` should print `hm X.Y.Z (git <short-sha>, schema <n>)` when build
metadata is available. Checksums use SHA-256 lines compatible with `sha256sum -c`.
Minimum glibc baseline should be documented before the first Linux release.

Recommended jobs:

- `fmt`: `cargo fmt --check`.
- `clippy`: `cargo clippy --all-targets -- -D warnings`.
- `test`: `cargo test` on Linux/macOS.
- `integration`: CLI tests with temp stores on Linux.
- `release-build`: build target archives for release tags.
- `smoke-install`: download release archive and run `hm --version`, `hm doctor`
  against a temp config.

Initial release target matrix:

```text
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
x86_64-apple-darwin
aarch64-apple-darwin
```

Installer target mapping:

| OS/arch | Target |
| --- | --- |
| Linux x86_64, including WSL | `x86_64-unknown-linux-gnu` |
| Linux aarch64 | `aarch64-unknown-linux-gnu` |
| macOS Intel | `x86_64-apple-darwin` |
| macOS Apple Silicon | `aarch64-apple-darwin` |

Deferred target:

```text
x86_64-unknown-linux-musl
```

Artifact layout:

```text
hm-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
hm-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz
hm-vX.Y.Z-x86_64-apple-darwin.tar.gz
hm-vX.Y.Z-aarch64-apple-darwin.tar.gz
checksums.txt
```

Each archive contains:

```text
hm
README.md
LICENSE
completions/   # optional after CLI stabilizes
```

Versioning:

- `0.x`: schema/CLI can change with changelog notes.
- `1.0`: config schema, note/event schemas, and command output contracts should
  be stable or migratable.
- store `schema_version` changes require migrators or clear read-only behavior.

### shdeps Install Recommendation

Dotfiles should install `hm` from GitHub releases, not build from source by
default. The exact shdeps function shape belongs in dotfiles, but release assets
should make this easy:

```sh
# pseudocode for dotfiles shdeps integration
shdeps_binary_from_github \
  "cgraf78/hive-memory" \
  "hm" \
  "hm-v${version}-${target}.tar.gz"
```

Installer responsibilities:

- detect OS/arch target.
- download matching archive and `checksums.txt`.
- verify checksum before installing.
- install `hm` into the dotfiles-managed bin dir.
- optionally install `hive-memory` alias only after that deferred decision is
  made.

Why this matters: install should be reliable during machine bootstrap, before any
agent-specific hook tries to call `hm`.

## Recommended Implementation Issues

When ready, create GitHub issues roughly in this order:

1. **Project skeleton**: Rust crate, clap CLI, CI fmt/clippy/test.
2. **License and metadata**: MIT license, README badges, contribution notes.
3. **Binary namespace check**: validate `hm` against Homebrew/Apt/common CLI
   namespaces before public release.
4. **Config loader**: TOML config, local overrides, env/CLI precedence, path
   expansion.
5. **Store initialization**: manifest schema, directory creation, `hm stores`.
6. **ID and atomic writer**: sortable IDs, temp-write/rename, collision retry.
7. **Markdown note writer**: `hm remember`, `hm note`, front matter, tests.
8. **JSON event sidecars**: event schema, event policy, import/dedupe helpers.
9. **Doctor diagnostics**: config/root/manifest/temp/conflict checks.
10. **Simple search**: text search over Markdown and selected JSON fields.
11. **Context assembler**: scope/store selection, project context, max-token
    approximation.
12. **Adapter render framework**: generated-file safety, atomic render writes.
13. **Claude adapter**: configured render output and hook expectations.
14. **Codex adapter**: configured render output and hook expectations.
15. **Local outbox and sync**: unavailable root fallback and flush behavior.
16. **Import Claude memory**: append-only migration with provenance/dedupe.
17. **Release automation**: target builds, archives, checksums, smoke install.
18. **Dotfiles integration PR**: shdeps install + hooks + config template.
19. **Compaction proposals**: dry-run/proposal flow, locks, provenance.

Keep each issue small enough to review independently. The early issues should
avoid model calls entirely; `hm` needs a deterministic foundation before agents
start using it heavily.
