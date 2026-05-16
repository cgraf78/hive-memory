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
| search | yes | simple deterministic text search backed by local index |
| local triage index | yes | rebuildable jsonl index in cache_dir; not full FTS |
| context | yes | curated-only by default; raw inbox opt-in |
| Claude render + `--install` | yes | generated file + include marker in `~/.claude/CLAUDE.md` |
| Codex render + `--install` | yes | generated file + include block through `~/.codex/AGENTS.md` |
| local outbox/flush | yes | required for laptop/offline ergonomics; data_dir, not state_dir |
| `hm promote` / `hm inbox` | yes | curation workflow that bridges raw inbox to curated memory |
| trust-boundary rendering | yes | source-labeled blocks; raw notes excluded by default |
| path normalization | yes | NFC + lowercase-on-case-insensitive + forward slashes |
| performance budget | yes | `hm context` p95 ≤ 200ms warm / ≤ 500ms cold on 5k-note store |
| import claude-memory | deferred | useful migration, not core write path |
| compact proposals | deferred | proposal-only after core commands work |
| cross-host curated writes | deferred | v1 curation is single-user per store |
| OpenClaw / Gemini adapters | deferred | after Claude/Codex stabilize |
| at-rest encryption | deferred | v1 documents the threat model honestly |
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
data_dir = "${XDG_DATA_HOME:-${HOME}/.local/share}/hive-memory"
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
context_sources = ["curated"]   # curated|inbox|all; curated-only default per Trust Boundary
render_scopes = ["global", "project"]
event_sidecar = "always" # never|always

[privacy]
default_render_policy = "conservative" # conservative|configured-only
allow_all_stores_flag = true
warn_sensitive_broad_render = true
secret_refuses_cloud_roots = true       # see Security and Privacy Model

[offline]
enabled = true
mode = "auto" # auto|always|never

[performance]
context_warm_p95_ms = 200
context_cold_p95_ms = 500
context_store_size_target = 5000  # notes; budget is calibrated against this size

[adapters.claude]
enabled = true
stores = ["personal"]
scopes = ["global", "project"]
output = "${HOME}/.claude/hive-memory.generated.md"
install_target = "${HOME}/.claude/CLAUDE.md"   # file to install include marker into
install_mode = "include"

[adapters.codex]
enabled = true
stores = ["personal"]
scopes = ["global", "project"]
output = "${HOME}/.codex/hive-memory.generated.md"
install_target = "${HOME}/.codex/AGENTS.md"    # agent-loaded path; symlink or regular file
install_mode = "include"
```

Validation rules:

- `schema_version` defaults to `1` when absent.
- `default_store` is required after all config layers are merged.
- `stores.<name>.root` is required for every configured store.
- `stores.<name>.expected_id` is optional but enables manifest identity checks.
- `stores.<name>.sensitivity = "secret"` MUST NOT use a cloud-synced root path
  when `privacy.secret_refuses_cloud_roots = true` (default). See Security model.
- store names must match `[a-z0-9][a-z0-9_-]*`.
- `${VAR}` and `${VAR:-fallback}` expansion is supported in path-like fields.
- unknown top-level keys should warn in v1, not fail, unless they are dangerous.
- unknown subkeys under known tables should warn so typos are visible.
- enabled adapters require `output`; `install_target` is required when dotfiles
  update or `hm render --install` should make the adapter visible.
- `adapters.<name>.install_mode` must be `include` in v1.
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
  read-only commands. See Schema Migration for the upgrade contract.
- missing manifest can be initialized by `hm stores init <name>` or repaired by
  `hm doctor --fix` only with an explicit target root.

Why a manifest: config says where a store is; the manifest says what the store
is. That distinction matters when folders are moved, synced, renamed, or mounted
on another machine.

### Schema Migration

Hive-memory uses `schema_version = 1` everywhere a schema appears: config,
manifest, Markdown front matter, JSON events, outbox metadata. The contract:

- **Pre-1.0 (`hm` version 0.x)**: schemas may change between any 0.x releases.
  Stores written by an older 0.x may be upgraded in-place on first open, or
  emit a warning and refuse depending on the change. CHANGELOG is the source
  of truth.
- **Post-1.0**: the schema contract listed under Stability Contracts is frozen.
  Subsequent breaking schema bumps require:
  1. A new `hm` minor version with the new schema.
  2. An in-tree migrator module (`migrate::v1_to_v2`, etc.).
  3. A `hm stores migrate [--dry-run] [--store NAME]` subcommand that runs
     the appropriate migrator on each store root.
  4. The migrator MUST be additive-where-possible, must preserve raw inbox
     notes/events verbatim, and must record a `memory.compaction` event with
     `type = "schema.migration"` describing what changed.
  5. `hm doctor` detects schema_version drift between stored data and the
     supported schema and prints the exact `hm stores migrate` invocation.
- v1 ships with NO migrators (only schema 1 exists). The contract is committed
  so future migrations are not retrofitted into a hostile design.

### Markdown Note Schema

Canonical notes live under:

```text
<root>/inbox/notes/YYYY/MM/DD/<note-id>.md
```

V1 front matter uses **TOML** with `+++` delimiters. This unifies the system on
TOML for all structured metadata (config, manifest, front matter) and JSON for
machine event streams; the canonical Markdown body remains pure prose. YAML is
NOT used in v1 — `serde_yaml` has been unmaintained since 2024 and the small
constrained schema fits cleanly into TOML.

Required fields:

```markdown
+++
schema_version = 1
type = "note"
entry_kind = "remember"   # remember|note
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

Optional fields:

```toml
user_id = "chris"
session_id = "abc123"
project_id = "github-com-cgraf78-hive-memory-018f5f57"
subject = "workflow.preference"
tags = ["preference", "workflow"]
source_kind = "session"
source_ref = "abc123"
related_event_id = "20260516T154233.184921Z_taylor_12345_codex_a8f31c"
expires_at = ""              # RFC3339 or empty
audience = ["codex"]         # see "Audience and agent-private" below
```

Note: `project_path` is intentionally NOT a recommended field. Absolute paths
are sensitive (leak machine layout) and non-portable across hosts. Project
identity belongs in `project_id` (see Project Identity below) and is rendered
via canonical-path normalization on demand by `hm doctor`.

Rules:

- The Markdown body is the durable human-readable record.
- Front matter must be parseable enough for `hm search`, `hm context`, and
  future compaction/proposal workflows to filter by store/scope/project/tags/audience.
- Agents should write concise, factual notes; promotion (`hm promote`) is the
  bridge into curated memory files.
- Notes are immutable by convention once written. Corrections should be new
  notes referencing the old ID, unless a human intentionally edits the file.

Why front matter: the human text remains clean Markdown, while metadata stays
machine-readable enough for filtering and auditing.

#### Project Identity

V1 project IDs are stable across hosts, clones, and protocol changes.

Derivation precedence (highest wins):

1. `--project-id <id>` CLI flag.
2. `HIVE_MEMORY_PROJECT_ID` environment variable.
3. `.hive-memory-project` file at the repo root containing a single `id = "..."`
   TOML line. OPTIONAL — not auto-created. Recommended only when the remote
   URL is unstable (forks, mirror moves) or when committing the binding is
   acceptable for the repo's privacy posture.
4. Derived: `sha256(normalize(git_remote_origin_url))[:12]` prefixed with a
   readable slug, e.g. `github-com-cgraf78-hive-memory-018f5f57bd9b`.
5. Fallback: `sha256(canonical_path)[:12]` prefixed with the path basename
   when no git remote exists.

URL normalization:

- strip scheme (`ssh://`, `https://`, `git://`)
- strip auth (`user@`)
- lowercase host
- strip `.git` suffix
- collapse `:` and `/` after host into a single separator
- so `git@github.com:cgraf78/hive-memory.git`, `ssh://git@github.com/cgraf78/hive-memory`,
  and `https://github.com/cgraf78/hive-memory.git` all normalize to
  `github.com/cgraf78/hive-memory`.

Aliases:

- `memories/projects/<id>/aliases.toml` lists prior IDs (e.g., after a repo
  rename). Search/context follow the alias chain so memory survives moves.
- `hm projects alias <old-id> <new-id>` updates this file under the same
  single-user constraint as other curated writes.
- `hm doctor` warns when unclaimed `project_id` values exist that no alias
  chain points to.

#### Audience and agent-private

Notes/events with `scope = "agent-private"` MUST include an `audience` field
listing the permitted reading agents. Valid v1 writes materialize this field
explicitly; `hm remember`/`hm note` refuse agent-private writes without
`--audience` unless `--audience-writer-only` is set, which records
`audience = [agent_id]`.

Render/search/context filtering:

- `scope = "agent-private"` AND no `audience` field → tolerated only for
  legacy/manual notes; readable by an `hm context --as-agent <id>` invocation
  matching the writing `agent_id`.
- `scope = "agent-private"` AND `audience = ["a", "b"]` → readable only when
  the invoking adapter identifies itself as `a` or `b`.
- Adapters MUST pass `--as-agent <id>` (or `HIVE_MEMORY_ADAPTER`) when rendering.
- `hm doctor` warns when agent-private notes lack an explicit `audience`.

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
  "project_id": "github-com-cgraf78-hive-memory-018f5f57",
  "subject": "workflow.preference",
  "tags": ["preference", "workflow"],
  "confidence": "high",
  "audience": [],
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
- `memory.promotion`: a raw inbox note promoted into curated memory
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
- `audience` is an empty list for public notes and a list of permitted agent IDs
  for agent-private notes (see Audience above).
- JSON events may exist without Markdown only for purely operational events such
  as compaction metadata, but memory observations should have Markdown.

#### Note/Event Pairing

When a Markdown note and a JSON event share the same `id`, they form ONE
logical record. The Markdown body is the canonical human-readable form; the
JSON event is the machine-readable view. Implementations MUST:

- collapse paired records into a single hit in search and context output.
- prefer the JSON event's metadata fields for filtering when both files exist.
- consider the pair "complete" only when both files are present after flush.
- treat a Markdown-without-event or event-without-Markdown as a doctor warning
  (paired writes from a single `hm` invocation should produce both).

When to write JSON:

- `hm remember`: write Markdown and JSON event sidecar by default.
- `hm note`: write Markdown always; write JSON when `event_sidecar = "always"`.
- `hm import`: write JSON to preserve import provenance and dedupe IDs.
- future `hm compact`: write JSON metadata describing inputs/outputs of compaction.
- `hm doctor`: may write JSON diagnostic reports only under local state/cache,
  not the canonical store, unless explicitly asked.

Benefit: JSON gives agents a durable event stream that is easy to process without
making Markdown ugly or forcing humans to maintain machine records manually.

### Curated Memory Files

Curated memory lives under `memories/`, `people/`, and `rules/`.

Rules:

- Curated files are human-readable Markdown.
- v1 curated writes are SINGLE-USER per store. Cross-host curated coordination
  (compaction-apply, multi-host `hm promote`) is DEFERRED to v2. See the
  Concurrency note below.
- Compaction updates curated files only under a LOCAL-process lock (fcntl/flock
  on the host's copy). Cloud-sync lock directories with TTL are NOT real
  distributed locks and are not used as such in v1.
- Compaction must preserve raw inbox notes/events unless an explicit retention
  policy says otherwise.
- Curated edits include a short provenance comment or compaction record so
  bad summaries can be traced back.
- Humans may edit curated files directly when needed; `doctor` detects edits
  by hash/mtime and avoids overwriting them during stale compactions.

Concurrency note: v1 does not provide cross-host curated coordination. README
and `hm doctor` MUST surface this: "Do not run `hm promote` or `hm edit` on
two hosts simultaneously against the same store. Use one curation host per
store." This is honest about what file-system locks under cloud sync can and
cannot guarantee.

Why curated files: raw notes are durable evidence, but agents need concise,
high-signal context. Curated memory is the promoted/summarized layer.

### Path Normalization

Cross-platform path handling is explicit, not implicit.

Canonical-form rules used everywhere paths appear in metadata or comparison:

- Unicode normalization: NFC.
- Case: lowercase on case-insensitive filesystems (macOS HFS+/APFS default,
  Windows, exFAT). Detection via `[storage].case_sensitive`. On case-sensitive
  filesystems, preserve case.
- Separator: forward slash `/` only; backslashes are converted on Windows-via-WSL
  reads.
- No trailing slash.
- Symlinks are resolved to their target's canonical form for project ID
  derivation; symlinked store roots are detected and reported by doctor.

Paths in front matter and JSON events MUST be normalized at write time.
Comparison (search, context filtering, alias matching) MUST normalize both
sides. `hm doctor --fix` rewrites non-normalized paths in a safe quarantine
step rather than mutating canonical files.

Absolute paths to user code (e.g. `project_path`) are sensitive metadata and
are excluded from `hm render` output by default. Adapters MUST set the explicit
`include_paths = true` capability if they want absolute paths rendered (none
do in v1).

### Local State, Cache, and Data

V1 separates three directories along XDG conventions:

```text
${XDG_DATA_HOME:-${HOME}/.local/share}/hive-memory/   # durable user data
${XDG_STATE_HOME:-${HOME}/.local/state}/hive-memory/  # ephemeral state
${XDG_CACHE_HOME:-${HOME}/.cache}/hive-memory/        # rebuildable cache
```

Data dir contents (durable, treat as user memory):

```text
outbox/                 # pending offline writes waiting for store root
projects/               # local-only project bindings, when not committed
store-identities.toml    # last-seen manifest identities for offline outbox binding
```

State dir contents (ephemeral, OK to lose):

```text
locks/                  # local process locks (fcntl/flock)
runs/                   # last render/flush/doctor metadata
quarantine/             # safe quarantine for stale temps/conflicts
```

Cache dir contents (rebuildable, safe to delete):

```text
indexes/                # local triage index (see below)
search/                 # search index workspace
renders/                # temporary render assembly
```

Outbox item shape:

```text
data/outbox/<store-alias>/<id>/
  meta.toml
  note.md
  event.json
```

Outbox `meta.toml` records target store alias, expected store ID, final relative
paths, payload hashes, created_at, attempt count, last_error, and `state` which
is one of `pending` (target store known, waiting for root) or `unbound` (target
store identity unknown, manual reconciliation required).

`hm flush` (formerly `hm sync`) is idempotent: if the final path already exists
with the same hash, mark flushed; if it exists with a different hash, refuse and
report a conflict.

Outbox archive: when an outbox item is successfully flushed AND the active
store root is reachable, `hm flush` writes a snapshot copy under
`<store-root>/.outbox-archive/<host-id>/<YYYY-MM-DD>/<id>/` containing the
exact files that were flushed. This is the safety net that survives local
data-dir wipe; doctor cleans archives older than `[offline].archive_retention_days`
(default 30).

Local triage index:

- `cache/indexes/<store-alias>.jsonl` — one line per inbox note containing
  `{id, store_id, scope, project_id, audience, tags, subject, confidence,
  agent_id, created_at, note_path, event_path}`.
- Rebuilt on `hm flush` and lazily on read commands when the inbox directory's
  recursive mtime/inode marker has changed since the last build.
- NOT full-text search — search still reads matched lines from the underlying
  files. The index exists to make `hm context` and filter operations fast.
- Always rebuildable from canonical files. Deletion is harmless.

Rules:

- Data is durable; state and cache are not. Deleting cache must never lose
  memory; deleting state may lose `runs/` history but not memory.
- Deleting data MAY lose un-flushed outbox writes that never made it to a
  store's outbox-archive. Doctor warns aggressively when outbox is non-empty.
- Offline writes require a known store manifest identity. The identity can come
  from `stores.<name>.expected_id` or from the local
  `data/store-identities.toml` cache populated every time `hm` successfully
  reads that store's manifest. If the active store root is unavailable AND no
  manifest identity is known, the write is enqueued in the outbox with
  `state = "unbound"`. Unbound items NEVER auto-flush.
  `hm flush --bind <outbox-id> --store <name>` is the only way to reconcile
  them. There is no `--force` escape hatch: unbound items exist precisely to
  prevent orphaned writes from flushing into the wrong store.

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
- `--include-inbox` opts read commands into raw inbox notes (excluded by default
  under the Trust Boundary model).
- `--as-agent ID` declares the invoking adapter's agent identity for audience
  filtering. Hooks should set `HIVE_MEMORY_ADAPTER` instead.
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

Stable `--json` success field sets. Fields are mandatory unless explicitly noted:

- `hm remember` / `hm note`:
  `{ "id", "store", "store_id", "note_path", "event_path" }`, where
  `event_path` is `null` when no sidecar is written.
- `hm search`:
  `[ { "id", "store", "store_id", "scope", "trust", "audience", "path",
  "title", "snippet", "score", "created_at" } ]`.
- `hm context --json`:
  `{ "stores", "scopes", "sources", "estimated_tokens", "sections" }`, where
  each section contains `{ "id", "store", "scope", "trust", "audience",
  "source_path", "estimated_tokens", "body" }`.
- `hm render --json`:
  `{ "adapter", "output_path", "written", "sha256", "installed", "visible",
  "install_targets", "backup_paths" }`; install fields are empty arrays when the
  adapter render did not run `--install` or `--uninstall`.
- `hm flush --json`:
  `{ "flushed", "skipped", "failed", "unbound", "pending", "items" }`, where
  each item contains `{ "id", "store", "state", "result", "message" }`.
- `hm stores list --json`:
  `[ { "name", "store_id", "root", "available", "default", "sensitivity" } ]`.
- `hm stores show --json`:
  `{ "name", "config", "manifest", "available" }`.
- `hm doctor --json`:
  `{ "ok", "summary", "checks" }`, where `summary` contains
  `{ "errors", "warnings" }` and each check contains
  `{ "id", "severity", "status", "message", "paths" }`.

Success JSON is command-specific and not wrapped in `{ "ok": true }`; errors
always use the error shape above.

Input rules:

- If `--text` is provided, stdin is ignored unless a command explicitly supports
  combining them.
- If `--text` is absent and stdin is not a TTY, read stdin.
- If both text and stdin are absent for write commands, return CLI usage error.
- Comma-list flags trim whitespace and reject empty entries.
- `--force` is narrowly scoped per command and disabled for non-interactive
  hooks unless config explicitly allows it. `--force` MUST NOT bypass manifest
  identity checks on outbox flush (see Local State section).

### `hm remember`

Purpose: capture a durable memory observation.

Examples:

```bash
hm remember --text "Chris prefers TOML for hive-memory config" --tags preference,config
hm --store work remember --scope project --project /repo --text "Release uses cargo-dist"
```

Inputs:

- `--text TEXT` or stdin required.
- optional `--scope`, `--project`, `--subject`, `--tags`, `--confidence`,
  `--audience` (repeatable).
- defaults: active store, configured default write scope `global`, confidence
  `medium`, empty audience.

Writes:

- Markdown note under `inbox/notes/` with TOML front matter.
- JSON event sidecar according to `event_sidecar` policy.

Output:

- human: created note ID and relative path.
- JSON: `{ "id", "store", "store_id", "note_path", "event_path" }`;
  `event_path` is `null` when no sidecar is written.

Errors/refusals:

- refuse empty text.
- refuse `scope = "agent-private"` without `--audience` unless `--audience-writer-only`
  is set (records `audience = [agent_id]`).
- refuse broad/sensitive scope mismatch unless `--force` and config allows it.
- write to outbox when active store is unavailable and offline fallback is enabled.

### `hm note`

Purpose: capture a more freeform note, usually project/session scoped.

Differences from `remember`:

- accepts multiline stdin by default.
- sets `entry_kind = "note"` with less semantic `subject` structure.
- should not imply the content is already a stable preference/fact.

Use `remember` for high-signal memory; use `note` for raw observations or longer
session notes.

### `hm search`

Purpose: find memories in canonical notes/curated files.

Examples:

```bash
hm search "TOML config"
hm search "release" --stores personal,work --scope project
hm search "Chris prefers" --json --include-inbox
```

V1 behavior:

- simple case-insensitive substring search over Markdown bodies and the
  indexed JSON fields (`subject`, `tags`, `body`).
- backed by the local triage index for filtering; matched lines are read from
  canonical files for snippets.
- collapses note/event pairs (same `id`) into a single hit; the Markdown body
  is the source of the snippet.
- default store only unless `--stores` or `--all-stores` is passed.
- default scopes from config; default sources from `[defaults].context_sources`
  (curated unless `--include-inbox`).
- returns path, score/rank, title/snippet, store, scope, audience, and
  timestamp.
- deterministic ordering: substring/exactness score, newer timestamp, then
  lexical path.
- default result limit: 20 unless `--limit` is provided.

Output:

- human: compact list of matches with snippets.
- JSON: array of match objects.

Future: post-v1, the simple substring path can be replaced by SQLite/FTS without
changing output contracts.

### `hm context`

Purpose: assemble concise context for an agent/session.

Examples:

```bash
hm context --as-agent codex --project /repo --max-tokens 4000
hm context --stores personal,work --scope global,project
hm context --include-inbox --as-agent codex
```

Behavior:

- reads selected stores/scopes only.
- includes active store name, sources used, and render policy in a small
  header.
- v1 default sources are CURATED ONLY (rules/, people/, memories/global/,
  memories/projects/<id>/). Raw inbox notes are EXCLUDED unless
  `--include-inbox` is passed (see Trust Boundary).
- each rendered memory is wrapped in an explicit data-boundary block:
  `<memory id=X agent=Y store=Z scope=W trust=raw|curated>...</memory>`.
- prioritizes rules/preferences, project memory, recent high-confidence notes
  (only when --include-inbox), and relevant search results.
- respects `--max-tokens` approximately; the v1 heuristic is `len(utf8_bytes) / 4`
  for budget estimation. Default token-ish budget: 4000.
- refuses `--all-stores` when config disables broad reads.
- audience filter: when `--as-agent <id>` is set, agent-private notes whose
  audience does not include `<id>` are filtered out.
- escape rule: when rendering any memory body, the CLI escapes lines that begin
  with `---`, `+++`, or `<memory`/`</memory` to prevent source content from
  terminating the data-boundary block. Raw inbox content remains excluded unless
  `--include-inbox` is passed.

Output:

- Markdown context block suitable for injecting into agent prompts/files.
- `--json` returns sections with source paths, audience, trust level, and
  estimated tokens.

### `hm render`

Purpose: render canonical memory into adapter-specific files.

Examples:

```bash
hm render codex
hm render claude --store personal
hm render --configured --quiet
hm render --configured --install --quiet
hm render claude --install
hm render claude --uninstall
hm render --upgrade-marker     # re-bless after intentional template changes
```

Behavior:

- without adapter argument, render the active/default configured adapter only
  if an adapter hint is present; otherwise require explicit adapter or
  `--configured`.
- `--configured` renders all enabled adapters from config.
- render uses each adapter's explicit store/scope allowlist.
- writes to temp file then atomic rename.
- `--install` renders first, then links each selected adapter into every
  configured instruction file the agent may load.
- includes a generated-file header as the FIRST line of every rendered file:

  ```text
  <!-- hive-memory:generated v=1 sha256=<sha256-of-body-minus-header> -->
  ```

  The sha256 covers the body bytes after the header line, including the
  terminating newline. `--upgrade-marker` re-blesses a renderer-template
  change by writing a fresh marker without contents-comparison.

Adapter `--install` contract:

1. Compute the install file set for the selected adapters from their
   `install_target` paths. Each `install_target` is the instruction path that the
   adapter's agent loads. Resolve symlinks before de-duping write targets, so
   `~/.codex/AGENTS.md -> ../.claude/CLAUDE.md` and
   `~/.claude/CLAUDE.md` become one resolved install file, while a regular
   `~/.codex/AGENTS.md` remains its own install file. This makes install
   idempotent whether AGENTS.md is a symlink or not.
2. For each resolved install file, read the current contents; if missing, create
   it with mode `0644`.
3. Copy each file to `<path>.hive-memory.bak` (a single rolling backup,
   overwritten on every install) and write `<path>.hive-memory.bak.toml` with
   `backup_sha256`, `installed_sha256` (filled after install),
   `pre_install_mtime`, and the exact marker blocks installed in that file.
4. Insert (or replace contents of) the stable policy marker block in each
   resolved install file. The policy block is shared across adapters, should be
   committed once when the shared instruction file is tracked, and must not
   contain rendered memory bodies:

   ```text
   # BEGIN hive-memory:policy
   ...stable Hive Memory read/write policy...
   # END hive-memory:policy
   ```

5. Insert (or replace contents of) the adapter-specific marker block for every
   adapter whose `install_target` resolves to the install file being written. In
   v1, the marker body is a native include of that adapter's generated output:

   ```text
   # BEGIN hive-memory:<adapter>
   @<adapter-output-include-path>
   # END hive-memory:<adapter>
   ```

   The include path MUST resolve to the selected adapter's configured `output`
   from the resolved instruction file where the marker is installed. Use the
   shortest native path that is unambiguous; when multiple adapters share one
   resolved install file, or when the generated output is not in the same
   directory as the install file, prefer an absolute or `~` path over a basename
   so Claude and Codex cannot accidentally include each other's generated files.

   Append at end of file if the block is absent; replace contents in place
   if the block exists (idempotent). When multiple enabled adapters share one
   resolved install file, `--configured --install` updates all selected adapter
   blocks in one write and preserves unselected adapter blocks.
6. Preserve the existing line-ending convention (LF or CRLF) per install file.
7. Refuse to edit when ANY of the following is true:
   - an `install_target` is a broken symlink
   - a resolved install file is not owned by `$USER`
   - a resolved install file's mode is not user-writable
   - the existing file contains conflicting markers (e.g.,
     `# BEGIN hive-memory:<adapter>` with a mismatched end marker)
8. `--uninstall` reads backup metadata for every install file and removes only
   the selected adapter's marker block. The policy block remains unless
   `--uninstall --all` is requested. If uninstalling every hive-memory marker
   that was installed into a file and the current file sha256 matches
   `installed_sha256`, restore that file's backup exactly. If the current file
   was edited after install, remove only the selected marker blocks in place,
   keep the backup, and report that automatic restore was skipped.

Render-time safety:

- Refuses broad sensitive-store render unless explicitly configured.
- Refuses to write to a target that has a marker header whose recorded
  sha256 does not match the file body (= a human edited the rendered file)
  unless `--force --backup` is used.
- Refuses to overwrite a target file that lacks the marker header at all,
  suggesting `hm render <adapter> --install` first.
- Refuses to overwrite secret-store renders without `--include-secret`.

Adapter visibility requirements:

- Claude v1: `hm render claude --install` MUST install an include marker into
  `~/.claude/CLAUDE.md` (or configured `install_target`) that references the
  generated Claude output. A normal Claude startup must load that target and
  therefore see the generated memory.
- Codex v1: `hm render codex --install` MUST make the configured
  `install_target` path (`~/.codex/AGENTS.md` by convention) contain or resolve
  to a current Codex include marker. If `AGENTS.md` is a symlink to the shared
  Claude file, editing the resolved shared file is sufficient. If it is a
  regular file, install the same idempotent policy and Codex marker directly into
  `AGENTS.md`.
- Rendered output alone is not considered fully installed. An adapter is
  "visible" only when doctor can prove its configured `install_target` contains,
  or resolves to a file containing, a current `hive-memory:<adapter>` marker for
  that adapter.

### `hm flush`

Purpose: flush and reconcile local hive-memory state.

Alias: `hm outbox flush` (preferred in scripts where `hm flush` could collide
with a domain-specific "flush" verb).

Behavior:

- flush local outbox writes to available store roots.
- retry failed temp/atomic writes where safe.
- writes outbox-archive snapshots under `<store-root>/.outbox-archive/...`
  for items that flushed successfully.
- lazily refreshes the local triage index for affected stores.
- does not replace Google Drive/Dropbox/Syncthing/iCloud transport.
- refuses to flush an outbox item if the destination manifest identity
  conflicts with recorded outbox metadata.
- does NOT auto-flush items with `state = "unbound"`. They require
  `hm flush --bind <outbox-id> --store <name>` to reconcile.

Output:

- counts of flushed, skipped, failed, unbound, and pending items.

### `hm promote`

Purpose: promote a raw inbox note into curated memory.

Examples:

```bash
hm promote <note-id>                           # default: curated/global/MEMORY.md
hm promote <note-id> --to memories/global/PREFERENCES.md
hm promote <note-id> --to memories/projects/<id>/MEMORY.md --as-bullet
```

Behavior:

- Appends a curated entry to the specified file, transforming the raw note
  body into a curated bullet by default. `--verbatim` skips transformation.
- Writes a `memory.promotion` event recording source note ID, target file,
  and timestamp.
- Acquires a LOCAL fcntl/flock on the target file. Cross-host curation is
  not supported in v1 (see Curated Memory Files).
- Reads target file hash before append; aborts if hash changes mid-operation.
- Idempotent on the same source note + target file pair: writes only one
  promotion event regardless of how many times invoked.

### `hm inbox`

Purpose: human triage of raw notes.

Subcommands:

```bash
hm inbox list                # default: pending notes not yet promoted
hm inbox list --all          # include promoted
hm inbox stale --days 14     # notes never promoted, older than N days
hm inbox show <note-id>
```

Behavior:

- Reads the local triage index plus `memory.promotion` events to compute
  promoted-vs-pending state.
- Output is intentionally human-first; `--json` returns the structured list.

### `hm stores`

Subcommands:

```bash
hm stores list
hm stores show [name]
hm stores init <name> --root PATH
hm stores doctor [name]
hm stores migrate [--dry-run] [--store NAME]
```

Behavior:

- `list`: show configured stores and root availability.
- `show`: print merged config + manifest summary.
- `init`: create manifest and canonical directories for a store root.
- `doctor`: run store-specific diagnostics.
- `migrate`: run schema migrators when the supported schema is ahead of the
  stored schema. v1 ships with no migrators; the command exists so the
  workflow is real before it is load-bearing.

### `hm projects`

Subcommands:

```bash
hm projects list
hm projects show [id]
hm projects alias <old-id> <new-id>
```

Behavior:

- `alias`: write `memories/projects/<new-id>/aliases.toml` recording `<old-id>`,
  enabling memory continuity across remote renames. Single-user curation per
  the same v1 constraint as `hm promote`.

### `hm doctor`

Purpose: detect unsafe or broken state before hooks rely on memory.

Checks (all surfaces at default verbosity unless noted):

- config parses and validates.
- default store exists.
- store roots are reachable or outbox is enabled.
- manifests are present and schema-compatible; schema drift surfaces the
  exact `hm stores migrate` command.
- required directories exist.
- temp files older than TTL exist.
- cloud conflict files exist (filename patterns: "conflicted copy",
  "Conflict", "sync-conflict", duplicate temp files older than TTL).
- adapter outputs have valid `<!-- hive-memory:generated -->` markers with
  matching sha256 (warns when drifted).
- for adapters with `install_target` configured, verifies the hive-memory marker
  block is present in the file the agent actually loads.
- verifies each marker points at the configured output path.
- each configured `install_target` exists and contains, or resolves to a file
  containing, its adapter's idempotently installed marker block.
- sensitive stores are not broadly rendered.
- local outbox has pending writes; aggressively warns when items are older
  than 7 days; reports unbound items as a separate error class.
- store roots have suspicious permissions, e.g. world-readable private/secret
  stores.
- write targets resolve through symlinks or escape expected roots.
- private/secret stores appear inside git repos without an explicit
  acknowledgement.
- secret-sensitivity store has a cloud-synced root (refused at config load,
  but doctor re-checks symlinks and mount points).
- agent-private notes lack an explicit `audience`.
- project_id values in inbox notes have no corresponding aliases.toml chain
  (unclaimed project memory).
- `fsync` policy + filesystem combination: warns when `fsync = "required"`
  is set on a known FUSE/cloud-drive mount where parent-dir fsync is a no-op.

Modes:

- default: read-only diagnostics.
- `--quick`: config/root checks suitable for hooks.
- `--fix`: safe repairs only, e.g. create missing directories, create generated
  `.gitignore` files, quarantine stale temp files under `state/quarantine/`,
  rewrite non-normalized paths in metadata via quarantine. Never deletes
  canonical notes/events by default.
- `--json`: machine-readable diagnostic report.

### `hm compact`

Purpose: propose or perform promotion of raw notes into curated memory.

Deferred v1 feature. Core v1 does not require `hm compact` to ship, and no hook
or adapter may depend on it. Implementations may reserve the command name with a
"deferred" message, but the stable command contract starts only when the
proposal workflow is implemented after the core write/search/render path works.
V1 ships `hm promote` for manual single-note promotion instead.

Future behavior:

- selects candidate notes/events by store/scope/project/age/tags.
- writes proposal under `compactions/YYYY/MM/`.
- single-user constraint applies to any apply-mode the future implements.

### `hm import claude-memory`

Purpose: import existing Claude memory-sync content without making Claude the
canonical architecture. This is deferred until the core write/search/render path
is stable.

Behavior:

- reads configured source path(s).
- writes imported Markdown notes (TOML front matter) and JSON `memory.import`
  events.
- preserves provenance source path and import timestamp.
- dedupes by content hash + source path.
- dry-run default should show planned imports before first destructive-looking
  migration, even though raw import writes are append-only.

## Agent Runtime Workflow

V1 should make memory feel automatic without letting hooks write noisy or
incorrect memories. The split is deliberate:

- `hm` owns storage, search, context assembly, rendering, and flush semantics.
- Dotfiles/agent hooks own lifecycle wiring for Claude, Codex, and later agents.
  In this dotfiles environment, Hive Memory behavior is added to the existing
  `agent-hook-*` scripts and shared hook-helper framework; v1 must not introduce
  a parallel hook system.
- Agents own judgment about what text is durable enough to remember.

### Installed Policy Block

`hm render --configured --install` installs stable adapter markers plus a stable
Hive Memory policy block into the configured shared instruction file. For the
current dotfiles layout this is `~/.claude/CLAUDE.md`; Codex reads the same file
through `~/.codex/AGENTS.md -> ../.claude/CLAUDE.md`.

The policy block is tracked and should change rarely. It MUST instruct agents:

- treat hook-provided `hm context` as durable user/project memory.
- write durable preferences, workflow rules, repo conventions, and repeated
  corrections with `hm remember`.
- write project-specific facts with
  `hm remember --scope project --project "$PWD" --text "..."`.
- use `hm note` only for lower-confidence observations worth later triage.
- never store secrets, credentials, one-off task details, or noisy transcript
  summaries by default.
- prefer not writing when unsure; hooks may remind, but should not force memory
  creation.

### Hook Contract

Agent hooks provide freshness and guardrails. They must be allowed to fail soft:
memory hooks should warn and continue unless a command would cause privacy or
store-identity leakage.

Integration rule:

- Implement the behaviors below as extensions to the existing dotfiles hook entry
  points: `agent-hook-session-start`, `agent-hook-prompt-submit`,
  `agent-hook-post-bash`, `agent-hook-post-edit`, and `agent-hook-stop`.
- Reuse the existing hook state directory and helper APIs for context injection,
  warnings, agent detection, and session-local state.
- Agent-specific files such as `agent-hook-session-start-claude` may remain thin
  extensions, but Hive Memory's common behavior should live in the shared hook
  path so Claude and Codex stay consistent.

Required hook behaviors:

- **SessionStart**: inject fresh context by running `hm context --as-agent <id>
  --project <cwd> --max-tokens <budget>` and passing the Markdown into the
  agent's hook-provided additional context. This is the primary read path; the
  generated include files are a stable fallback and bootstrap surface.
- **UserPromptSubmit**: detect obvious durable-memory intent in the user's prompt
  (for example "remember", "from now on", "always", "never", "note that",
  "I prefer", "don't do X anymore"). When detected, record a session-local
  `memory-pending` marker and inject a reminder that the agent should call
  `hm remember` if the interpreted request is truly durable.
- **PostToolUse**: when a tool command successfully runs `hm remember` or
  `hm note`, clear `memory-pending`, then run `hm flush --quiet` and
  `hm render --configured --quiet` best-effort so future sessions see the new
  memory.
- **Stop**: if `memory-pending` remains, emit a reminder that durable memory may
  have been requested but no `hm remember`/`hm note` ran. Stop hooks MUST NOT
  write new memories automatically.

Hooks MAY run `hm flush --quiet` on Stop as best-effort maintenance. Hooks MUST
NOT blindly summarize sessions, prompts, or transcripts into memory.

### Prompt Intent Heuristic

The prompt-intent detector is intentionally conservative and advisory. It should
prefer false positives that become reminders over false negatives that silently
miss explicit "remember this" requests, but it must not write memory itself.

The pending marker should be session-local state, not canonical memory. It exists
only to catch "the agent forgot to write" within the same session.

## Trust Boundary and Prompt Injection

`hm context` produces Markdown that gets injected into agent prompts. Notes can
be written by ANY agent with `hm` access, so a compromised or confused agent
can write instruction-like content into durable memory and poison every future
agent that reads context. The v1 trust boundary mitigates this:

1. **Source labeling**. Every memory included in `hm context` output is wrapped
   in an explicit data-boundary block:

   ```text
   <memory id="..." agent="..." store="..." scope="..." trust="curated|raw" created="...">
   ...body...
   </memory>
   ```

   The block signals to consuming prompts that the enclosed text is DATA,
   not instructions. Prompts that consume `hm context` output should be
   constructed to honor this boundary (see README guidance for hook authors).

2. **Curated-only by default**. v1 default `[defaults].context_sources` is
   `["curated"]`. Raw inbox notes are NEVER included in context unless the
   caller explicitly passes `--include-inbox` or config sets a different
   default. Curated memory is human-reviewed or has been explicitly promoted
   from inbox via `hm promote`.

3. **Escape rules**. For every rendered body, lines that begin with `---`,
   `+++`, `<memory`, or `</memory` are escaped (prefixed with a zero-width
   space) so source content cannot terminate the data-boundary block or
   impersonate front matter. This applies to curated memory too; promoted
   content may still contain copied raw text.

4. **Doctor patterns**. `hm doctor` flags raw inbox notes whose body exhibits
   instruction-language patterns (regex: `(?i)^(ignore|disregard|system|you must|now do)\b`)
   or length spikes (>5000 chars) so the user can review them before they
   land in curated memory via `hm promote`.

5. **Trusted writers config (deferred)**. Post-v1, `[trust] allowed_writers`
   may restrict which `agent_id` values are allowed to write at all. v1 does
   not enforce this — the design exists so the schema can evolve there.

This is the right-sized v1 answer: full prompt-injection defense is a deep
area, but unreviewed-raw-notes-by-default is the dangerous part, and v1
prevents that.

## Performance Budget

`hm context` runs on every agent session-start hook. Slow startup ruins the
ergonomics that make this project worth shipping.

v1 budget:

- `hm context` p95 ≤ 200ms warm (OS page cache hot, local triage index hot)
  on a 5000-note store.
- `hm context` p95 ≤ 500ms cold on the same store.
- `hm search` p95 ≤ 300ms warm on a 5000-note store with substring filter.
- `hm flush` of a 100-item outbox p95 ≤ 2s on a local filesystem.

How v1 hits the budget:

- the local triage index (cache/indexes/<store>.jsonl) is the hot-path data
  structure. Reading 5000 jsonl lines is microseconds; matching/filtering is
  bounded by the index, not the underlying files.
- snippet text is read only for matched IDs, not for every note.
- curated files are small and cacheable across invocations within a session
  via the `state/runs/` last-run cache.

CI enforcement:

- the integration test suite generates a synthetic 5000-note store under
  `tempfile`, then benchmarks `hm context` and `hm search` 30 times each;
  CI fails when measured p95 exceeds budget.
- `hm doctor` exposes the last-measured latencies and warns when they drift
  above budget on a real user's store.

Out-of-budget scenarios:

- larger stores (>5000 notes) are post-v1; the index design extends but the
  budget does not commit numbers for them.
- cloud-mounted roots add I/O latency that v1 does not promise to absorb;
  doctor warns when the budget regresses on a cloud root.

## Security and Privacy Model

Primary privacy risk: rendering or searching the wrong store/scope in the wrong
context, or accepting attacker-written notes as trusted context. See Trust
Boundary for the second risk; the rest of this section covers the first.

Principles:

- Create private stores with restrictive permissions where the platform supports
  it: directories `0700`, files `0600`.
- Reads across stores are opt-in.
- Renders are configured allowlists, not global dumps.
- Store sensitivity is metadata used for warnings/refusals.
- Agent-private scope is enforced via the `audience` field (see Markdown Note
  Schema). Default rendering excludes agent-private notes whose audience
  does not include the active adapter.
- Group/chat contexts should not receive personal memory unless explicitly
  configured for that surface.
- `doctor` warns about broad render policies and unknown adapter outputs.
- `hm context` prints active store/scope/sources metadata so mistakes are
  visible.
- Absolute paths, host IDs, and session IDs are sensitive; renderers omit
  them by default unless an adapter opts in.

Recommended sensitivity levels:

- `public`: safe to render broadly.
- `internal`: safe within a trusted team/family context.
- `private`: default personal/work private memory.
- `secret`: never rendered automatically; explicit search/read only; refuses
  cloud-synced root paths.

Cloud-sync refusal for `secret`:

- `privacy.secret_refuses_cloud_roots = true` (default) refuses to load any
  config that pairs `sensitivity = "secret"` with a root path that matches
  known cloud-sync prefixes: `~/gdrive`, `~/Google Drive`, `~/Dropbox`,
  `~/iCloud`, `~/Library/Mobile Documents`, `~/SkyDrive`, `~/OneDrive`,
  `~/Sync`, `~/syncthing`.
- The match is configurable via `[privacy].cloud_root_prefixes` for users
  who mount cloud drives elsewhere.
- Setting `privacy.secret_refuses_cloud_roots = false` is an explicit user
  override and produces a startup warning every time `hm` runs.

Encryption note: v1 does NOT provide at-rest encryption. The `secret`
sensitivity level relies on filesystem permissions, exclusion from cloud
roots, and exclusion from adapter renders by default. Anyone with shell
access on the host can still read these stores. Encryption is deferred to
v2; the spec is honest about this rather than implying otherwise.

Refusal cases:

- `--all-stores` with a `secret` store unless `--include-secret` is passed
  and config allows it.
- rendering a `private`/`secret` store to an adapter without explicit allowlist.
- writing to a store whose manifest identity conflicts with config unless
  `--force` is used. `--force` does NOT bypass identity checks on outbox
  flush; unbound outbox items require `hm flush --bind`.
- if config and manifest sensitivity disagree, use the stricter sensitivity
  and emit a doctor warning.
- overwriting a non-generated adapter output file (see `hm render` safety).

Why this matters: memory tools are only helpful if users trust that context
will not bleed across personal/work/client/public boundaries.

## Crash Durability

The write path's `fsync` policy determines what survives a host crash.

`[storage].fsync` values:

- `never`: skip all fsync. Fastest, least safe. Recommended only when the
  store root is on a battery-backed or otherwise crash-safe medium.
- `best-effort` (default): fsync the temp file after write, fsync the parent
  directory after rename, but treat EIO/ENOSPC on the parent-dir fsync as a
  warning rather than a failure. Skips parent-dir fsync entirely on detected
  FUSE/cloud-drive mounts where the syscall is meaningless.
- `required`: fsync the temp file, fsync the parent directory, surface any
  error as a write failure. Recommended for local SSDs where durability is
  expected.

Parent-directory fsync is the difference between "file written" and "file
linked into the directory entry"; without it, a crash after rename but before
the directory entry hits stable storage can leave the file invisible despite
the write reporting success. v1 makes this explicit instead of implicit.

## Stability Contracts

The 1.0 stability promise covers exactly the surfaces listed here. Everything
else may evolve in 1.x with changelog notes.

Frozen at 1.0:

- Config schema (every documented field, including `[stores]`, `[storage]`,
  `[defaults]`, `[privacy]`, `[offline]`, `[performance]`, `[adapters.*]`).
- Manifest schema.
- Markdown front matter schema (TOML, every required and optional field).
- JSON event schema.
- Outbox `meta.toml` schema.
- Exit codes (`0` through `5`).
- `--json` output shape per command (`hm remember`, `hm note`, `hm search`,
  `hm context --json`, `hm render --json`, `hm flush`, `hm stores list/show`,
  `hm doctor --json`).
- The data-boundary block syntax used by `hm context`.
- The adapter install marker block syntax used by `hm render --install`,
  including include-mode semantics and symlinked install targets.
- The generated-file header syntax used by `hm render`.

Free to evolve in 1.x:

- Search ranking algorithm and scoring.
- Context section ordering and selection heuristics.
- Token-estimation heuristic (the byte-divided-by-4 v1 approach is explicitly
  approximate).
- Human-readable text output (only `--json` is stable).
- Warning messages, doctor diagnostic phrasing, error message text.
- Performance budget numbers (the contract is "there is a CI-enforced budget",
  not "the budget is exactly 200ms").

This narrow surface means v1 can ship before every implementation detail is
settled, while still giving downstream consumers (hooks, dotfiles integrations,
future adapters) something they can build against.

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
- `integration`: CLI tests with temp stores on Linux, including the
  performance-budget benchmark suite.
- `cloud-sync-sim`: integration tests using the cloud-sync simulation harness
  (see issue list) to validate atomic-rename safety, conflict detection, and
  eventual-delivery behavior.
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
- `1.0`: see Stability Contracts.
- store `schema_version` changes require migrators or clear read-only behavior
  (see Schema Migration).

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

Dotfiles update integration:

- A normal dotfiles update MUST be sufficient to link enabled Claude and Codex
  adapters into their agent-visible instruction files.
- After installing or updating `hm`, the dotfiles merge hook runs
  `hm doctor --quick`, then `hm render --configured --install --quiet`, then
  `hm doctor --quick` again.
- The second doctor pass is required so dotfiles update fails visibly when an
  enabled adapter rendered successfully but is not visible from its configured
  `install_target`.
- Agent lifecycle hooks may refresh renders opportunistically, but v1 correctness
  does not depend on a hook that runs after the agent has already loaded
  instructions.

## Testing

V1 ships with required test coverage organized by module. Each implementation
issue below carries a "Tests required" sub-list pulled from this catalog. CI
enforces coverage via `cargo-llvm-cov` (target: 80% line coverage on non-CLI
modules, 60% on CLI plumbing).

Test categories per module:

### Config loader

- precedence: CLI > env > local > main > built-in defaults.
- `${VAR}` and `${VAR:-fallback}` expansion in path-like fields.
- store-name regex `[a-z0-9][a-z0-9_-]*`.
- required-field absence emits exit-3 errors.
- unknown top-level keys warn; unknown subkeys warn.
- secret-sensitivity + cloud-sync root path = exit-3 refusal.

### Store init / manifest

- `schema_version > supported` = hard error without `--force`.
- `store.id` vs config alias mismatch = doctor warning, not destructive repair.
- sensitivity conflict between config and manifest = strictest applied,
  doctor warning emitted.
- manifest write atomicity (temp file + rename).

### ID and atomic writer

- temp-then-rename happy path.
- 5-retry collision behavior; ID regeneration after collision.
- mid-write crash leaves only stable files (no `.tmp.*` lingering past TTL).
- `fsync = "best-effort"` vs `"required"` semantics.
- parent-directory fsync on `"required"`; skip on FUSE.
- cross-platform path separators (Linux/macOS/WSL).
- case-sensitivity auto-detect on tempdir creation.

### Markdown note writer

- TOML front matter round-trip (write + parse).
- required-field enforcement.
- paired `note_path` consistency with companion event.
- `audience` field omission for non-agent-private notes.
- agent-private writes require explicit audience or `--audience-writer-only`;
  legacy/manual missing-audience notes warn in doctor.
- normalization at write time (NFC, lowercase on case-insensitive FS).

### JSON event sidecar

- sidecar policy `always` vs `never`.
- event `id` matches Markdown `id` when paired.
- schema_version field always present.
- collapsing: search/context sees one logical record per paired ID.

### Search

- case-insensitive substring match.
- `--stores` / `--scope` filters.
- `--include-inbox` opt-in.
- deterministic ordering (score → newer timestamp → lexical path).
- default limit 20; `--limit N` respected.
- audience filter when `--as-agent` set.

### Context assembler

- v1 default sources = curated only.
- `--include-inbox` opens raw notes; escape rules apply to every rendered body,
  including curated content.
- data-boundary block emitted with required attributes.
- audience filter under `--as-agent`.
- `--max-tokens` byte/4 approximation respected.
- active store name in header.
- `--all-stores` refused when config disables broad reads.
- performance budget: p95 ≤ 200ms warm on synthetic 5000-note store.

### Adapter render framework

- Magic header + checksum write path.
- Refusal on drifted checksum without `--force --backup`.
- Refusal on missing header.
- `--upgrade-marker` re-blesses cleanly.
- `--install` adds idempotent policy and adapter marker blocks; second call is
  no-op.
- `--configured --install` renders and installs every enabled adapter.
- include-mode install writes a native include pointing at the configured output.
- install target checks cover both symlinked and regular files such as
  `~/.codex/AGENTS.md`.
- `--install` refusal on broken symlink, foreign owner, non-writable file.
- `--uninstall` removes selected adapter blocks and restores backup only when all
  installed hive-memory markers are being removed and the target still matches
  install metadata; edited targets remove selected marker blocks in place.
- Sensitive-store render refusal.
- Generated `.gitignore` for `generated/` directory.

### Agent runtime hooks

- Installed policy block appears in the shared instruction file and remains
  stable across repeated installs.
- Hive Memory runtime behavior is implemented in the existing dotfiles
  `agent-hook-*` scripts, not in a parallel hook stack.
- SessionStart injects `hm context` additional context with the active adapter
  identity and project path.
- UserPromptSubmit detects explicit memory-intent prompts and records a
  session-local `memory-pending` marker.
- UserPromptSubmit reminder is advisory and does not write canonical memory.
- PostToolUse clears `memory-pending` after successful `hm remember`/`hm note`.
- PostToolUse runs best-effort `hm flush --quiet` and
  `hm render --configured --quiet` after memory writes.
- Stop emits a reminder when `memory-pending` remains.
- Stop does not write memories automatically.
- Hook failures warn and continue unless a privacy/store-identity refusal occurs.

### Outbox + flush

- Outbox path uses XDG_DATA_HOME.
- Write-to-outbox when active store root absent.
- last-seen store identity cache lets offline writes bind after prior manifest
  reads; never-seen stores become unbound.
- Flush idempotency on same-hash collision (mark flushed).
- Flush refuses on different-hash collision (report conflict).
- Flush respects manifest identity check; no `--force` bypass.
- Unbound items never auto-flush; require `--bind`.
- Outbox-archive snapshot written on successful flush.

### Promote / inbox

- Promote appends curated entry + writes promotion event.
- Local fcntl/flock acquired and released cleanly.
- Hash precondition aborts append on mid-operation change.
- Idempotency on same (note, target) pair.
- `hm inbox stale --days N` correctly filters by age and promoted status.

### Doctor

- All listed checks fire with appropriate severity.
- `--quick` runs only config/root checks.
- `--fix` creates missing dirs, generated `.gitignore`, quarantines stale
  temps; never deletes canonical notes.
- `--json` schema matches Stability Contracts.

### Privacy / exit codes

- Exit 3 on schema failure.
- Exit 4 on privacy refusal (cross-store leakage attempt, secret-in-cloud,
  agent-private audience mismatch).
- Exit 5 on backend unavailable with outbox disabled.
- JSON error shape conforms.

### Cloud-sync simulation harness (cross-cutting, dedicated CI job)

- Eventual delivery: host-A writer's file appears for host-B reader after
  configurable delay.
- Conflict-copy filename detection (`*Conflict*`, `*conflicted copy*`,
  `*sync-conflict*`).
- Atomic-rename safety under a sync-watcher that mutates files concurrently.
- Outbox-archive recovery: simulated data-dir wipe still has the last flush
  available under `<store-root>/.outbox-archive/`.

## Recommended Implementation Issues

When ready, create GitHub issues roughly in this order. Each carries a
"Tests required" sub-list from the Testing catalog.

0. **Binary namespace check**: validate `hm` against Homebrew/Apt/common CLI
   namespaces, and `hm` collisions on Linux/macOS/WSL before cementing CLI
   examples. Tests required: none (manual research issue).
1. **Project skeleton**: Rust crate, clap CLI, CI fmt/clippy/test. Tests
   required: smoke `hm --version`.
2. **License and metadata**: MIT license, README badges, contribution notes.
3. **Config loader**: TOML config, local overrides, env/CLI precedence, path
   expansion, cloud-sync prefix refusal. Tests: Config loader (above).
4. **Store initialization**: manifest schema, directory creation, `hm stores`,
   schema-migration scaffolding (`hm stores migrate` with no migrators).
   Tests: Store init / manifest.
5. **ID and atomic writer**: sortable IDs, temp-write/rename, collision retry,
   parent-dir fsync. Tests: ID and atomic writer.
6. **Markdown note writer**: TOML front matter, `hm remember`, `hm note`,
   path normalization. Tests: Markdown note writer.
7. **JSON event sidecars**: event schema, event policy, pairing rule, import
   dedupe helpers. Tests: JSON event sidecar.
8. **Local triage index**: jsonl per-store, rebuild on flush + lazy mtime
   refresh, cache_dir placement. Tests: shared with Search/Context.
9. **Doctor diagnostics**: full checklist including normalization, cloud
   conflict, marker validation, install-target presence, audience absence,
   secret-on-cloud, fsync-on-FUSE. Tests: Doctor.
10. **Simple search**: substring scan over index + matched-line snippet
    read; pair collapse. Tests: Search.
11. **Context assembler**: scope/store selection, curated-default, data-boundary
    blocks, escape rules, byte/4 token approx, performance budget. Tests:
    Context assembler.
12. **Adapter render framework**: generated-file header + sha256 marker,
    atomic render writes, `--upgrade-marker`, refusal rules. Tests:
    Adapter render framework.
13. **Claude adapter + `--install`**: configured render output, include-mode
    installer with backup/idempotent markers/broken-symlink refusal,
    `--uninstall`.
    Tests: Adapter render framework (install-target paths).
14. **Codex adapter + `--install`**: configured render output, include-mode
    installer into `~/.codex/AGENTS.md` whether it is a symlink or regular file,
    plus hook expectations.
15. **Agent runtime hooks**: installed memory policy block, SessionStart context
    injection, prompt memory-intent reminders, memory-pending debt tracking,
    post-write flush/render, Stop reminders. Tests: Agent runtime hooks.
16. **Local outbox and flush**: data_dir placement, unbound state, `--bind`
    workflow, outbox-archive snapshots. Tests: Outbox + flush.
17. **Promote / inbox**: `hm promote`, `hm inbox`, single-user fcntl lock,
    promotion events. Tests: Promote / inbox.
18. **Trust boundary**: data-boundary block rendering, doctor patterns for
    instruction-language detection and length spikes. Tests: shared with
    Context assembler.
19. **Import Claude memory**: append-only migration with provenance/dedupe.
    Tests: pairing + idempotent re-runs.
20. **Cloud-sync simulation test harness**: synthetic delay/conflict/rename
    test framework used by `cloud-sync-sim` CI job. Tests: Cloud-sync
    simulation harness.
21. **Performance benchmark suite**: CI-enforced p95 budget for `hm context`,
    `hm search`, `hm flush` on synthetic stores. Tests: budget enforcement.
22. **Release automation**: target builds, archives, checksums, smoke install.
23. **Dotfiles integration PR**: shdeps install + hooks + config template.
24. **Compaction proposals**: dry-run/proposal flow, local locks, provenance.

Keep each issue small enough to review independently. The early issues should
avoid model calls entirely; `hm` needs a deterministic foundation before agents
start using it heavily.
