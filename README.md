# hive-memory

[![CI](https://github.com/cgraf78/hive-memory/actions/workflows/ci.yml/badge.svg)](https://github.com/cgraf78/hive-memory/actions/workflows/ci.yml)
[![Release](https://github.com/cgraf78/hive-memory/actions/workflows/release.yml/badge.svg)](https://github.com/cgraf78/hive-memory/actions/workflows/release.yml)

**Durable, shareable, plain-text memory for AI agents — across sessions,
agents, and machines.**

AI agents forget everything between sessions. Every new chat re-learns your
preferences, re-discovers your project conventions, and re-asks questions you
already answered. Hive Memory fixes that with one small command-line tool, `hm`,
that gives any agent a durable place to remember facts, preferences, project
context, and follow-ups — without tying that memory to a single vendor, model,
editor, or chat session.

The canonical data is just files on disk: Markdown notes with TOML front matter,
JSON event sidecars, and curated Markdown. Indexes and caches are rebuildable. A
human can read and edit the store without ever running the CLI, and any normal
file-sync (Google Drive, Dropbox, git) carries the same memory to every machine.

---

## What you get

- **One memory across sessions and agents.** Write a fact once with `hm
  remember`; recall it from any future session, with `claude`, `codex`,
  `gemini`, or your own tooling.
- **Cross-machine by file-sync.** A store is a directory with a stable UUID
  identity. Sync it however you already sync files; identity survives moves and
  renames.
- **Memory that updates itself.** Append-only with *supersession*: newer facts
  quietly hide stale ones at query time. Nothing is ever hard-deleted, so
  history stays auditable.
- **Capture and reconcile.** Distill durable facts out of a conversation, then
  fold each one into memory mem0-style (add / update / delete / noop) — with a
  review gate so capture can never silently change what an agent sees.
- **Agent lifecycle hooks.** Inject the right context at session start and at
  prompt time, and keep the index warm after tool calls.
- **Plain files, human-readable.** Browse, grep, or hand-edit the store. No
  database, no server, no lock-in.
- **Fast, no daemon.** Every command is a fresh process — no server to start or
  keep warm. A typical recall returns in well under 50ms even on a synthetic
  store of thousands of notes, and the per-turn agent hooks add only a few
  milliseconds. See [Performance](#performance).
- **Scoped recall.** Global memory follows you everywhere; project memory
  follows a repo via VCS-agnostic identity.
- **Privacy-aware.** Secret-looking content is refused on the write path;
  capture silently drops credentials before they ever land on disk.

---

## 60-second quick start

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

Initialize the store, write a memory, recall it:

```sh
hm stores init personal --root ~/hive-memory/personal

hm remember --text "Prefer small, focused patches with tests." --scope global

hm search "focused patches"
```

Ask Hive Memory for agent-ready context (Markdown wrapped in trust-boundary
blocks, fit to a token budget):

```sh
hm context --max-tokens 1200
```

```text
Hive Memory Context
store: personal
scopes: global,project
sources: curated,remembered

<memory id="…" agent="human" store="personal" scope="global" trust="remembered">
Prefer small, focused patches with tests.
</memory>
```

That output is data, not instructions: every block is labeled with its source
store, scope, and trust level so an agent can tell curated knowledge from a raw
note.

---

## Core concepts

Hive Memory keeps four things cleanly separated.

**Stores** are durable memory roots — a local directory, a synced folder, or a
network mount. Each store has a `manifest.toml` with a stable **UUIDv7
identity**, so the folder can move, sync, or be renamed without changing *which*
store it is. There is no built-in sync daemon: a store is just a directory tree
designed to ride on whatever file-sync you already use.

**Memory vs notes.** `hm remember` writes a *durable* fact, preference, or
project truth. `hm note` writes a *lower-confidence* raw note for triage. Raw
notes are searchable but excluded from injected context by default, so they can
never silently steer an agent.

**Global vs project scope.** Global memory is recalled everywhere a synced store
is present. Project memory is keyed to a project identity, so `hm search` and
`hm context` only surface it when you are working in that project (or pass
`--project`).

**Supersession — memory that updates itself.** Writes are append-only. When a
newer `remember` record replaces an older one (explicitly via `--supersedes`, or
heuristically when the wording signals a change), the stale record is *suppressed
at query time* rather than deleted. Broad recall sees only the current truth, but
a direct historical search for the old fact still finds it.

---

## Use cases

### Give every agent the same long-term memory across machines

Put the store in a synced folder and point each machine's config at it. Identity
travels with the manifest, not the path.

```toml
# ~/.config/hive-memory/config.toml  (on every machine)
default_store = "personal"

[stores.personal]
root = "${HOME}/Google Drive/hive-memory/personal"
```

```sh
# laptop
hm remember --text "Always run the test suite before opening a PR." --scope global

# desktop, later — same synced store
hm search "before opening a PR"
```

Global memory recalls everywhere. Project-scoped memory recalls on another
machine **as long as the project resolves to the same id there** (see
[Scopes and cross-machine identity](#scopes-and-cross-machine-identity) for the
honest caveat).

### Inject relevant project context at prompt time via hooks

Wire your agent host's lifecycle events to thin `hm hook` calls. The adapter
passes the event shape; `hm` returns the context or action to apply.

```sh
hm hook session-start --project ~/git/acme-api
hm hook prompt-submit --project ~/git/acme-api --text "remember to use the v2 client"
hm hook tool-complete --status 0
hm hook stop
```

- **`session-start`** assembles startup memory context for the session (fit to
  `hook_context_max_tokens`) and emits it as an inject action.
- **`prompt-submit`** refreshes context if the selection changed, runs
  prompt-specific recall, and — using a small deterministic phrase heuristic
  ("remember this", "don't forget", "from now on") — reminds the agent when the
  user clearly wants something remembered.
- **`tool-complete`** is the high-frequency post-tool hook. It keeps the search
  index warm off the hot path and refreshes context using the project id from
  the session's write receipt — never the process CWD — so home-launched,
  multi-project sessions don't mistake `$HOME` for the active project.
- **`stop`** reminds the agent at session end if a memory request was never
  satisfied.

> **Note:** There is no automatic capture-on-stop. Memory is written only when
> an agent (or you) explicitly runs `hm remember` / `hm note` / `hm capture`.
> The hooks *prompt* and *prepare*; they never silently persist memory.

### Capture durable facts from a conversation and reconcile them in

`hm capture` asks a model backend to distill a conversation into atomic, durable
facts. By default it **stages** them as raw inbox notes for review — it never
writes canonical memory by itself:

```sh
hm capture --dry-run < transcript.txt        # preview extracted facts
cat transcript.txt | hm capture              # stage as inbox notes
hm inbox list                                # review what was staged
```

When you trust the extraction, `--promote` folds each fact straight into durable
memory mem0-style, comparing it against the most similar existing records and
choosing one operation per fact:

```sh
cat transcript.txt | hm capture --promote
```

Or reconcile a single candidate directly:

```sh
echo "The default branch is now main, not master." | hm reconcile
```

```text
update: wrote <new-id> in store personal
```

Reconciliation picks **add**, **update**, **delete**, or **noop**. Update and
delete don't erase anything — they write a new record that supersedes the old
one, which is retained for audit. Capture and reconcile both require a model
backend (configured `[classifier]`, or an installed `codex` / `claude` /
`gemini`), and both refuse to write secret-looking content.

### Keep memory current without deleting history (supersession)

State a replacement and Hive Memory suppresses the stale fact from broad recall
while keeping it on disk:

```sh
hm remember --text "We deploy from the release branch." --scope project --project ~/git/acme-api
# …later…
hm remember --text "We now deploy from main instead." --scope project --project ~/git/acme-api \
  --supersedes <old-id>
```

After this, `hm context --project ~/git/acme-api` shows only the current fact. A
deliberate historical query (`hm search "release branch" --project ~/git/acme-api`)
still surfaces the superseded record — append-only means nothing is lost.

---

## Going deeper

### Retrieval backends

Hive Memory ships two candidate-generation backends, selected by
`defaults.search_backend`:

| Backend | Value | Default | What it is |
| --- | --- | --- | --- |
| Lexical | `"lexical"` | ✅ | Deterministic, stable text scan. Phrase matches weighted above term matches; ties broken by confidence, temporal intent, then recency. |
| Tantivy BM25 | `"tantivy"` | opt-in | Local **BM25** full-text index for higher recall (subject/tags field-boosted). |

```toml
[defaults]
search_backend = "tantivy"
```

This is **full-text / BM25 lexical** search — there is no embedding, semantic, or
vector search. Retrieval ranking is not yet tuned; an unrecognized
`search_backend` value degrades to lexical rather than failing. In both backends
the index returns ranked ids only; store, scope, project, audience, and validity
policy are applied as a mandatory post-filter, so the index is a recall
optimization, never a security boundary.

Inspect scoring with `hm search --explain`, and measure ranking changes against
labeled corpuses with `hm eval`.

Search has its own source defaults (`defaults.search_sources`) so explicit
recall remains broad even when prompt context is tuned for precision. Use
`--since 30m`, `--since 2h`, `--since 1d`, `--since today`, or an RFC3339
timestamp to constrain recall to recently created indexed records. Raw
lower-confidence `hm note` entries stay opt-in through `--include-inbox` or
`--source inbox`.

### Context assembly

`hm context` filters indexed records and curated files by source, scope,
project, audience, and validity, then renders each memory in a `<memory …>`
trust-boundary block up to a token budget (`--max-tokens`, a deterministic
byte/4 estimate in v1). The selection strategy is set by
`defaults.context_strategy`:

| Strategy | Default | Behavior |
| --- | --- | --- |
| `adaptive` | ✅ | Recall-safe. Withholds a remembered record only when it carries a non-startup `kind` (incident, reference, or a project-fact outside its own project). Untagged content is always kept. |
| `recency` | | Everything in scope, ordered by recency. |
| `relevance` | | Most aggressive; runs the content classifier and can withhold ambiguous global facts. |

`--if-changed` suppresses output when the session already saw the same
selection, which keeps hook output quiet.

### Classification

`hm classify` runs an optional background pass that asks a model backend to set
the durable **kind** of remembered records — `preference`, `project-fact`,
`incident`, or `reference`. Kind drives `adaptive` context selection: a
project-scoped incident can stop appearing in every session while staying fully
searchable.

The classifier is deliberately off the hot path: `hm remember`, `hm search`,
`hm context`, and hook output never invoke a model. Only `hm hook stop` may spawn
a detached `hm classify --auto` after checking local stamp/lock files; it exits
quietly when disabled, already running, fresh, or missing a backend.

```toml
[classifier]
mode = "off"          # off | auto | on
batch_limit = 25
min_interval = "6h"
timeout_seconds = 60
apply_confidence = "high"
```

In `mode = "auto"`, Hive Memory only auto-detects backend CLIs whose labels also
appear in `[agents]` (`claude`, `codex`, `gemini`) — those agents already read
memory through context, so classification adds no new implicit reader. Set
`mode = "on"` with an explicit `backend` (or a `command` that reads a prompt on
stdin and prints a JSON verdict) to use any other CLI. Inspect or test without
writing via `hm classify --pending` and `hm classify --dry-run`.

`hm retag <id> --kind <kind>` corrects a record's kind by hand. Secret stores and
audience-restricted (`agent-private`) records are never sent to any backend.

### Doctor

```sh
hm doctor             # full diagnostics
hm doctor --quick     # hook/update-safe subset
hm doctor --fix       # safe layout repairs only
hm doctor --json
```

`hm doctor` checks config, store availability and layout, generated `.gitignore`
files, sensitive-store permissions and cloud-root policy, project bindings, agent
policies, outbox state, event pairing, agent-private audiences, classifier
status, secret-looking content, and cloud-sync conflicts. `--fix` performs only
safe layout repairs — it never initializes missing stores or rewrites your
memory. `hm sync-status` reports store and index freshness without mutating
anything.

### Offline writes and the outbox

When offline fallback is enabled and a target store is temporarily unavailable,
writes are queued under `data_dir/outbox` instead of being lost. Flush them when
the store is reachable again:

```sh
hm refresh                              # rebuild local indexes/state
hm flush                                # publish queued writes
hm flush --bind <outbox-item-id> --store personal
```

Flushing verifies the target store's manifest identity before publishing. An item
queued before the store identity was known stays unbound until you bind it
explicitly.

### Scopes and cross-machine identity

Project identity lets memory follow a repo without depending on the agent
process's current directory. Resolution is **shell-free and VCS-agnostic**,
first match wins:

1. `--project-id` (explicit)
2. `HIVE_MEMORY_PROJECT_ID` (environment)
3. a `.hive-memory-project` marker file (TOML with an `id`), found by walking
   ancestors
4. the normalized VCS remote URL — works across `.git`, `.hg`, `.jj`, and `.svn`
   (read directly from on-disk VCS config, no subprocess on the common path)
5. a `$HOME`-relative path key as a final fallback

```sh
hm projects resolve ~/git/acme-api
hm projects bind ~/git/acme-api --store work
hm projects alias old-project-id new-project-id
```

```text
project_id: github-com-acme-api-…
project_source: git-remote
```

SSH and HTTPS spellings of the same remote collapse to one identity, and project
renames are handled by shared alias metadata so every machine maps old → new id.

**Cross-machine caveat (be honest about this):** project-scoped memory recalls on
another machine only when the project resolves to the **same id** there — which
is guaranteed for explicit/env ids, marker files, VCS remotes, and
`$HOME`-relative layouts. Projects *outside* `$HOME` (or hosts where `$HOME` is
unknown) fall back to a host-local absolute path and won't match across machines;
declare a `.hive-memory-project` marker or rely on a VCS remote for those.
**Global-scope memory always recalls everywhere a synced store is present.**

### Trust and privacy

Hive Memory treats stored memory as **data, not instructions**, and wraps every
context block in explicit source/trust boundaries. Store access is governed by
config, project bindings, per-agent read/write allowlists, and explicit flags:

```toml
[agents.codex]
default_store = "personal"
read_stores = ["personal"]
write_stores = ["personal"]
allow_all_stores = false
```

`agent-private` records require an explicit `--audience`. Secret-looking writes
are refused unless the target is a `secret` store **and**
`privacy.allow_secret_writes = true` **and** the write passes
`--allow-secret-write` (plus `privacy.allow_hook_secret_writes` for hooks).
Detection is conservative and key-driven (private keys, AWS/GitHub tokens,
`password=`/`api_key=` style assignments with real-looking values); matched
secret *values* are never echoed back. Capture drops secret-bearing candidates
silently. Hive Memory does not encrypt stores at rest — use filesystem, disk,
vault, or sync-provider encryption for sensitive data.

---

## Performance

Hive Memory is a **process-per-invocation** CLI: there is no daemon and nothing
to keep warm. It is pure Rust, reads never call a model, and recall runs over a
local, rebuildable index (deterministic lexical scan by default, opt-in Tantivy
BM25). Whatever the index returns is a recall optimization only — store, scope,
project, audience, and validity are always applied as a **mandatory post-filter**,
so candidate generation is fast without ever becoming a trust boundary. Writes
are append-only single files with no global lock, so a `remember` never waits on
a reader.

Two things are worth measuring separately, because they answer different
questions:

- **Interactive per-command latency** — what a user (or agent) feels per
  invocation on a normal small store. This includes process startup.
- **Core engine p95 at scale** — how the engine holds up at ~5000 notes, the
  number that matters for "does this stay fast as my memory grows".

**Interactive per-command latency** (small store, warm, single invocation):

| Command | Core binary (mean) | Via `hm` launcher (mean) |
| --- | --- | --- |
| `hm search` | ~3ms | ~14ms |
| `hm context` | ~5ms | ~19ms |
| `hm remember` | ~4ms | ~14ms |
| `hm doctor --quick` | ~3ms | ~12ms |
| `hm sync-status` | ~3ms | ~13ms |

The core binary is single-digit milliseconds per command. The `hm` launcher (a
thin shell wrapper that detects the calling agent so writes can record a session
receipt) adds roughly 10ms of shell startup on top — still well under what a
human perceives as instant.

**Core engine p95 at ~5000 notes** (release build, warm index, p95 over repeated
runs, each measurement includes full process startup and JSON serialization
because agent hooks pay those costs every time):

| Operation | p95 |
| --- | --- |
| `hm context` (4000-token budget) | ~46ms |
| `hm search` (term) | ~21ms |
| `hm search` (multi-word / "semantic") | ~45ms |
| `hm search` (supersession query) | ~23ms |
| `hook tool-complete` (no receipt) | ~2ms |
| `hook prompt-submit` (baseline) | ~23ms |
| `hook prompt-submit` (with recall) | ~22ms |
| `hook prompt-submit` (recall, store offline / cached) | ~18ms |
| `hm flush` (100-item outbox) | ~660ms |

The hot-path hooks that run on **every agent turn** — `session-start`,
`prompt-submit`, and the high-frequency `tool-complete` — stay in the low tens of
milliseconds or less, so wiring Hive Memory into an agent's lifecycle does not
slow the turn down.

**Honest caveats.** These numbers are approximate and machine-dependent —
measured on a typical Linux machine, so treat them as order-of-magnitude, not
guarantees. Real-world conditions shift them:

- A **cloud-synced store** (Google Drive, Dropbox) adds filesystem latency on
  top of these local-disk figures.
- The **first query after a write or change** rebuilds the local index before it
  is warm; the figures above are warm-index numbers.
- The **`hm` launcher** adds a few milliseconds of shell startup that the raw
  core binary does not.
- Writes are append-only single files (fast, no global lock) and the entire
  index is rebuildable, so recovery is `hm refresh`, not a migration.

**Reproduce it yourself.** The scaling numbers come from the `perf_budget`
integration test, which builds a 5000-note synthetic store and prints each `p95`:

```sh
cargo test --release --test perf_budget -- --ignored --nocapture
```

---

## Store layout

`hm stores init` creates this v1 skeleton:

```text
<store-root>/
  manifest.toml        # stable UUID identity
  entities.toml        # optional search alias registry
  people/
  rules/
  memories/
    global/
    agents/
    projects/
  inbox/
    notes/             # inbox/notes/YYYY/MM/DD/<note-id>.md
    events/            # inbox/events/YYYY/MM/DD/<note-id>.json
  generated/
    .gitignore         # rebuildable artifacts stay out of git
```

Canonical memory is plain Markdown with TOML front matter (delimited by `+++`)
plus JSON event sidecars. The `generated/` tree is disposable.

---

## Configuration

The default config path is `~/.config/hive-memory/config.toml`, with an optional
machine-local override at `config.local.toml` beside it. `--config <path>`
overrides the path; `HIVE_MEMORY_CONFIG` is used when `--config` is absent. A
fuller config:

```toml
schema_version = 1

default_store = "personal"
data_dir  = "${XDG_DATA_HOME:-${HOME}/.local/share}/hive-memory"
state_dir = "${XDG_STATE_HOME:-${HOME}/.local/state}/hive-memory"
cache_dir = "${XDG_CACHE_HOME:-${HOME}/.cache}/hive-memory"

[stores.personal]
root = "${HOME}/hive-memory/personal"
description = "Personal memory"
sensitivity = "private"

[defaults]
write_scope = "global"
search_scopes = ["global", "project"]
search_sources = ["curated", "remembered"]
context_sources = ["curated", "remembered"]
search_backend = "lexical"     # or "tantivy" for BM25 full-text
context_strategy = "adaptive"  # adaptive | recency | relevance
hook_context_max_tokens = 4000

[privacy]
secret_refuses_cloud_roots = true
allow_secret_writes = false
allow_hook_secret_writes = false

[offline]
enabled = true

[classifier]
mode = "off"
```

Store `sensitivity` is `public`, `internal`, `private`, or `secret` — a policy
class, not encryption. Secret stores are refused under common cloud-sync roots by
default.

---

## Command reference

All commands accept `--config`, `--store`, and `--as-agent`; most read/write
commands also support `--json`.

| Command | Purpose |
| --- | --- |
| `hm stores init\|list\|show\|doctor\|migrate` | Manage and diagnose store roots |
| `hm remember` | Write a durable fact/preference/context note |
| `hm note` | Write a lower-confidence raw note |
| `hm search <query>` | Search remembered memory (`--since`, `--include-inbox`, `--explain`) |
| `hm context` | Assemble agent-readable context (`--max-tokens`, `--if-changed`) |
| `hm capture` | Extract durable facts from a conversation; stage, or `--promote` |
| `hm reconcile` | Reconcile one candidate fact mem0-style (add/update/delete/noop) |
| `hm classify` | Run the LLM kind-classification pass (`--pending`, `--dry-run`) |
| `hm retag <id> --kind` | Correct a record's persisted kind |
| `hm projects resolve\|bind\|unbind\|alias\|list\|show` | Project identity and bindings |
| `hm inbox list\|stale\|show` | Inspect raw inbox notes |
| `hm promote <note-id> --to <path>` | Promote a raw note into curated memory |
| `hm hook session-start\|prompt-submit\|tool-complete\|stop` | Agent lifecycle hooks |
| `hm refresh` / `hm flush` / `hm outbox` | Rebuild state; publish queued offline writes |
| `hm sync-status` | Report store/index freshness (read-only) |
| `hm doctor` | Top-level diagnostics (`--quick`, `--fix`, `--json`) |
| `hm eval` | Capture retrieval misses/bad hits as eval fixtures |

Run `hm <command> --help` for the full flag set.

---

## Status

The primary binary is `hm`. The crate is pre-1.0; release versions are generated
from the UTC commit timestamp plus the commit suffix. The implemented command
surface follows the v1 schema and behavior in [SPEC.md](SPEC.md); broader design
rationale lives in [PLAN.md](PLAN.md). Because the project is `0.x`, storage
schemas may change between releases — changes to public command behavior, file
formats, or hook contracts are tracked in [SPEC.md](SPEC.md).

---

## Development

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS='-D missing-docs' cargo doc --no-deps
```

CI runs these through the shared `cgraf78/actions` Rust workflow. The library
crate uses `#![deny(missing_docs)]`. If a change affects public behavior, update
[SPEC.md](SPEC.md); if it changes design rationale or non-goals, update
[PLAN.md](PLAN.md).

## Release

Hive Memory uses the release identity scheme `YYYYMMDD-HHMMSS-<8hex>`, derived
from the UTC commit timestamp and commit hash. To publish from a clean `main`:

```sh
scripts/release.sh --push
```

Linux archives use musl targets; published assets use installer-facing platform
names (e.g. `hm-<version>-linux-x86_64-musl.tar.gz`,
`hm-<version>-macos-aarch64.tar.gz`).

## License

MIT. See [LICENSE](LICENSE).
