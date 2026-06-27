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
| agent store affinity | yes | per-agent default/read/write store policy |
| remember/note | yes | append-only Markdown notes |
| JSON sidecars | yes | always for `remember`; configurable for `note` |
| search | yes | simple deterministic text search backed by local index |
| local triage index | yes | rebuildable jsonl index in cache_dir; not full FTS |
| context | yes | curated + remembered by default; raw inbox opt-in |
| local outbox/flush | yes | required for laptop/offline ergonomics; data_dir, not state_dir |
| `hm promote` / `hm inbox` | yes | curation workflow that bridges raw inbox to curated memory |
| trust-boundary rendering | yes | source-labeled blocks; raw notes excluded by default |
| path normalization | yes | NFC + lowercase-on-case-insensitive + forward slashes |
| performance budget | yes | `hm context` p95 ≤ 200ms warm / ≤ 500ms cold on 5k-note store |
| import claude-memory | deferred | useful migration, not core write path |
| compact proposals | deferred | proposal-only after core commands work |
| cross-host curated writes | deferred | v1 curation is single-user per store |
| OpenClaw / Gemini hooks | deferred | after Claude/Codex stabilize |
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
search_sources = ["curated", "remembered"] # curated|remembered|inbox|all
context_sources = ["curated", "remembered"] # curated|remembered|inbox|all
event_sidecar = "always" # never|always
hook_context_max_tokens = 4000
context_cache_max_age = "7d"

[agents.codex]
default_store = "personal"
read_stores = ["personal"]  # stores usable by search/context/hooks
write_stores = ["personal"] # stores usable by remember/note
allow_all_stores = false

[agents.claude]
default_store = "personal"
read_stores = ["personal"]
write_stores = ["personal"]
allow_all_stores = false

[privacy]
allow_all_stores_flag = true
secret_refuses_cloud_roots = true       # see Security and Privacy Model
allow_secret_writes = false             # requires secret store + --allow-secret-write
allow_hook_secret_writes = false        # non-interactive hooks stay safer by default

[offline]
enabled = true
mode = "auto" # auto|always|never

[performance]
context_warm_p95_ms = 200
context_cold_p95_ms = 500
context_store_size_target = 5000  # notes; budget is calibrated against this size

[classifier]
mode = "off"           # auto|on|off
batch_limit = 25
min_interval = "6h"
timeout_seconds = 60
apply_confidence = "high" # high|medium

```

Validation rules:

- `schema_version` defaults to `1` when absent.
- `default_store` is required after all config layers are merged.
- `stores.<name>.root` is required for every configured store.
- `stores.<name>.expected_id` is optional but enables manifest identity checks.
- `stores.<name>.sensitivity = "secret"` MUST NOT use a cloud-synced root path
  when `privacy.secret_refuses_cloud_roots = true` (default). See Security model.
- `privacy.allow_hook_secret_writes = true` requires
  `privacy.allow_secret_writes = true`; otherwise config validation fails. Hook
  secret writes are an extra opt-in on top of secret-store targeting and
  `--allow-secret-write`.
- store names must match `[a-z0-9][a-z0-9_-]*`.
- `${VAR}` and `${VAR:-fallback}` expansion is supported in path-like fields.
- unknown top-level keys should warn in v1, not fail, unless they are dangerous.
- unknown subkeys under known tables should warn so typos are visible.
- `agents.<id>.default_store`, `read_stores`, and `write_stores` must reference
  configured store aliases. Missing agent entries resolve to the global
  `default_store` with `read_stores = [default_store]` and
  `write_stores = [default_store]`, which preserves simple single-store installs.
- local override config may replace scalar values and merge tables.
- CLI flags and environment variables override merged config, not source files.
- `[classifier]` defaults to `mode = "off"`. Invalid `mode`, `backend`,
  `apply_confidence`, non-positive limits/timeouts, invalid `min_interval`, or
  `backend = "command"` with an empty `command` are config errors.
- In `mode = "auto"`, backend auto-detection only considers known backend labels
  that also exist under `[agents]`; `mode = "on"` or explicit `backend = ...`
  opts into the selected adapter. When multiple allowed backends are installed,
  auto-detection prefers `codex`, then `claude`, then `gemini`. Secret stores
  are never sent to classifier backends, and audience-restricted
  (`agent-private`) records are never part of the classifier queue: their bodies
  are visible only to the listed agents, not to whichever backend CLI wins
  detection.

Why this shape: humans get one readable TOML file; launchers get deterministic
overrides; agents get explicit store affinity; and future schema migration has
an obvious version field.

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

The rebuildable cache under `cache/indexes` is deliberately OUTSIDE this frozen
store/record schema. It carries its own private versions — an
`INDEX_FINGERPRINT_SCHEMA_VERSION` for the freshness fingerprint and an index
header format version for the embedded header line — that may be bumped at any
time without a store migration. Bumping either simply marks existing cache files
stale so they are rebuilt from the canonical notes/events on the next read; no
data is migrated because the cache holds no canonical data. Accordingly, the
schema reported by `hm --version` is the store/record `schema_version`, not the
cache's internal versions, which are an implementation detail of the local
triage index and never appear in the stability contract.

The `schema <n>` in `hm --version`'s `hm X.Y.Z (git <short-sha>, schema <n>)`
line is specifically the manifest/store `schema_version` — the single number the
stability contract freezes. The per-artifact schema versions (config, Markdown
front matter, JSON event, outbox metadata), which all share the value `1` in v1
but could diverge under a future migration, are reported per store by
`hm doctor` and `hm stores show`, not collapsed into the one-line `--version`
banner.

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
supersedes = ["20260515T101112.000000Z_taylor_12000_codex_b1c2d3"] # record ids this entry replaces
expires_at = ""              # RFC3339 or empty
kind = "preference"          # preference|project-fact|incident|reference
audience = ["codex"]         # see "Audience and agent-private" below

[classified]
source = "llm"                # llm|manual
backend = "claude"            # optional; diagnostics only
at = "2026-06-12T00:00:00Z"  # RFC3339
verdict_version = 1
confidence = "high"          # optional; high|medium|low for llm verdicts
```

Note: `project_path` is intentionally NOT a recommended field. Absolute paths
are sensitive (leak machine layout) and non-portable across hosts. Project
identity belongs in `project_id` (see Project Identity below) and is rendered
via canonical-path normalization on demand by `hm doctor`.

Rules:

- The Markdown body is the durable human-readable record.
- Front matter must be parseable enough for `hm search`, `hm context`, and
  future compaction/proposal workflows to filter by store/scope/project/tags/audience.
- `kind` is a durable memory-kind verdict that drives relevance-mode injection.
  `classified` records who issued that verdict. Missing `classified` means the
  record is eligible for background LLM review; `source = "manual"` means an
  explicit human retag and MUST NOT be overridden by the LLM worker.
- LLM verdicts carry `verdict_version`; bumping the classifier prompt/policy
  version re-queues older LLM verdicts, while manual verdicts are version-exempt.
- Agents should write concise, factual notes; promotion (`hm promote`) is the
  bridge into curated memory files.
- Notes are immutable by convention once written. Corrections should be new
  notes referencing the old ID, unless a human intentionally edits the file.
- `supersedes` is the explicit, authoritative correction link: it lists the
  record ids this entry replaces. Broad recall (`hm search` and `hm context`)
  hides a record that a newer one explicitly supersedes, across scope and entry
  kind. See the Supersession section for the full resolution algorithm,
  invariants, and the lower-confidence natural-language fallback.

Why front matter: the human text remains clean Markdown, while metadata stays
machine-readable enough for filtering and auditing.

#### Project Identity

V1 project IDs are stable across hosts, clones, and protocol changes when
identity comes from an explicit ID, a `.hive-memory-project` marker, or a VCS
remote. The path fallback (no marker, no remote) is keyed off the
`$HOME`-relative path, so it is host-stable only when the project sits at the
same location relative to `$HOME` on each machine; a path outside `$HOME`, or a
different layout across machines, stays host-local and needs a marker or remote
for cross-host identity.

Project path inputs are hints, not identity by themselves. `--project PATH`,
`HIVE_MEMORY_PROJECT`, `hm projects resolve PATH`, and
`hm projects resolve --project PATH` may point at a repo root, subdirectory, or
file. `hm` canonicalizes the hint before deriving identity:

1. If the hint is a file, use its parent directory as the starting point.
2. Walk upward for the nearest `.hive-memory-project`; when found, use that
   directory as the project root and the file's explicit ID as the identity.
3. Otherwise, walk upward for the nearest VCS worktree root (`.git`, `.hg`,
   `.jj`, or `.svn`) and use that root's normalized remote URL when present. The
   root and remote URL are read directly from the on-disk VCS config
   (`.git/config`, `.hg/hgrc`, jj's git backend) rather than by invoking a VCS
   binary, so resolution is fast and does not require git to be installed.
4. Otherwise, use the starting directory as a local path project, keyed by its
   `$HOME`-relative path.

Process CWD is only a last-resort hint when no CLI/env/project-specific path is
available. Launching an agent from a subdirectory should therefore resolve to
the same project as launching from the repo root, and hook adapters should pass
the active file path or tool working path instead of the agent process CWD when
that information is available.

Derivation precedence (highest wins):

1. `--project-id <id>` CLI flag.
2. `HIVE_MEMORY_PROJECT_ID` environment variable.
3. `.hive-memory-project` file at the resolved project root containing a single
   `id = "..."` TOML line. OPTIONAL — not auto-created. Recommended only when
   the remote URL is unstable (forks, mirror moves), when a monorepo subtree is
   the real memory project, or when committing the binding is acceptable for the
   repo's privacy posture.
4. Derived: `sha256(normalize(vcs_remote_url))[:12]` prefixed with a readable
   slug, e.g. `github-com-cgraf78-hive-memory-018f5f57bd9b`. The remote URL
   comes from whichever VCS owns the root (git `origin`, hg/sl `paths.default`,
   or jj's git backend).
5. Fallback: `sha256(home_relative_path)[:12]` prefixed with the directory
   basename when no remote exists. `home_relative_path` is `~/<path-under-HOME>`
   when the root is under `$HOME`, else the absolute path. Keying off the
   `$HOME`-relative path keeps the ID identical across machines whose home dirs
   differ (`/home/u` vs `/Users/u`); absolute paths outside `$HOME` remain
   host-local.

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
- Project ids derived from on-disk alias metadata are untrusted input: a synced
  or tampered store could list ids like `../../../../etc` or an absolute path,
  which would otherwise be joined onto `memories/projects/` and inject arbitrary
  `.md` at the highest (`curated`) trust level. Every alias/project id is
  validated at the resolution boundary and must be a single normal path
  component (no `..`, absolute, rooted, prefix, or separator components);
  unsafe ids are dropped rather than followed, with defense-in-depth re-checks
  at the curated filesystem join.
- `hm projects alias <old-id> <new-id>` updates this file under the same
  single-user constraint as other curated writes.
- `hm doctor` warns when unclaimed `project_id` values exist that no alias
  chain points to.

Local project store binding:

- `hm projects bind PATH --store NAME` writes a local-only binding under
  `data_dir/projects/` from resolved project ID to preferred store alias.
- `hm projects unbind PATH` removes that local binding.
- `hm projects resolve [PATH|--project PATH] --as-agent <id>` prints the
  derived project ID, effective store, and whether the store came from CLI/env,
  local project binding, agent default, or global default.
- Bindings are intentionally local data, not canonical store data. They capture
  "this checkout belongs to the work store on this machine" without committing a
  private work/personal policy into the project repo.
- A project binding never bypasses agent store affinity. If a bound store is not
  in the active agent's read/write policy, the command fails with exit `4`
  instead of silently using the bound store.

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
- Adapters MUST pass `--as-agent <id>` (or set `HIVE_MEMORY_AGENT_ID`) when
  rendering.
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

Absolute paths to user code (e.g. `project_path`) are sensitive metadata.
Context output includes only the path hints needed for project disambiguation
and should keep raw absolute paths out of durable memory unless explicitly
written by the user/agent.

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
runs/                   # last flush/doctor metadata and session receipts
context-cache/           # last successful hook context per agent/project/store
quarantine/             # safe quarantine for stale temps/conflicts
```

Cache dir contents (rebuildable, safe to delete):

```text
indexes/                # local triage index (see below)
search/                 # search index workspace
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
  `{id, store_id, entry_kind, scope, project_id, audience, tags, subject,
  confidence, kind, classified, agent_id, host_id, created_at, body, note_path,
  event_path}`.
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

### Intended Use Model

`hm` is the policy boundary. Adjacent surfaces provide facts and display
results:

- Agent launchers provide `HIVE_MEMORY_AGENT_ID`, `HIVE_MEMORY_SESSION_ID`, and
  the best available `HIVE_MEMORY_PROJECT` path hint to normal tool subprocesses.
- Agents call direct commands such as `hm remember`, `hm note`, `hm search`, and
  `hm context`.
- Hook adapters call `hm hook <event>` once per lifecycle event and translate its
  returned actions into agent-specific context, warning, and reminder surfaces.
- Humans use `hm inbox`, `hm promote`, `hm projects`, `hm stores`, and
  `hm doctor` to inspect and curate.
- Install/update automation installs `hm`, materializes config, installs static
  agent guidance/hook wiring owned by dotfiles, and runs `hm doctor --quick`.
  Agent instruction-file linkage is not a responsibility of the generic `hm`
  binary.

The lower-level commands `hm context --if-changed`, `hm refresh`, and `hm flush`
remain stable for debugging, maintenance, and unusual integrations, but v1
dotfiles hooks should not orchestrate them directly.

### Store Affinity and Resolution

Store selection is a privacy boundary, not just a convenience default. Agent
identity is **self-asserted** via `--as-agent`/`HIVE_MEMORY_AGENT_ID`: any
process can claim any agent id, so per-agent store affinity is defense in depth,
not a cryptographic boundary. The rules below are written so that *dropping*
identity does not silently widen access.

- Memory read/write commands (`remember`, `note`, `search`, `context`,
  `promote`, `inbox`, `classify`, `reconcile`, `projects alias`, hooks) without
  any asserted agent identity fail closed to the **global default store's**
  conservative policy: `read_stores = [default_store]`,
  `write_stores = [default_store]`, `allow_all_stores = false`. A plain human
  shell running `hm remember`/`hm search` with no `--as-agent` therefore keeps
  working against the default store, but a no-identity request for a NON-default
  store via `--store`/`HIVE_MEMORY_STORE` is exit `4` (`privacy_refusal`). This
  closes the bypass where a restricted agent reaches an out-of-allowlist store
  by simply not passing `--as-agent`. Because identity is self-asserted, this is
  a guardrail against accidental/lazy widening, not a hard security boundary.
- Local-affinity commands that only manage machine-private binding metadata
  (`projects bind`, `projects resolve`, `projects show`) keep human any-store
  access: a human may bind/inspect any configured store regardless of the global
  default, since the binding is local data under `data_dir` and agent affinity
  is still re-checked when memory is actually read or written. An asserted agent
  identity is enforced under these commands too, so a binding can never bless a
  store outside the agent's allowlist.
- Agent/hook commands with `HIVE_MEMORY_AGENT_ID` are constrained by
  `[agents.<id>]`. If no matching section exists, v1 creates a conservative
  effective policy with `default_store = <global default_store>`,
  `read_stores = [default_store]`, `write_stores = [default_store]`, and
  `allow_all_stores = false`.
- Write store resolution is: explicit `--store`, then `HIVE_MEMORY_STORE`, then
  local project binding for `--project` / `HIVE_MEMORY_PROJECT`, then
  `agents.<id>.default_store`, then global `default_store`. The resolved write
  store MUST be present in `write_stores` for agent/hook commands.
- Read store resolution is: explicit `--store`, `HIVE_MEMORY_STORE`, local
  project binding for `--project` / `HIVE_MEMORY_PROJECT`, then
  `agents.<id>.default_store`, then global `default_store`. The resolved store
  MUST be within `read_stores` for agent/hook commands. **Multi-store reads
  (`--stores`, `HIVE_MEMORY_STORES`, `--all-stores`) are DEFERRED — see below —
  so v1 read commands resolve exactly one store.** When implemented, the
  resolution order will prepend explicit `--stores` and `HIVE_MEMORY_STORES`,
  and the resolved set MUST be a subset of `read_stores` for agent/hook commands.
- `--all-stores` is DEFERRED (not in the v1 implemented surface). The frozen
  contract, once shipped, is: read-only; for humans it expands to all configured
  stores allowed by privacy flags, and for agents it expands only to
  `read_stores` and is refused unless `agents.<id>.allow_all_stores = true`.
- A named-store request outside the effective policy is exit `4`
  (`privacy_refusal`) rather than silently falling back to another store.

General CLI behavior:

- `--config PATH` selects config path.
- `--store NAME` selects one active store for write commands and for
  read/search/context commands. In v1, read commands operate on this single
  resolved store.
- `--stores a,b` (multiple explicit store aliases for read/search/context) is
  DEFERRED and not accepted in v1.
- `--all-stores` (read-only fan-out across stores) is DEFERRED and not accepted
  in v1.
- `HIVE_MEMORY_STORES=a,b` (multi-store launcher default) is DEFERRED and not
  read in v1; use `HIVE_MEMORY_STORE` for the single read/write store.
- `HIVE_MEMORY_HOOK_ACTIVE=1` tells `hm` the caller is an agent lifecycle hook,
  not an interactive human command. In this mode `hm context` enables safe cache
  fallback, and maintenance commands such as `hm refresh` use hook-safe
  coalescing locks by default. `hm hook <event>` applies these semantics
  internally, so hook adapters do not need to set this variable for the normal
  hook workflow.
- `--scope a,b` filters scopes for read/search/context commands.
- `--scope SCOPE` on write commands selects exactly one write scope.
- `--include-secret` allows read-only inclusion of `secret` stores only when
  config permits it.
- `--allow-secret-write` permits write commands to store text that matches
  secret detectors only when the resolved store has `sensitivity = "secret"` and
  config explicitly enables secret writes. It is refused in hook mode unless
  config allows non-interactive secret writes. This flag never permits writing
  detected secrets into public/internal/private stores.
- `--include-inbox` opts read commands into raw inbox notes (excluded by default
  under the Trust Boundary model).
- `--as-agent ID` declares the invoking agent identity for audience filtering
  and write metadata. Launchers and hook adapters may provide
  `HIVE_MEMORY_AGENT_ID`; `--as-agent` wins when both are present.
- `--project PATH` provides the project path hint for project-scoped commands.
  Launchers and hook adapters should provide the best available active file,
  buffer, tool working path, or launch path via `--project` or
  `HIVE_MEMORY_PROJECT` so agent commands do not have to interpolate `$PWD`
  themselves. CWD is only the fallback when no hint is provided.
- `--json` prints machine-readable output.
- `--quiet` suppresses non-error human chatter.
- `--dry-run` shows planned writes without changing files.
- exit code `0`: success.
- exit code `1`: operational/user error.
- exit code `2`: invalid CLI usage.
- exit code `3`: config/schema validation failure.
- exit code `4`: privacy/safety refusal.
- exit code `5`: backend unavailable and no outbox fallback.

Hook ergonomics rule: lifecycle hooks should need only event facts plus one
simple command. The preferred hook interface is `hm hook <event>`, which owns
context selection, project-switch detection, prompt memory-intent heuristics,
receipt-aware refresh, and memory-pending state. Low-level primitives such as
`hm context`, `hm refresh`, and `hm projects resolve` remain available for human
debugging and direct agent commands, but hook scripts should not orchestrate
them by default. Any design that requires hooks to parse config, inspect store
manifests, compute project IDs, manage cache files, detect prompt intent, or
sequence flush/index refresh manually belongs in `hm` instead.

JSON error shape:

```json
{
  "ok": false,
  "error": { "code": "privacy_refusal", "message": "...", "details": {} }
}
```

Stable `--json` success field sets. Fields are mandatory unless explicitly noted:

- `hm remember` / `hm note`:
  `{ "id", "store", "store_id", "store_source", "scope", "project_id",
  "audience", "note_path", "event_path", "created", "duplicate_of" }`, where
  `project_id` is `null` for non-project writes, `event_path` is `null` when no
  sidecar is written, `created` is `false` when an idempotency/dedupe rule
  returned an existing item, and `duplicate_of` is `null` unless a prior item
  was reused.
- `hm search`:
  `[ { "id", "store", "store_id", "scope", "trust", "audience", "path",
  "title", "snippet", "score", "created_at" } ]`.
- `hm context --json`:
  `{ "agent_id", "project_id", "project_hint", "stores", "store_source",
  "scopes", "sources", "estimated_tokens", "emitted", "stale",
  "cache_created_at", "sections" }`, where `emitted = false` means
  `--if-changed` found no context-selection change and produced no Markdown
  body, `stale` is `true` only for last-success cache fallback,
  `cache_created_at` is `null` for fresh context, and each section contains
  `{ "id", "store", "scope", "trust", "audience", "source_path",
  "estimated_tokens", "body" }`. `stores` is an array, but because multi-store
  reads are DEFERRED (see Read store resolution), v1 always emits exactly one
  resolved store in it; consumers should not yet rely on more than one element.
- `hm flush --json`:
  `{ "flushed", "skipped", "failed", "unbound", "pending", "items" }`, where
  each item contains `{ "id", "store", "state", "result", "message" }`.
- `hm refresh --json`:
  `{ "indexes", "flushed", "skipped", "failed", "unbound", "pending",
  "coalesced", "write_receipts", "refreshed" }`, where
  `refreshed = false` means no unrefreshed session writes were present and the
  command skipped expensive work.
- `hm hook <event> --json`:
  `{ "event", "actions", "warnings", "memory_pending", "context_emitted",
  "refresh" }`, where `actions` is an ordered list of hook adapter actions such
  as `{ "kind": "inject_context", "body": "..." }`,
  `{ "kind": "remind", "body": "..." }`, or `{ "kind": "warn", "body": "..." }`.
  `refresh` is `null` unless the event ran receipt-aware refresh.
- `hm stores list --json`:
  `[ { "name", "store_id", "root", "available", "default", "sensitivity",
  "readable", "writable", "default_for_agent" } ]`; the last three fields are
  computed from `HIVE_MEMORY_AGENT_ID` / `--as-agent` when present and are `null`
  for unrestricted human invocations.
- `hm stores init --json`:
  `{ "name", "root", "store_id", "sensitivity" }`.
- `hm stores show --json`:
  `{ "name", "config", "manifest", "available", "effective_agent_policy" }`.
- `hm stores doctor --json`:
  `[ { "name", "root", "manifest_available", "manifest", "issues" } ]`.
- `hm projects resolve --json`:
  `{ "project_id", "project_root", "project_hint", "store", "store_source",
  "agent_id", "readable", "writable" }`.
- `hm projects bind --json`:
  `{ "project_id", "store", "binding" }`.
- `hm projects unbind --json`:
  `{ "project_id", "removed", "binding" }`, where `binding` is `null` when no
  local binding existed.
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
  hooks unless config explicitly allows it. Exception: `hm refresh --force` is
  allowed in hook mode because it only bypasses the session-receipt no-op
  optimization; it MUST NOT bypass privacy, manifest identity, or generated-file
  drift refusals. `--force` MUST NOT bypass manifest identity checks on outbox
  flush (see Local State section).
- `--idempotency-key KEY` may be passed to write commands. Reusing the same key
  with the same normalized payload returns the existing ID; reusing it with a
  different payload is a safety refusal. Retrying callers should use this after
  a transient backend failure.
- Write commands run built-in secret detectors before writing canonical memory
  or durable outbox data. Likely credentials, private keys, API tokens, SSH keys,
  OAuth tokens, and high-entropy bearer strings are exit `4`
  (`privacy_refusal`) by default. The refusal message should say that Hive
  Memory does not store secrets by default and should include only detector IDs,
  not the matched secret text.

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
  `--audience` (repeatable), `--idempotency-key`, `--allow-secret-write`.
- defaults: active store, configured default write scope `global`, confidence
  `high`, empty audience. Project-scoped writes use `--project`,
  `HIVE_MEMORY_PROJECT`, or the current working directory as a last-resort hint
  in that order.

Writes:

- Markdown note under `inbox/notes/` with TOML front matter.
- JSON event sidecar according to `event_sidecar` policy.
- The note is written with `entry_kind = "remember"` and is included by default
  in `hm context` as `trust = "remembered"` after source labeling and escaping.
- Exact normalized duplicates in the same store/scope/project/audience are
  idempotent by default for `remember`: return the existing ID instead of writing
  another note. `--idempotency-key` makes retry behavior explicit and stricter.
- When `HIVE_MEMORY_SESSION_ID` is present, append a lightweight write receipt
  under `state_dir/runs/<session-id>/writes.jsonl` after a successful created or
  duplicate/idempotent write. The receipt records the resolved store, scope,
  project ID, note ID, and created/duplicate status. Receipts are ephemeral
  coordination state for hooks; deleting them must never lose canonical memory.

Output:

- human: created note ID and relative path.
- JSON: same field set as the stable `hm remember` / `hm note` JSON contract
  above.

Errors/refusals:

- refuse empty text.
- refuse `scope = "agent-private"` without `--audience` unless `--audience-writer-only`
  is set (records `audience = [agent_id]`).
- refuse write store outside the effective agent `write_stores` policy.
- refuse likely secret material unless `--allow-secret-write` is permitted for a
  resolved `secret` store.
- refuse broad/sensitive scope mismatch unless `--force` and config allows it.
- write to outbox when active store is unavailable and offline fallback is enabled.

### `hm note`

Purpose: capture a more freeform note, usually project/session scoped.

Differences from `remember`:

- accepts multiline stdin by default.
- sets `entry_kind = "note"` with less semantic `subject` structure.
- should not imply the content is already a stable preference/fact.
- is excluded from default `hm context`; use `--include-inbox` for triage or
  `hm promote` to turn it into curated memory.
- does not perform automatic duplicate suppression unless `--idempotency-key` is
  provided.

Use `remember` for high-signal memory; use `note` for raw observations or longer
session notes.

### `hm search`

Purpose: find memories in canonical notes/curated files.

Examples:

```bash
hm search "TOML config"
hm search "release" --store work --scope project
hm search "Chris prefers" --json --include-inbox
hm search "remaining work" --since 30m --include-inbox
```

V1 behavior:

- simple deterministic text search over Markdown bodies and indexed metadata
  fields (`subject`, `tags`). Exact case-insensitive phrase matches rank
  highest; otherwise every query term must be present.
- backed by the local triage index for filtering; matched lines are read from
  canonical files for snippets.
- collapses note/event pairs (same `id`) into a single hit; the Markdown body
  is the source of the snippet.
- single resolved store only; multi-store reads (`--stores` / `--all-stores`)
  are DEFERRED (see Read store resolution).
- under agent policy, the resolved store means the agent's `default_store` (or an
  explicit `--store`), which must be within the agent's `read_stores`.
- default scopes from config; default sources from `[defaults].search_sources`
  (`curated` and `remembered` unless overridden).
- `--since` accepts `today`, durations such as `30m`/`2h`/`1d`, or an RFC3339
  timestamp, and filters indexed note records before the lexical or full-text
  backend sees candidates. Curated files do not carry indexed creation
  timestamps, so curated search remains governed by the selected source and
  scope filters.
- returns path, score/rank, title/snippet, store, scope, audience, and
  timestamp.
- deterministic ordering: exactness/term score, newer timestamp, then
  lexical path.
- default result limit: 20 unless `--limit` is provided.

Output:

- human: compact list of matches with snippets.
- JSON: array of match objects.

Future: post-v1, the simple text path can be replaced by SQLite/FTS without
changing output contracts.

### `hm context`

Purpose: assemble concise context for an agent/session.

Examples:

```bash
hm context --as-agent codex --project /repo --max-tokens 4000
hm context --store work --scope global,project
hm context --include-inbox --as-agent codex
HIVE_MEMORY_HOOK_ACTIVE=1 HIVE_MEMORY_AGENT_ID=codex HIVE_MEMORY_PROJECT=/repo hm context
HIVE_MEMORY_HOOK_ACTIVE=1 HIVE_MEMORY_SESSION_ID=abc hm context --if-changed
```

Behavior:

- reads the single resolved store and selected scopes only; multi-store reads
  (`--stores` / `--all-stores`) are DEFERRED (see Read store resolution).
- under agent policy, the resolved store means the agent's `default_store` (or an
  explicit `--store`), which must be within the agent's `read_stores`.
- when `HIVE_MEMORY_HOOK_ACTIVE=1`, uses `defaults.hook_context_max_tokens`
  when no explicit `--max-tokens` is provided, enables last-success cache
  fallback, and still enforces the active agent's read policy before returning
  either fresh or cached context.
- includes active store name, sources used, and render policy in a small
  header. The header MUST also include the active agent ID (when known), resolved
  project ID, path hint, store source (`cli`, `env`, `project-binding`,
  `agent-default`, or `global-default`), and the resolved store alias (a
  single-element list in v1; see deferred multi-store reads). Cached
  fallback context MUST mark itself stale/offline in that header, including the
  cache creation time and the resolved store alias.
- v1 default sources include curated memory (rules/, people/, memories/global/,
  memories/projects/<id>/) and high-signal `hm remember` entries. Raw `hm note`
  inbox entries are EXCLUDED unless `--include-inbox` is passed (see Trust
  Boundary).
- each rendered memory is wrapped in an explicit data-boundary block:
  `<memory id=X agent=Y store=Z scope=W trust=raw|remembered|curated>...</memory>`.
- prioritizes rules/preferences, project memory, recent high-confidence notes
  from `hm remember`, and relevant search results. Raw notes only participate
  when `--include-inbox` is passed.
- respects `--max-tokens` approximately; the v1 heuristic is `len(utf8_bytes) / 4`
  for budget estimation. Default token-ish budget: 4000.
- `--all-stores` is DEFERRED (not accepted in v1); once shipped it MUST be
  refused when config disables broad reads.
- audience filter: when `--as-agent <id>` is set, agent-private notes whose
  audience does not include `<id>` are filtered out.
- escape rule: when rendering any memory body, the CLI escapes lines that begin
  with `---`, `+++`, or `<memory`/`</memory` to prevent source content from
  terminating the data-boundary block. Raw inbox content remains excluded unless
  `--include-inbox` is passed.
- on successful fresh assembly, writes a last-success cache under
  `state_dir/context-cache/` keyed by agent ID, project ID, resolved stores,
  scopes, and source set. This cache is an operational fallback only; deleting it
  must not lose memory.
- cached fallback is used only when the selected store roots are unavailable,
  the cache is not older than `defaults.context_cache_max_age`, and every cached
  store is still permitted by the active agent's current read policy. A privacy
  refusal never falls back to stale context.
- `--if-changed` is the low-level primitive used by `hm hook` for long-lived
  sessions. It resolves the same
  agent/project/store/scope/source selection as a normal context call, compares
  it with the last context selection emitted for `HIVE_MEMORY_SESSION_ID`, and
  emits no Markdown when the selection is unchanged. When the selection changed
  (for example the agent moved from one repo to another), it emits context and
  records the new selection under `state_dir/runs/<session-id>/context.json`.
  Hook adapters should prefer `hm hook <event>`; one-off integrations may call
  this directly instead of implementing their own project-switch detector.
- `--if-changed` compares selection identity, not file mtimes. New memory writes
  are handled by `hm refresh` receipts; project/store switches are handled by
  `hm context --if-changed`.

Output:

- Markdown context block suitable for injecting into agent prompts/files.
- With `--if-changed`, no output when unchanged; exit code remains `0`.
- `--json` returns sections with source paths, audience, trust level, and
  estimated tokens, plus `emitted`.

### Supersession

When a newer record corrects an older one, broad recall must reflect current
truth instead of injecting both the stale and the corrected fact. Supersession
is the resolution layer that decides, at read time, which records to hide. It
never rewrites or deletes canonical memory; it only filters what broad recall
shows. Both `hm search` and `hm context` apply the SAME resolution through one
shared resolver so the two surfaces can never disagree about what is current.

Each surface resolves supersession with two distinct sets in mind: the set of
records eligible to ACT as a suppressor, and the set of records actually
RENDERED. A suppressor must be **audience-permitted and valid**: a record the
caller cannot see (another agent's `agent-private`) can never suppress one it
can, and a record outside its validity window (expired or not yet valid) — which
is itself dropped from rendering — can never suppress a live older fact, or the
caller would lose both the corrector and the target. Visibility and validity
therefore take precedence over supersession.

Suppressors are NOT scope/project filtered, because an explicit `supersedes`
link is authoritative ACROSS scope (see below): a global or other-project
correction must still retire its target even for a viewer who narrowed `--scope`
and would never render the corrector. The natural-language heuristic stays
conservative regardless, because it independently requires the corrector and
target to share the same scope and project, so widening the suppressor input can
never make the heuristic fire across scope. The RENDERED set still applies every
filter (source, scope, project, audience, validity); supersession then removes
any rendered record that an audience-permitted, valid record supersedes.

For `hm search` the suppressor and rendered sets coincide in the query-matched,
audience/scope/project/validity-filtered hits: search inherently considers only
records that match the query, which is its legitimate scope. The cross-scope
behavior above is specific to `hm context`, whose suppressor set is every
audience-permitted, valid record regardless of the viewer's selected scope.

There are two clearly separated confidence layers:

1. **Explicit `supersedes` links (authoritative).** A record may carry a
   `supersedes` list naming the record ids it replaces (see Markdown Note
   Schema). An explicit link suppresses its target regardless of scope, project,
   or entry kind. A correction written as a `note`, or one that moves the fact
   from project scope to global scope, still hides the fact it explicitly
   replaces. This is the durable contract a writer opts into. Explicit links are
   resolved across the FULL suppressor set the surface considers (an O(n) id
   lookup): in `hm context` that is every audience-permitted, valid record — NOT
   scope/project filtered, so a cross-scope correction retires its target even
   when the viewer narrowed `--scope` — and in `hm search` it is every
   query-matched hit, regardless of rank. An explicit correction is never missed
   because its target ranked low or because the corrector sits outside the
   viewer's selected scope.
2. **Natural-language heuristic (lower confidence, windowed).** When there is no
   explicit link, a deliberately narrow heuristic may still suppress an older
   record. It fires only when both records are `remember` entries in the SAME
   scope and project, the older body carries a stale marker (for example "used
   to", "previously", "formerly", "no longer"), the newer body carries a
   replacement marker (for example "now", "instead", "replaces", "current"), and
   the two bodies share at least two topic words. Because it is inherently
   pairwise (O(n²)), it is bounded to the top window of candidates (the top 256,
   priority/recency ordered) and is therefore best-effort: it suppresses only
   among the top candidates. The heuristic MUST NOT relax the explicit-link
   rules; it only adds suppression the explicit layer did not already provide.

Invariants:

- **Broad recall reflects current truth.** Both `hm search` (without a
  historical query) and `hm context` suppress superseded records, so the
  most-trusted agent read path never injects a fact that a newer record has
  corrected. This guarantee is *unconditional* for explicit `supersedes` links
  (resolved across the full set) and *among the top candidates* for the
  windowed natural-language heuristic.
- **Suppressors are audience-permitted and valid.** Only a record the caller may
  see and that is currently within its validity window can suppress another. An
  `agent-private` corrector the viewer cannot see, or an expired/not-yet-valid
  corrector that is itself dropped from rendering, never hides a live older fact —
  otherwise the caller would be left with neither the corrector nor the target.
- **Historical recall exception (search only).** A query that explicitly names a
  token living only in the older fact (for example searching `cargo fmt` after a
  `checkrun format` correction) keeps the older record visible, so historical
  questions can still find what the fact used to be. `hm context` has no query,
  so this exception never applies there and superseded records are always
  hidden.
- **Cycle resolution.** Explicit `supersedes` links can form a cycle of any
  length (A↔B, or A→B→C→A) through a hand-edit or import. Such a cycle must not
  erase all its members. The explicit layer detects cycle membership over the
  present-entry `supersedes` graph (a strongly connected component of size ≥ 2,
  or a self-loop) and keeps a single deterministic winner — the newest member by
  `created_at`, tie-broken by the lexicographically larger id — suppressing only
  the others. A cycle therefore never makes every fact in it vanish, no matter
  how many records it spans. An acyclic chain (A→B→C, A supersedes B, B
  supersedes C) is not a cycle: the head A stays live and B and C are suppressed.
- **Clock-skew immunity (explicit links only).** Explicit `supersedes`
  resolution is id-based and therefore clock-skew-immune: it does not depend on
  comparing `created_at` across records (the cycle tie-break uses `created_at`
  only to pick a winner among already-linked members). The natural-language
  heuristic and recency ordering, by contrast, compare wall-clock `created_at`,
  so on records written by hosts with skewed clocks the heuristic's
  newer/older decision is best-effort. Writers who need a reliable cross-host
  correction should use an explicit `supersedes` link rather than relying on the
  heuristic.
- **No canonical mutation.** Suppression is a read-time view. The superseded
  Markdown note and JSON event remain on disk and remain discoverable through
  the historical-recall exception.

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

### `hm refresh`

Purpose: one-shot hook-safe maintenance after memory writes.

Examples:

```bash
hm refresh --quiet
hm refresh --quiet --force
HIVE_MEMORY_HOOK_ACTIVE=1 HIVE_MEMORY_AGENT_ID=codex hm refresh --quiet
```

Behavior:

- runs the post-write maintenance sequence hooks need: flush local outbox writes
  and refresh affected indexes.
- when `HIVE_MEMORY_HOOK_ACTIVE=1` and `HIVE_MEMORY_SESSION_ID` are set, reads
  session write receipts from `state_dir/runs/<session-id>/writes.jsonl` and
  skips expensive flush/index work when no unrefreshed writes are present,
  unless `--force` is passed. This makes low-level integrations safe to call
  `hm refresh --quiet` broadly without implementing a fragile shell-command
  classifier; normal hooks should use `hm hook tool-complete`.
- after a successful refresh, records the consumed receipt cursor under the same
  session run state. Duplicate refresh calls are therefore cheap and idempotent.
- uses a local per-session/per-agent coalescing lock when
  `HIVE_MEMORY_HOOK_ACTIVE=1`; if another refresh is already running for the
  same session, returns success with `coalesced = true` instead of queueing more
  work. Callers should not implement their own flush/index orchestration.
- treats temporarily unavailable store roots as non-fatal when outbox fallback is
  enabled and no privacy/store-identity refusal occurred. Pending or unbound
  items are reported in output and by `hm doctor`, not hidden.
- returns non-zero for config errors, privacy refusals, manifest identity
  conflicts, or other conditions where continuing could leak memory or overwrite
  user edits.

Output:

- human: compact one-line summary unless `--quiet`.
- JSON: combined flush/index/coalescing summary.

### `hm hook`

Purpose: centralize agent lifecycle policy behind one command per hook event.

Examples:

```bash
hm hook session-start --project /repo --json
hm hook prompt-submit --project /repo --text "$PROMPT" --json
hm hook tool-complete --project /repo/src/main.rs --status 0 --json
hm hook stop --json
```

Subcommands:

```bash
hm hook session-start
hm hook prompt-submit
hm hook tool-complete
hm hook stop
```

Behavior:

- requires or infers `HIVE_MEMORY_AGENT_ID` and `HIVE_MEMORY_SESSION_ID` for
  session-scoped behavior. Missing session ID downgrades to stateless behavior
  with a warning rather than making hooks reimplement fallback logic.
- accepts the same project path hint model as `hm context` through
  `--project PATH` or `HIVE_MEMORY_PROJECT`; hooks may pass a file, buffer,
  tool working directory, or launch path.
- internally sets hook-mode semantics for the event. Hook scripts do not need to
  export `HIVE_MEMORY_HOOK_ACTIVE=1` when they call `hm hook`; the command
  applies the same recursion guards, cache fallback, and coalescing behavior
  itself. `HIVE_MEMORY_HOOK_ACTIVE=1` remains supported for hook scripts that
  call lower-level primitives directly.
- `session-start`: resolves agent/project/store policy, emits initial context,
  records the session context selection and emitted memory ids, and returns an
  `inject_context` action.
- `prompt-submit`: resolves the current project/store selection, emits context
  when the selection changed, otherwise runs bounded prompt-specific
  recall against `[defaults].search_sources`. Prompt recall keeps raw inbox
  records opt-in through that source policy, suppresses memories already emitted
  to the session, and returns an `inject_context` action only when it selects new
  useful context. It also runs the durable-memory intent heuristic and records
  `memory-pending` when the heuristic matches. It returns `inject_context`
  and/or `remind` actions as needed.
- `tool-complete`: resolves the current project/store selection when the hook
  supplies a project/path hint, emits context only when that hinted selection
  changed, runs receipt-aware refresh after successful tool events, and clears
  `memory-pending` when consumed write receipts prove a memory write occurred.
  Hooks pass tool status and an optional active path; `hm` owns deciding whether
  refresh/context actions are needed. A projectless tool completion MUST NOT
  downgrade the session's last context selection to global/no-project context.
- `stop`: if `memory-pending` remains, returns a reminder action. It may run
  `hm refresh --force` equivalent maintenance when configured, but it MUST NOT
  write canonical memory automatically.
- privacy refusals, manifest identity conflicts, generated-file drift, and
  secret write refusals are surfaced as warnings/actions for hook adapters to
  show to the user; they are not silently downgraded to another store.

Output:

- human: concise action summaries suitable for hook logs.
- JSON: ordered actions for hook adapters. Dotfiles hooks translate these actions
  into the host-specific injection/warning surfaces; they do not inspect memory
  policy state themselves. `prompt-submit` responses may include a `recall`
  object with stable diagnostic fields such as `query_fingerprint`,
  `candidate_count`, `selected_count`, `selected_ids`, `reason`,
  `reused_previous`, `timed_out`, and `retrieval_ms`; hook adapters may ignore
  this metadata and continue applying only `actions`.

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
- `list --as-agent <id>` or `HIVE_MEMORY_AGENT_ID=<id> hm stores list`: show the
  effective readable/writable/default store affinity for that agent.
- `show`: print merged config + manifest summary, including effective agent
  policy when an agent identity is active.
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
hm projects resolve [path|--project path]
hm projects bind PATH --store NAME
hm projects unbind PATH
hm projects alias <old-id> <new-id>
```

Behavior:

- `resolve`: derive the project ID for `PATH` (or `HIVE_MEMORY_PROJECT`, then
  current directory as a last resort), apply store resolution, and print the
  effective store plus its source (`cli`, `env`, `project-binding`,
  `agent-default`, or `global-default`). `PATH` can be a file, subdirectory, or
  project root. This is the hook/debug command for answering "which memory store
  would this project use?"
- `bind`: write a local project-to-store binding under `data_dir/projects/`.
  Bindings are local machine policy, not canonical store data, so work/personal
  affinity does not get committed into a shared memory store.
- `unbind`: remove the local binding for a project.
- `alias`: write `memories/projects/<new-id>/aliases.toml` recording `<old-id>`,
  enabling memory continuity across remote renames. Single-user curation per
  the same v1 constraint as `hm promote`.
- project bindings never bypass agent affinity. If `HIVE_MEMORY_AGENT_ID` or
  `--as-agent` is active and the bound store is outside that agent's read/write
  policy for the requested operation, the command exits with privacy refusal
  instead of falling back to a different store.

### `hm doctor`

Purpose: detect unsafe or broken state before hooks rely on memory.

Checks (all surfaces at default verbosity unless noted):

- config parses and validates.
- default store exists.
- store roots are reachable or outbox is enabled.
- every configured agent has a valid `default_store`, `read_stores`, and
  `write_stores`.
- local project store bindings point at configured stores.
- manifests are present and schema-compatible; schema drift surfaces the
  exact `hm stores migrate` command.
- required directories exist.
- temp files older than TTL exist.
- cloud conflict files exist (filename patterns: "conflicted copy",
  "Conflict", "sync-conflict", duplicate temp files older than TTL).
- agent policies do not give broad store access by accident, e.g. work/secret
  stores in an agent's defaults without an explicit config entry.
- remembered/note bodies do not contain likely secrets according to the current
  detector set; doctor reports detector IDs and paths, never matched secret
  values.
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
proposal workflow is implemented after the core write/search/context path works.
V1 ships `hm promote` for manual single-note promotion instead.

Future behavior:

- selects candidate notes/events by store/scope/project/age/tags.
- writes proposal under `compactions/YYYY/MM/`.
- single-user constraint applies to any apply-mode the future implements.

### `hm import claude-memory`

Purpose: import existing Claude memory-sync content without making Claude the
canonical architecture. This is deferred until the core write/search/context path
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

### Agent Guidance

The generic `hm` binary does not install or edit agent instruction files.
Dotfiles owns the tracked top-level Claude/Codex guidance and keeps it installed
through normal dotfiles update. That guidance should change rarely and MUST
instruct agents:

- treat hook-provided `hm context` as durable user/project memory.
- trust the active store header in hook-provided context; use explicit
  `--store <name>` only when the user or task clearly names another store.
- write durable preferences, workflow rules, repo conventions, and repeated
  corrections with `hm remember`.
- write project-specific facts with
  `hm remember --scope project --text "..."`; hooks set `HIVE_MEMORY_PROJECT`
  so agents do not have to infer the path from shell state.
- use `hm note` only for lower-confidence observations worth later triage.
- never store secrets, credentials, one-off task details, or noisy transcript
  summaries by default.
- when working across multiple repositories/projects in one session, pass
  `--project <path>` explicitly for memories about a project other than the
  active context header.
- prefer not writing when unsure; hooks may remind, but should not force memory
  creation.

### Agent Runtime Environment

There are two related but separate environments:

- **Agent/tool subprocess environment**: the launcher or session bootstrap should
  make `HIVE_MEMORY_AGENT_ID`, `HIVE_MEMORY_SESSION_ID`, and the best available
  `HIVE_MEMORY_PROJECT` path hint visible to normal agent tool commands. This is
  what makes an agent-issued command like
  `hm remember --scope project --text "..."` land in the right project without
  re-deriving context in the prompt. Launchers should NOT set
  `HIVE_MEMORY_PROJECT_ID` for general long-lived agent sessions; that variable
  intentionally pins identity and prevents path hints from following project
  switches. Reserve it for narrow, explicitly pinned runs.
- **Hook subprocess environment**: hook code normally calls `hm hook <event>`,
  which applies hook-safe behavior internally. `HIVE_MEMORY_HOOK_ACTIVE=1` is
  reserved for hook code that intentionally calls lower-level primitives such as
  `hm context` or `hm refresh` directly. It should not be exported into the
  long-lived agent process, because normal agent-issued `hm` commands are not
  lifecycle hook maintenance.

If a host integration cannot inject env vars into normal agent tool subprocesses,
the static agent guidance must tell agents to include `--project <path>` on
project-scoped writes. That is acceptable but less ergonomic; the preferred path
is env-backed session context plus explicit `--project` only for cross-project
writes.

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

- **Shared setup**: every Hive Memory hook path calls `hm hook <event>` with
  `HIVE_MEMORY_AGENT_ID=<id>`, `HIVE_MEMORY_SESSION_ID=<id>`, and the best
  available project path hint (`--project PATH` or `HIVE_MEMORY_PROJECT`). The
  path hint may be an active file, buffer, tool working path, or launch path; it
  does not need to be a project root. Optionally pass `HIVE_MEMORY_STORE=<name>`
  only when the launcher has a stronger context signal than config/project
  binding. Hook code should not resolve project IDs, store bindings, agent
  affinity, cache paths, refresh locks, prompt memory intent, or memory-pending
  state itself; those decisions belong in `hm hook`.
- **Re-entrancy guard**: hook entry points MUST skip Hive Memory behavior when
  `HIVE_MEMORY_HOOK_ACTIVE=1` is already present from a parent hook process.
  This prevents hook-launched `hm` maintenance commands from recursively
  triggering more memory maintenance in hosts that observe subprocesses.
- **SessionStart**: call `hm hook session-start` and inject any returned
  `inject_context` action into the agent's hook-provided additional context.
  This is the primary read path; the generated include files are a stable
  fallback and bootstrap surface.
- **Project switches in long-lived sessions**: on prompt/tool boundaries where a
  hook has a more precise active file, buffer, or tool path than the launch path,
  pass that path to `hm hook prompt-submit` or `hm hook tool-complete`. If `hm`
  returns an `inject_context` action, inject it as fresh additional context for
  the new project/store selection. Project-switch detection stays inside `hm`.
- **UserPromptSubmit**: call `hm hook prompt-submit --text <prompt>`. `hm` owns
  the durable-memory intent heuristic, `memory-pending` state, and reminder
  action text.
- **PostToolUse**: call `hm hook tool-complete --status <status>` after tool
  events. `hm` owns receipt-aware refresh, context-if-changed behavior, and
  clearing `memory-pending` when consumed receipts prove a memory write occurred.
- **Stop**: call `hm hook stop`. If it returns a reminder action, show it. Stop
  hooks MUST NOT write new memories automatically.

Hooks MAY call lower-level `hm context --if-changed` or `hm refresh --quiet` for
manual debugging or one-off integrations, but v1 dotfiles hooks should prefer
`hm hook <event>`. Hooks MUST NOT blindly summarize sessions, prompts, or
transcripts into memory.

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
   <memory id="..." agent="..." store="..." scope="..." trust="curated|remembered|raw" created="...">
   ...body...
   </memory>
   ```

   The block signals to consuming prompts that the enclosed text is DATA,
   not instructions. Prompts that consume `hm context` output should be
   constructed to honor this boundary (see README guidance for hook authors).

2. **Curated + remembered by default**. v1 default
   `[defaults].context_sources` is `["curated", "remembered"]`. Curated memory
   is human-reviewed or explicitly promoted from inbox via `hm promote`.
   Remembered memory comes from `hm remember`, is source-labeled as
   `trust="remembered"`, and is visible by default because agents and hooks use
   it for explicit durable preferences/facts. Raw `hm note` inbox entries are
   NEVER included in context unless the caller explicitly passes
   `--include-inbox` or config sets a different default.

3. **Escape rules**. For every rendered body, lines that begin with `---`,
   `+++`, `<memory`, or `</memory` are escaped (prefixed with a zero-width
   space) so source content cannot terminate the data-boundary block or
   impersonate front matter. This applies to curated memory too; promoted
   content may still contain copied raw text.

4. **Doctor patterns**. `hm doctor` flags remembered and raw inbox entries whose
   body exhibits instruction-language patterns (regex:
   `(?i)^(ignore|disregard|system|you must|now do)\b`) or length spikes
   (>5000 chars) so the user can review them before they remain in default
   context or land in curated memory via `hm promote`.

5. **Trusted writers config (deferred)**. Post-v1, `[trust] allowed_writers`
   may restrict which `agent_id` values are allowed to write at all. v1 does
   not enforce this — the design exists so the schema can evolve there.

This is the right-sized v1 answer: full prompt-injection defense is a deep area,
but unbounded raw-note inclusion is the dangerous part. V1 keeps raw `hm note`
content out of default context while making explicit `hm remember` writes useful
immediately and visibly labeled.

## Performance Budget

`hm context` runs on every agent session-start hook. Slow startup ruins the
ergonomics that make this project worth shipping.

v1 budget:

- `hm context` p95 ≤ 200ms warm (OS page cache hot, local triage index hot)
  on a 5000-note store.
- `hm context` p95 ≤ 500ms cold on the same store.
- `hm search` p95 ≤ 300ms warm on a 5000-note store with text filter.
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
- Agent/hook reads and writes are bounded by `[agents.<id>]` store affinity; an
  agent cannot silently hop into a named store outside its read/write policy.
- Read/context commands are scoped by store policy, source filters, and memory
  scope; they are not global dumps.
- Store sensitivity is metadata used for warnings/refusals.
- Write commands refuse likely secrets before writing canonical notes or durable
  outbox items. Secret-looking content may be written only with
  `--allow-secret-write` into a configured `secret` store whose config enables
  secret writes; agent hooks cannot bypass this by default.
- Agent-private scope is enforced via the `audience` field (see Markdown Note
  Schema). Context/search exclude agent-private notes whose audience does not
  include the active agent identity.
- Group/chat contexts should not receive personal memory unless explicitly
  configured for that surface.
- `doctor` warns about broad store policies and unknown/missing project claims.
- `hm context` prints active store/scope/sources metadata so mistakes are
  visible.
- `hm stores list --as-agent <id>` exposes the effective readable/writable store
  set so hooks and agents can debug affinity without parsing config.
- Absolute paths, host IDs, and session IDs are sensitive; context output should
  omit them unless they are required for the selected command output.

Recommended sensitivity levels:

- `public`: safe to read broadly.
- `internal`: safe within a trusted team/family context.
- `private`: default personal/work private memory.
- `secret`: never read automatically; explicit search/read only; refuses
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
sensitivity level relies on filesystem permissions, exclusion from cloud roots,
and explicit opt-in for reads/writes. Anyone with shell access on the host can
still read these stores. Encryption is deferred to v2; the spec is honest about
this rather than implying otherwise.

Refusal cases:

- `--all-stores` (DEFERRED; see Read store resolution) with a `secret` store
  unless `--include-secret` is passed and config allows it.
- writing to a store whose manifest identity conflicts with config unless
  `--force` is used. `--force` does NOT bypass identity checks on outbox
  flush; unbound outbox items require `hm flush --bind`.
- if config and manifest sensitivity disagree, use the stricter sensitivity
  and emit a doctor warning.
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
  `[defaults]`, `[privacy]`, `[offline]`, `[performance]`).
- Manifest schema.
- Markdown front matter schema (TOML, every required and optional field).
- JSON event schema.
- Outbox `meta.toml` schema.
- Exit codes (`0` through `5`).
- `--json` output shape per command (`hm remember`, `hm note`, `hm search`,
  `hm context --json`, `hm flush`, `hm stores list/show`, `hm doctor --json`).
- The data-boundary block syntax used by `hm context`.
- The supersession contract (see Supersession): an explicit `supersedes` link is
  authoritative across scope and entry kind and is resolved across the full set
  each surface considers (viewer-visible records in `hm context`, query-matched
  hits in `hm search`), so an explicit correction is never missed by rank;
  supersession is resolved only over records the caller can already see, so
  visibility takes precedence over supersession; broad recall in both `hm
  search` and `hm context` reflects current truth (unconditionally for explicit
  links); the historical-recall exception applies only to a search query that
  names the old fact; an explicit `supersedes` cycle of any length resolves to a
  single deterministic winner so it never erases all its members. The
  natural-language suppression heuristic is NOT frozen (it is windowed and
  best-effort) and may evolve under "Search ranking" and "Context section
  ordering and selection heuristics" below.

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
settled, while still giving downstream consumers (hooks and dotfiles
integrations) something they can build against.

## Release and CI Plan

Use GitHub Actions for tests and releases.

Recommended repository: `cgraf78/hive-memory`.

`hm --version` should print `hm X.Y.Z (git <short-sha>, schema <n>)` when build
metadata is available. Checksums use SHA-256 lines compatible with `sha256sum -c`.
Linux releases use musl targets so normal installs do not depend on a distro
glibc baseline.

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
x86_64-unknown-linux-musl
aarch64-unknown-linux-musl
x86_64-apple-darwin
aarch64-apple-darwin
```

Installer target mapping:

| OS/arch | Platform |
| --- | --- |
| Linux x86_64, including WSL | `linux-x86_64-musl` |
| Linux aarch64 | `linux-aarch64-musl` |
| macOS Intel | `macos-x86_64` |
| macOS Apple Silicon | `macos-aarch64` |

Artifact layout:

```text
hm-YYYYMMDD-HHMMSS-<8hex>-linux-x86_64-musl.tar.gz
hm-YYYYMMDD-HHMMSS-<8hex>-linux-aarch64-musl.tar.gz
hm-YYYYMMDD-HHMMSS-<8hex>-macos-x86_64.tar.gz
hm-YYYYMMDD-HHMMSS-<8hex>-macos-aarch64.tar.gz
hm-YYYYMMDD-HHMMSS-<8hex>-<platform>.tar.gz.sha256
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
  "hm-${version}-${platform}.tar.gz"
```

Installer responsibilities:

- detect OS/arch platform.
- download the matching archive and `.sha256` file.
- verify the checksum before installing.
- install `hm` into the dotfiles-managed bin dir.
- optionally install `hive-memory` alias only after that deferred decision is
  made.

Why this matters: install should be reliable during machine bootstrap, before any
agent-specific hook tries to call `hm`.

Dotfiles update integration:

- A normal dotfiles update MUST be sufficient to install the `hm` binary,
  materialize Hive Memory config, and keep Claude/Codex guidance plus hook
  wiring in place.
- After installing or updating `hm`, the dotfiles merge hook runs
  `hm doctor --quick`.
- V1 runtime correctness depends on hook-provided context and static agent
  guidance, not on `hm` mutating instruction files.

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
- missing `[agents.<id>]` resolves to conservative default-store-only policy.
- invalid agent default/read/write store aliases fail validation.

### Store affinity / projects

- write resolution order: CLI, env, local project binding, agent default, global
  default.
- read resolution order: CLI stores, CLI store, env stores, env store, local
  project binding, agent default, global default.
- file, subdirectory, and repo-root project hints resolve to the same git-root
  project identity.
- `.hive-memory-project` in a monorepo subtree overrides the outer git root for
  paths under that subtree.
- current working directory is used only when no CLI/env path hint exists.
- `hm projects bind` writes local data only and validates the target store alias.
- `hm projects unbind` removes only the local binding.
- `hm projects resolve --json` reports project ID, project root, original path
  hint, effective store, store source, agent ID, readable, and writable.
- project binding outside active agent policy exits with privacy refusal.
- `hm doctor` warns on local project bindings that target missing stores.

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
- agent policy constrains default and explicit write stores.
- likely secret material is refused before writing canonical notes or durable
  outbox data; refusal output does not echo the secret.
- `--allow-secret-write` works only for configured secret stores and only when
  config permits that mode.
- JSON output includes resolved store source, scope, project ID, and audience so
  agents can verify where the memory landed.
- session write receipt is appended when `HIVE_MEMORY_SESSION_ID` is set; receipt
  loss does not lose canonical memory.
- normalization at write time (NFC, lowercase on case-insensitive FS).

### JSON event sidecar

- sidecar policy `always` vs `never`.
- event `id` matches Markdown `id` when paired.
- schema_version field always present.
- collapsing: search/context sees one logical record per paired ID.

### Search

- case-insensitive exact phrase match, with all-term fallback.
- `--store` / `--scope` filters; multi-store `--stores` / `--all-stores` are
  DEFERRED (single resolved store in v1).
- agent policy constrains the default and explicit read store.
- `--include-inbox` opt-in.
- deterministic ordering (score → newer timestamp → lexical path).
- default limit 20; `--limit N` respected.
- audience filter when `--as-agent` set.

### Context assembler

- v1 default sources = curated + remembered.
- `--include-inbox` opens raw notes; escape rules apply to every rendered body,
  including curated content.
- data-boundary block emitted with required attributes.
- audience filter under `--as-agent`.
- agent policy constrains the default and explicit context store; multi-store
  `--stores` / `--all-stores` are DEFERRED (single resolved store in v1).
- `--max-tokens` byte/4 approximation respected.
- active store name in header.
- header and JSON include active agent ID, resolved project ID, original path
  hint, the resolved store (single-element list in v1), and store source.
- hook-active default max tokens comes from `defaults.hook_context_max_tokens`.
- fresh context writes last-success cache.
- `--if-changed` emits no Markdown when session context selection is unchanged,
  emits context when project/store/scope/source selection changes, and updates
  the session context cursor.
- backend-unavailable hook context falls back to non-expired cache only when
  current agent policy still permits the cached stores.
- stale fallback context header names cache age and the resolved store.
- privacy refusal never falls back to cached context.
- `--all-stores` is DEFERRED; once shipped it MUST be refused when config
  disables broad reads.
- performance budget: p95 ≤ 200ms warm on synthetic 5000-note store.
- `hm remember` entries are visible by default; `hm note` entries are not visible
  unless `--include-inbox` is passed.

### Adapter render framework

- Magic header + checksum write path.
- Refusal on drifted checksum without `--force --backup`.
- Refusal on missing header.
- `--upgrade-marker` re-blesses cleanly.
- Sensitive-store render refusal.
- Generated `.gitignore` for `generated/` directory.

### Agent runtime hooks

- Static Hive Memory guidance appears in the shared instruction file and remains
  stable across repeated dotfiles updates.
- Agent/tool subprocesses receive `HIVE_MEMORY_AGENT_ID`,
  `HIVE_MEMORY_SESSION_ID`, and a best-available `HIVE_MEMORY_PROJECT` path hint
  when the host integration supports env injection.
- Dotfiles hook entry points call `hm hook <event>` and translate returned
  actions into host-specific context/warning/reminder surfaces.
- `hm hook <event>` owns prompt memory-intent detection, `memory-pending`, context
  selection changes, receipt-aware refresh, and refresh coalescing.
- `hm hook session-start --json` returns an `inject_context` action with the
  same selected stores/project metadata as `hm context --json`.
- `hm hook prompt-submit --text ... --json` records memory-pending and returns a
  reminder action for durable-memory intent, without writing canonical memory.
- `hm hook prompt-submit` emits context only when the resolved
  project/store/scope/source selection changed, or when prompt-specific
  recall finds memory from `[defaults].search_sources` that was not already
  emitted to the session.
  Repeated equivalent prompt recall returns valid JSON with
  `recall.reason = "unchanged"` and no duplicate `inject_context` action.
- `hm hook tool-complete` emits context only when it receives a project/path hint
  and the resolved project/store/scope/source selection changed; projectless
  completions leave the prior context selection intact.
- `hm hook tool-complete --status 0 --json` consumes session write receipts,
  runs receipt-aware refresh, and clears memory-pending when receipts prove a
  memory write occurred.
- `hm hook tool-complete --status nonzero` does not run refresh and does not
  clear memory-pending.
- `hm hook stop --json` returns a reminder action when memory-pending remains
  and never writes canonical memory.
- `HIVE_MEMORY_HOOK_ACTIVE=1` is scoped to hook-launched low-level `hm`
  subprocesses and is not leaked into normal agent tool commands.
- Hive Memory runtime behavior is implemented in the existing dotfiles
  `agent-hook-*` scripts, not in a parallel hook stack.
- SessionStart injects the `inject_context` action returned by
  `hm hook session-start`.
- SessionStart uses last-success context cache only when the selected backend is
  unavailable, the cache is within max age, and current agent policy still
  allows every cached store.
- Prompt/tool-boundary hooks pass the best active path hint to `hm hook
  prompt-submit` / `hm hook tool-complete` so long-lived sessions receive fresh
  context after moving to another project without hook-side project tracking.
  Tool-complete hooks that intentionally omit a path stay on the cheap
  receipt/refresh path and must not force a visible global/no-project context
  reinjection.
- General long-lived agent launchers do not set `HIVE_MEMORY_PROJECT_ID`; an
  explicitly pinned project ID prevents path-hint based project switching.
- `hm hook prompt-submit` reminder action is advisory and does not write
  canonical memory.
- PostToolUse behavior is driven by `hm hook tool-complete`; no-write events are
  cheap when no unrefreshed session write receipts exist.
- Hook entry points skip Hive Memory behavior when `HIVE_MEMORY_HOOK_ACTIVE=1`
  is already present from a parent hook process.
- `hm hook tool-complete` / `hm refresh` coalesce overlapping refreshes for the
  same session instead of queueing duplicate flush/index work.
- `hm hook stop` emits a reminder when `memory-pending` remains.
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
- `hm refresh` runs flush and affected-index refresh in that order.
- `hm refresh` skips expensive work when hook/session write receipts have not
  advanced.
- `hm refresh --force` runs maintenance even without unrefreshed receipts.
- `hm refresh` reports coalesced success when a hook refresh lock is already
  active for the same session.
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

Implementation strategy:

- Work in small implementation cycles: code a narrow slice, run the relevant
  tests/checks, review the diff for policy-boundary drift, fix what the review
  finds, then move to the next slice. Do not accumulate a large unverified batch
  of `hm` behavior before testing it.
- Build from deterministic foundations outward. Start with config loading,
  store/project resolution, store affinity, and privacy refusals before any
  agent hook integration.
- Implement direct write/query commands before lifecycle automation:
  `hm remember`, `hm note`, indexing, `hm search`, then `hm context`.
- Treat `hm hook <event>` as composition over tested core modules, not as its
  own policy island. Hook behavior should call the same resolver, context,
  receipt, refresh, and prompt-intent modules used by lower-level commands.
- Integrate dotfiles hooks only after `hm hook <event> --json` action shapes are
  stable and covered by tests. Hook scripts should be thin translators from
  JSON actions to host-specific context/warning/reminder surfaces.
- Enforce the performance budget early. `hm context` is on the agent startup
  path, so slow behavior should fail tests before hook integration depends on it.
- Avoid model calls in the core v1 implementation. Prompt-intent detection,
  secret detection, context selection, and doctor checks must be deterministic.

Risks to watch during implementation:

- `hm hook` becoming a kitchen sink. Keep internals decomposed into small modules
  with clear contracts: resolver, context assembler, hook state, receipts,
  refresh, prompt intent, and action rendering.
- Project resolution drift. File, subdirectory, repo-root, monorepo subtree,
  no-git directory, symlink, and long-lived multi-project session cases all need
  dedicated tests.
- Store-affinity bypasses. Every read/write path must call the shared
  resolver and enforce agent read/write policy; no command should reimplement
  store selection.
- Secret detector noise. The default detector should catch obvious credentials
  and private keys without blocking normal technical notes too often. Refusals
  must never echo matched secret values.
- Context performance regressions. Index reads, cache fallback, token budgeting,
  and project-switch context refresh should stay within the v1 latency budget.
- Hook JSON action stability. `hm hook <event> --json` is an API for dotfiles and
  future adapters; action shapes should be versioned by tests before integration.

When ready, create GitHub issues roughly in this order. Each carries a
"Tests required" sub-list from the Testing catalog.

0. **Binary namespace check**: validate `hm` against Homebrew/Apt/common CLI
   namespaces, and `hm` collisions on Linux/macOS/WSL before cementing CLI
   examples. Current audit: `hm` is not an exact Homebrew core formula name, but
   there are existing `hm` names in Ubuntu and crates.io. V1 still keeps the
   primary binary named `hm` for direct release/shdeps installs; do not rely on
   package-manager global uniqueness, and keep the publishable project/package
   name `hive-memory`. Tests required: none (manual research issue).
1. **Project skeleton**: Rust crate, clap CLI, CI fmt/clippy/test. Tests
   required: smoke `hm --version`.
2. **License and metadata**: MIT license, README badges, contribution notes.
3. **Config loader**: TOML config, local overrides, env/CLI precedence, path
   expansion, cloud-sync prefix refusal, agent store-affinity resolution. Tests:
   Config loader (above).
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
   conflict, audience absence, secret-on-cloud,
   fsync-on-FUSE. Tests: Doctor.
10. **Simple search**: deterministic text scan over index candidates plus
    matched-line snippet read; pair collapse. Tests: Search.
11. **Context assembler**: scope/store selection, curated+remembered defaults,
    data-boundary blocks, escape rules, byte/4 token approx, performance budget.
    Tests: Context assembler.
12. **Agent runtime hooks**: static memory guidance, SessionStart context
    injection, `hm hook <event>` actions, prompt memory-intent reminders,
    memory-pending debt tracking, receipt-aware refresh, Stop reminders. Tests:
    Agent runtime hooks.
13. **Local outbox and flush**: data_dir placement, unbound state, `--bind`
    workflow, outbox-archive snapshots. Tests: Outbox + flush.
14. **Promote / inbox**: `hm promote`, `hm inbox`, single-user fcntl lock,
    promotion events. Tests: Promote / inbox.
15. **Trust boundary**: data-boundary block rendering, doctor patterns for
    instruction-language detection and length spikes. Tests: shared with
    Context assembler.
16. **Import Claude memory**: append-only migration with provenance/dedupe.
    Tests: pairing + idempotent re-runs.
17. **Cloud-sync simulation test harness**: synthetic delay/conflict/rename
    test framework used by `cloud-sync-sim` CI job. Tests: Cloud-sync
    simulation harness.
18. **Performance benchmark suite**: CI-enforced p95 budget for `hm context`,
    `hm search`, `hm flush` on synthetic stores. Tests: budget enforcement.
19. **Release automation**: target builds, archives, checksums, smoke install.
20. **Dotfiles integration PR**: shdeps install + hooks + config template.
21. **Compaction proposals**: dry-run/proposal flow, local locks, provenance.

Keep each issue small enough to review independently. The early issues should
avoid model calls entirely; `hm` needs a deterministic foundation before agents
start using it heavily.
