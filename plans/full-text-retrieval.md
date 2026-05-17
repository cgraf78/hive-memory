# Full-Text Retrieval Plan

## Goal

Make Hive Memory recall scale with years of accumulated agent memory.

The current search path is useful as a bootstrap, but it still behaves like a
small-store text scan: metadata narrows candidates, then command paths read
canonical Markdown and apply simple matching. That will degrade as stores grow.
Retrieval is core infrastructure, so `hm` should own a real local full-text
retrieval index.

The target architecture is:

```text
canonical store: Markdown notes + JSON events
local retrieval cache: Tantivy full-text index
policy and context selection: owned by hm
future compaction: optional quality maintenance, not a dependency
```

Canonical memory remains plain files. The retrieval index is local,
rebuildable, and disposable.

## Non-Goals

- Do not make Tantivy or any local index canonical memory.
- Do not require memory compaction for search, context, or hook usefulness.
- Do not move policy into hooks, launchers, or search-engine query syntax.
- Do not require embeddings or a network service for v1 retrieval.
- Do not expose full advanced search syntax as the primary agent contract.

## Core Decisions

- Tantivy is enabled by default because retrieval is core.
- Keep the existing JSONL triage index during the first implementation pass.
  Remove or narrow it only after Tantivy-backed search/context is stable.
- Do not add `hm recall` initially. Keep `hm search` as the human/debug surface
  and build agent-oriented retrieval behind `hm context` and `hm hook`.
- Validate canonical records before injecting memory into context. Human search
  may tolerate and report repairable stale hits, but context must not inject
  missing or corrupt canonical content.
- Prompt-submit retrieval is required. Session-start context is broad; prompt
  context is where recall becomes precise for long-lived agents.
- Hook retrieval must be stable. Prompt-specific context should emit only when
  relevance and selection changes are material enough to help the agent.

## Relationship To Compaction

Retrieval must work without compaction.

Compaction is still valuable later, but it is optional quality maintenance. If
search quality depends on compaction, then normal agent workflows become fragile:
agents will accumulate memories faster than compaction can be designed, reviewed,
and trusted.

Retrieval must handle:

- thousands to tens of thousands of remembered notes
- duplicate and near-duplicate memories
- stale observations mixed with stable preferences
- project-specific memory mixed with global memory
- multiple agents writing overlapping notes
- no curated summaries at all

Compaction can later improve:

- repeated fact cleanup
- curated long-term summaries
- context token pressure
- stale low-confidence memory triage

But `hm search`, `hm context`, and hooks must remain useful before compaction
exists.

## Agent Ergonomics

Normal agents should not need to know that a search index exists.

The expected manual command surface remains simple:

```sh
hm remember --text "..."
hm note --text "..."
hm search "..."
hm context --project /path
```

The expected hook surface remains policy-free:

```sh
hm hook session-start --project "$path" --json
hm hook prompt-submit --project "$path" --text "$prompt" --json
hm hook tool-complete --project "$path" --status "$status" --json
hm hook stop --json
```

Hooks pass the best available event data. `hm` resolves stores, project identity,
aliases, audience policy, index freshness, retrieval, token budgeting, and safe
context output.

Agent-visible behavior:

- SessionStart injects broad project/global memory.
- UserPromptSubmit injects prompt-specific memory when it adds value.
- ToolComplete refreshes writes/indexes and emits changed context only when
  needed.
- Stop reminds only when memory intent remains unresolved.

Agents should not need to remember refresh commands, index commands, special
offline flags, store affinity rules, or compaction workflows.

## Internal Boundaries

Keep Tantivy behind narrow interfaces so search, context, hooks, and doctor do
not each grow their own index logic.

Suggested module responsibilities:

- `DocumentExtractor`: reads canonical notes/events/curated files and produces
  normalized search documents.
- `SearchIndex`: owns index paths, schema, manifests, rebuild, refresh, status,
  locking, and repair.
- `QueryBuilder`: turns human text, hook prompts, and project/file hints into
  safe natural-text retrieval queries. It is tested independently from CLI and
  hook plumbing.
- `Retriever`: accepts query text plus resolved policy context and returns
  policy-filtered candidates.
- `ContextSelector`: turns candidates plus curated memory plus token budget into
  context-safe sections.

Only `SearchIndex` should know Tantivy schema details. `Retriever` may know about
field intent and ranking, but it should not expose Tantivy documents to hooks or
CLI presentation code.

## What Already Exists

Reuse these existing pieces instead of rebuilding them:

- Canonical note/event writers already create durable Markdown and event records.
  Retrieval should index those outputs, not introduce a second write format.
- The JSONL triage index already provides metadata extraction and cheap filtering.
  Keep it during migration as the existing metadata hot path.
- Project resolution and alias lookup already exist. Retrieval should call those
  APIs instead of re-deriving project ids from cwd or paths.
- Store affinity, source selection, and audience filtering already exist in
  search/context paths. Retrieval must use the same policy decisions.
- Hook state, refresh locks, receipts, memory-pending, and context-if-changed
  already exist. Prompt retrieval should extend that machinery rather than add a
  parallel hook state system.
- Context escaping and data-boundary wrapping already exist. Retrieved memories
  must flow through that same output path before agent injection.

## NOT In Scope

Explicitly defer:

- Memory compaction: useful later, but retrieval must work without it.
- Embedding/vector search: leave architectural room for a hybrid backend, but do
  not require model selection, network access, or vector caches for this phase.
- Advanced query syntax as default behavior: natural text is the user/agent
  contract; advanced syntax can be an explicit later mode.
- A background daemon: process-per-hook CLI behavior remains the baseline. Add a
  daemon only if measured startup/index-open cost demands it.
- Remote/shared search indexes: local cache only. Shared memory stays plain
  canonical files.
- Approximate semantic dedupe: first pass uses deterministic duplicate collapse.
  Similarity-based collapse can come later if real stores need it.
- Replacing the JSONL triage index in the first implementation pass: define its
  temporary responsibility and remove it only after Tantivy-backed paths are
  stable.

## Data Flow

Index maintenance:

```text
hm remember/note/promote
  └─ write canonical note/event
      └─ write refresh receipt
          └─ hm refresh / lazy search refresh
              ├─ DocumentExtractor reads canonical store
              ├─ SearchIndex updates Tantivy cache + manifest
              └─ previous committed index remains readable on failure
```

Human search:

```text
hm search "query"
  ├─ resolve config/store/agent/project policy
  ├─ SearchIndex opens or refreshes selected store indexes
  ├─ Retriever runs safe natural-text query
  ├─ hm post-filters by store/scope/source/project/audience
  ├─ hm reranks and dedupes candidates
  └─ CLI prints validated or explicitly stale-marked hits
```

Agent prompt retrieval:

```text
hm hook prompt-submit --project PATH --text PROMPT --json
  ├─ resolve agent/store/project policy
  ├─ build natural-text retrieval query from prompt + project/file terms
  ├─ retrieve and post-filter candidates
  ├─ ContextSelector applies gates, dedupe, and token budget
  ├─ compare retrieval fingerprint with session hook state
  └─ emit inject_context only when new context is materially useful
```

## Storage Layout

Canonical store remains unchanged:

```text
store-root/
  inbox/notes/**/*.md
  inbox/events/**/*.json
  memories/**/*.md
```

Local retrieval cache lives under `cache_dir`:

```text
cache_dir/
  search/
    <store-key>/
      tantivy index files
      manifest.json
```

The store key should include enough identity to avoid collisions between a
renamed store alias and a different physical root. It should be derived from
store alias, store id, and canonical store root fingerprint.

The manifest should be small and cheap to check:

```json
{
  "schema_version": 1,
  "search_schema_version": 1,
  "backend": "tantivy",
  "store_id": "...",
  "store_root": "...",
  "store_fingerprint": "...",
  "indexed_at": "...",
  "document_count": 12345,
  "hm_version": "0.3.0"
}
```

Deleting `cache_dir/search` must never lose memory.

`schema_version` describes the manifest shape. `search_schema_version` describes
the Tantivy schema and search document contract. Any incompatible
`search_schema_version`, Tantivy schema, store id, or store root mismatch forces
a safe local rebuild. Do not attempt cache migrations until a measured rebuild
cost requires it.

## Search Document Model

Index one logical memory record per document, not one arbitrary file.

Remembered/raw note document fields:

```text
id
store
store_id
source_kind        remembered | raw
entry_kind         remember | note
scope              global | project | agent-private
project_id
project_alias_ids
agent_id
audience
confidence
created_at
updated_at
subject
tags
body
canonical_path
event_path
content_hash
source_kind/source_ref from event metadata when present
promoted_from when a raw note has been promoted
```

Curated document fields:

```text
id
store
store_id
source_kind        curated
scope
project_id
project_alias_ids
created_at or empty
subject/title
tags if available
body
canonical_path
content_hash
```

Curated memory must participate in the same retrieval pipeline as remembered
memory. It should rank strongly, but it is not required for recall to work.

Store body text in Tantivy for search snippets, but never treat indexed body as
authoritative context. Human search may display indexed snippets after top-hit
hash validation. Agent context always re-reads canonical Markdown and uses the
existing escaping/data-boundary path.

Bound indexed body size. Index the first fixed budget of normalized text per
document and store a `body_truncated` flag when the canonical file exceeds that
budget. Oversized notes should still be discoverable through subject/tags/path
terms and the indexed prefix, but doctor should warn if agent-written memory is
large enough to hurt retrieval quality. The exact byte/character budget belongs
in the implementation, with tests that prove huge notes do not blow the indexer
memory budget.

Promotion should produce one active logical search result. Raw notes remain
canonical for audit/triage, but once a `memory.promotion` event links a raw note
to curated or remembered memory, default retrieval should prefer the promoted
record and suppress the raw source unless the caller explicitly includes inbox
sources. Duplicate collapse should use `promoted_from`/`source_ref` to avoid
showing both records in normal search/context.

Normalize fields before indexing:

- use the same Unicode normalization rules as canonical paths and project ids
- normalize enum-like values to one spelling, for example `agent-private`,
  `remembered`, `raw`, and `curated`
- store case-folded path/search terms while preserving display paths separately
- preserve canonical ids exactly, but index query-friendly aliases where useful
- apply the same case-sensitive/case-insensitive path behavior used elsewhere in
  the store

`content_hash` is the SHA-256 of the canonical Markdown file bytes as stored on
disk at indexing time. It is intentionally file-level, not body-only: any manual,
cloud-sync, or metadata rewrite means the cached search document may be stale.
Search can then skip/mark stale hits, while context re-reads and validates the
canonical file before injection. Event metadata changes that do not touch the
note body are picked up by refresh/rebuild through the normal event/index
freshness paths.

## Tantivy Schema

Use typed fields for filters and boosted text fields for ranking.

Stored/filter fields:

- `id`
- `store`
- `store_id`
- `source_kind`
- `entry_kind`
- `scope`
- `project_id`
- `project_alias_ids`
- `agent_id`
- `audience`
- `confidence`
- `created_at`
- `updated_at`
- `canonical_path`
- `event_path`
- `content_hash`
- `source_ref`
- `promoted_from`
- `body_truncated`

The schema must be a direct superset of the policy/presentation fields needed by
retrieval, doctor, JSON output, duplicate collapse, and offline cache identity
checks. If the document model gains a field that affects filtering, validation,
or display, update the Tantivy schema contract in the same change so the first
implementation does not immediately require a search schema bump.

Text fields:

- `subject` with high boost
- `tags` with high boost
- `body` with normal boost
- `path_terms` with low/medium boost for coding terms such as `AGENTS.md`,
  `Cargo.toml`, `sley`, `nvim`, and file names

Tokenization must handle agent/developer vocabulary well:

- `hm`
- `CI`
- `nvim`
- `g<C-x>`
- `AGENTS.md`
- `Cargo.toml`
- `dot update`
- `sley ready`
- hyphenated and slash-separated terms

Do not assume default natural-language tokenization is sufficient. Add tests for
coding-agent terms before treating the tokenizer as stable.

`path_terms` should be bounded and predictable. Extract:

- basename and extension
- parent directory names up to a small fixed depth
- repo/package names when known
- punctuation-split variants of common code filenames
- exact filename tokens for known files such as `AGENTS.md` and `Cargo.toml`

Do not index unbounded full paths as free text. Store canonical paths separately
for display and validation.

## Query And Ranking

Use Tantivy for candidate retrieval, then apply `hm` policy and reranking.

High-level flow:

1. Resolve config, agent, stores, project, source, and scope.
2. Ensure selected search indexes are present and fresh enough.
3. Build a Tantivy query from user query or hook prompt context.
4. Apply Tantivy filters where practical for store, source, scope, project, and
   audience.
5. Retrieve an over-fetched candidate set large enough to survive policy
   post-filtering.
6. Post-filter candidates through `hm` policy.
7. Rerank and deduplicate.
8. Return search hits or assemble context.

Policy post-filtering is mandatory. Tantivy filters are a speed optimization,
not a safety boundary.

Avoid recall loss from post-filtering:

- push coarse filters into Tantivy whenever they are cheap and trustworthy:
  store, source kind, scope, project ids/aliases, and audience terms
- still post-filter every returned candidate through `hm` policy
- over-fetch per selected store/source before merging, not just globally
- increase over-fetch when filters are broad or when many candidates are later
  removed by policy
- tests should prove an allowed hit still surfaces when earlier high-scoring
  disallowed hits exist

Default query parsing must be safe for agent prompts and code-heavy text.

- Human `hm search` defaults to natural text query mode.
- Hook prompt retrieval always uses natural text query mode.
- Natural text mode escapes or tokenizes input instead of treating punctuation,
  quotes, paths, `<tags>`, and operators as advanced syntax.
- Advanced Tantivy query syntax may be added later behind an explicit flag.
- Query parse failures degrade to a token query when possible instead of failing
  the hook.

`QueryBuilder` should be a testable unit. It owns:

- escaping/tokenizing natural text before it reaches Tantivy
- extracting project/file terms for session-start retrieval
- combining prompt text with active path/project hints for prompt-submit
- falling back from parser errors to simpler token queries
- producing a stable retrieval fingerprint for hook state

Reranking should prefer:

- active project and project-alias matches
- curated over remembered
- remembered over raw
- subject/tag hits over body hits
- high confidence over low confidence
- exact phrase over loose term matches
- newer memories as a tie-breaker, not as the dominant signal

Global memory should remain available, but when a project is active, strong
project matches should outrank generic global matches.

Keep final reranking simple enough to test. A useful shape is:

```text
final_score = lexical_score
            + field_boost
            + project_boost
            + source_boost
            + confidence_boost
            + bounded_recency_boost
            - duplicate_penalty
```

Exact weights can evolve, but tests should lock relative expectations: subject
beats body, active project beats global for comparable hits, curated beats raw,
and recency cannot bury a strongly relevant older memory.

## Duplicate Handling

Retrieval must not rely on future compaction to avoid noisy duplicate results.

Add first-pass collapse before returning or injecting:

- same canonical `id`
- same `source_ref`
- same normalized `subject`
- same `content_hash`

Later, add approximate similarity collapse if needed. Keep the first pass simple
and deterministic.

When duplicates collapse, prefer the representative with:

- curated source over remembered over raw
- higher confidence
- active project match
- newest timestamp as a final tie-breaker
- shortest useful body/snippet when all else is equal

## Cross-Store Retrieval

Multi-store retrieval must not let a large store drown out a smaller selected
store.

When multiple stores are readable:

- query each selected store independently or keep per-store candidate caps
- apply store affinity and sensitivity policy before merging
- merge candidates with a balanced cap before final rerank
- include store names in JSON and human output
- preserve context token budget across stores, with project-bound stores
  preferred over broad personal/global stores when project context is active

## Search Versus Context

`hm search` is human/debug-oriented:

- ranked hit list
- transparent snippets
- stable JSON
- explicit source/scope/project filters

`hm context` and `hm hook` are agent-oriented:

- compact retrieval
- stronger dedupe
- token-budget aware selection
- trust labels and data-boundary escaping
- no unchanged-context spam

They should share the retrieval engine, but not the final presentation policy.

Human `hm search` may display cached snippets from the retrieval index if the
canonical file still exists and its content hash matches. If validation fails,
skip or mark the hit stale and ask doctor/refresh to repair.

Agent context must re-read canonical content, validate hashes when available,
and route output through the existing context escaping and data-boundary path.

Validate only the returned top hits, not every Tantivy candidate. Candidate
retrieval must stay index-bound; canonical file reads are for presentation and
agent-injection safety.

## Prompt-Aware Hook Retrieval

Prompt-submit retrieval is the main scale feature.

Session-start can only inject broad memory because it does not know the next
task. Long-lived agents need prompt-specific recall as they move between files,
projects, and tasks.

`hm hook prompt-submit` should construct a retrieval query from:

- prompt text
- active file/project hint
- resolved project id
- stable repo/file terms such as file names and package names
- optional recent tool/file context if the hook event surface later carries it

The hook should still pass only simple inputs. Query construction belongs in
`hm`, not in the hook script.

Do not block the first implementation on richer hook payloads. Start with prompt
text plus project/file hint; add optional recent file/tool fields only after the
basic retrieval path is stable.

Context injection should happen only when the retrieved context changes
meaningfully. Reuse the existing context selection/cache machinery, extended with
a retrieval query fingerprint.

Use gates to avoid prompt-submit context churn:

- require a minimum relevance score
- cap new sections per prompt
- do not emit when top hits are already present in current session context
- do not emit when only ordering changes
- suppress remembered low-confidence hits unless strongly relevant
- never inject raw notes by default
- include the retrieval query fingerprint in hook state so repeated prompts do
  not re-inject equivalent context

## Session-Start Retrieval

SessionStart has no task prompt, so it must not run a vague empty query over the
whole store.

Startup context selection should be structured:

1. Include curated global/project memory first, within budget.
2. Build broad project query terms from resolved project id, project aliases,
   repo name, package names, active file path terms, and configured project
   bindings.
3. Retrieve remembered project memory with those terms.
4. Include global remembered memory only when it is high-confidence, preference
   oriented, or strongly matches project terms.
5. Apply strict per-source and per-scope caps before token budgeting.

This keeps startup context useful without turning a large store into a noisy
memory dump.

Startup query terms should come from deterministic sources:

- resolved project id and project aliases
- normalized git remote slug when available
- repository/worktree basename
- package names from known manifests, such as `Cargo.toml`, `package.json`,
  `pyproject.toml`, `go.mod`, or similar ecosystem files
- active file basename, extension, and bounded parent directory names
- explicit project binding metadata when configured

If none of those sources exist, SessionStart should include curated/global
defaults and skip remembered broad retrieval rather than querying the whole
store.

Maintain session-level retrieval state. Prompt-submit should track which memory
sections are active in the session, cap the total active retrieval sections, and
replace lower-value prompt-specific sections when a new project/query makes them
stale. This prevents a long-lived agent from accumulating unbounded injected
context across many prompts.

## Index Freshness

Hot-path checks should be cheap. Avoid walking every note on every command.

Freshness inputs:

- store id
- store root
- index schema version
- Tantivy schema version
- canonical directory fingerprint
- latest indexed write receipts when available

Write path:

- `hm remember` and `hm note` write canonical files.
- writes create receipts for refresh.
- `hm refresh` indexes affected records.
- `hm search` and `hm context` lazily refresh missing/stale indexes when safe.

Start with full rebuilds if incremental indexing adds too much complexity, but
preserve the interface shape for incremental updates. The long-term target is
receipt-driven incremental indexing plus periodic repair/full rebuild.

Freshness must handle non-`hm` changes, especially cloud sync and manual edits.

Use a layered model:

- receipt-driven incremental updates for writes made by `hm`
- cheap directory fingerprints for create/delete/rename detection, reusing the
  existing JSONL index fingerprint strategy unless implementation proves it is
  insufficient
- manifest schema/store/root checks on every index open
- `hm refresh --force` for explicit full rebuild
- doctor-triggered full reconciliation for content-only edits or suspicious
  drift
- periodic full reconciliation as a later maintenance option if real stores show
  drift that cheap fingerprints miss

Do not depend only on write receipts. Cloud sync can introduce files that have no
local receipt.

Directory fingerprints are intentionally cheap and may miss content-only manual
edits on some filesystems. That is acceptable only because full reconciliation is
available through `hm refresh --force`, doctor deep checks, and later periodic
maintenance if real stores need it.

Hook paths must not unexpectedly pay for a full large-store rebuild. Interactive
commands may lazily rebuild when the user asked for search/context. Hook commands
should use this order:

1. use a fresh index
2. run cheap receipt-driven refresh if it fits the hook budget
3. use an allowed labeled stale context cache when the backend/index is
   unavailable
4. emit no new retrieval context and report a warning in JSON

Only run a full rebuild from a hook when it is explicitly configured and bounded
by a short timeout. Normal full rebuilds belong in `hm refresh`, interactive
commands, or `hm doctor --fix`.

## Legacy JSONL Index During Transition

The JSONL triage index remains temporarily responsible for:

- existing context/search metadata flows not yet moved to Tantivy
- fast non-text metadata inspection during incremental rollout
- compatibility with current tests until the equivalent retrieval tests exist

It should not gain new full-text responsibilities. Exit criteria for removing or
narrowing it:

- `hm search` uses Tantivy for remembered, raw, and curated memory
- `hm context` selects remembered memory through retrieval
- doctor can diagnose and repair Tantivy indexes
- performance tests cover warm search/context on large stores
- no command depends on JSONL-only metadata that Tantivy documents cannot supply

If those conditions are met, either remove the JSONL index or keep it explicitly
metadata-only with a documented owner.

## Concurrency And Locks

Hooks can fire close together, and multiple agents may share a machine.

Search index maintenance should use one writer lock per store index:

- readers keep using the last committed index while a rebuild is in progress
- refresh coalesces or skips when another refresh is already active
- failed rebuild leaves the previous committed index intact
- lock files live in local state/cache, never in the shared store
- stale locks are detected with age/process checks before being ignored

This mirrors the existing hook-refresh coalescing policy and prevents prompt
submits from queuing unbounded index work.

Reuse the existing refresh lock helper where possible. If the current helper is
too hook-specific, extract a shared local lock primitive instead of adding a
second lock scheme. Search index locks should have the same stale-lock,
process-check, and non-overlap behavior as hook refresh locks.

Control writer resource usage:

- set an explicit Tantivy writer memory budget
- avoid rebuilding every selected store in a prompt-submit hot path
- batch commits during refresh/full rebuild
- expose rebuild duration and document count in verbose/JSON output
- make merge/compaction cost visible in performance tests if Tantivy segment
  behavior becomes a bottleneck

## Cloud And Offline Behavior

Cloud-backed stores can be temporarily unavailable or partially synced.

Expected behavior:

- If the store is available and the index is stale, rebuild or refresh.
- If the store is unavailable but a prior search/context cache is valid, hook
  context may use a clearly labeled stale fallback when policy allows it.
- If a search index contains documents whose canonical files are missing, human
  search should skip/report stale hits and doctor should repair.
- Agent context must not inject missing or corrupt canonical content.
- Doctor reports stale/corrupt search indexes and can rebuild them.

Do not let a local search cache bypass store affinity or audience policy.

If a store is unavailable, interactive `hm search` should report that it is using
only cached metadata/snippets, or refuse if the selected operation requires
canonical validation. Hook context may use the existing stale context fallback
only when current policy still permits the cached stores and the output is
clearly labeled stale.

Every content-bearing offline cache replay must prove identity before exposing
content. This applies to search snippets, retrieved candidate caches, and stale
hook/context cache fallback. Cache records must include enough identity to
reconstruct the policy boundary that created them: store alias, store id, store
root fingerprint, store sensitivity, context/search schema versions, selected
scopes/sources, agent id, project id, and audience-relevant policy inputs. If the
current config cannot resolve the same store identity and sensitivity under the
same effective agent/read policy, refuse cached snippets or stale context rather
than treating disconnected private memory as generally searchable.

## Doctor And Repair

Add doctor checks for:

- missing search index
- stale search index manifest
- manifest store id/root mismatch
- search schema mismatch
- corrupt Tantivy index
- indexed document pointing to missing canonical file
- unexpected documents outside selected store root
- document count drift beyond expected freshness rules

`hm doctor --fix` should:

- delete/rebuild corrupt local search indexes
- refresh stale search indexes
- never modify or delete canonical memory as part of search-index repair
- report rebuilt document count and repaired paths

Doctor should distinguish:

- cache repair, which is safe and automatic
- canonical memory problems, which are reported but not changed by search-index
  repair
- cloud/offline unavailability, which should not become scary recurring agent
  noise

Keep quick and deep diagnostics separate. Default `hm doctor` should inspect
manifests, schema compatibility, index openability, and cheap drift signals.
Expensive canonical hash validation across all indexed documents belongs behind a
deep/full mode or `doctor --fix`, so normal diagnostics do not become slow on
large cloud-backed stores.

## Performance Budgets

Keep the existing 5k-note budget, but add larger retrieval targets.

Suggested budgets:

- `hm search` warm p95 <= 300ms on 5k remembered notes.
- `hm context` warm p95 <= 200ms on 5k remembered notes.
- `hm search` warm p95 <= 500ms on 50k remembered notes.
- `hm hook prompt-submit` retrieval/context warm p95 <= 500ms on 50k remembered
  notes.

Measure cold index rebuild separately. Do not mix rebuild time into warm query
budgets.

Benchmark the full CLI path for hook commands, not just library retrieval. The
budget includes process startup, config load, project resolution, index open,
retrieval, canonical validation when required, context assembly, and JSON output.

Hook budget failure policy: bounded retrieval failures should degrade quietly.
When prompt-submit retrieval exceeds budget or cannot open a usable index, return
valid hook JSON with no new `inject_context` action plus a warning. Do not block
the agent prompt on full rebuild or expensive repair. Human commands and doctor
can surface the slower repair path.

Track release/build impact:

- Tantivy dependency compile time
- release binary size
- cross-platform release builds
- shdeps install behavior remains precompiled-binary based
- no runtime Rust toolchain requirement
- index compatibility across hm/Tantivy upgrades; schema changes should force a
  safe local rebuild instead of trying to read incompatible cache files

## Test Plan

Unit tests:

- extract search documents from remembered notes
- extract search documents from raw notes
- extract search documents from curated files
- index project aliases
- index agent-private audience
- detect stale manifest
- detect corrupt/missing index
- collapse duplicate candidates deterministically
- handle coding-agent tokenizer cases
- query builder escapes code-heavy prompt text and falls back after parser errors
- `content_hash` changes when canonical Markdown bytes change
- oversized notes are truncated for indexing without exceeding memory budgets

CLI tests:

- `hm search` finds subject-only hits
- `hm search` finds body hits
- subject hits rank above body-only hits
- project search excludes other-project memory
- project aliases retrieve old project memories
- global memory can appear but ranks below active project memory
- raw notes are excluded by default
- raw notes are included with `--include-inbox`
- promoted raw notes collapse to the promoted record by default
- agent-private memory is excluded for the wrong agent
- negated preferences preserve enough context, for example "do not use X" does
  not get reduced to a misleading positive keyword hit
- prompt-submit does not re-emit unchanged or low-value context
- multi-store search uses balanced store caps before final rerank
- stale cached search hits are skipped or marked, never injected into context
- JSON output remains stable
- missing index rebuilds lazily
- `hm refresh --json` reports retrieval index work
- `hm doctor --fix` repairs corrupt retrieval index

Quality tests should use realistic agent queries:

- `agent ergonomics plain hm`
- `release binaries github`
- `nvim diff hunk revert`
- `dotfiles bare repo launcher`
- `rust warnings fail compile`
- `gdrive cloud root unavailable`
- `sley ready check secrets verify`
- `Cargo.toml edition rustfmt`

Performance tests:

- 5k-note warm search
- 50k-note warm search
- 50k-note prompt-submit retrieval
- index rebuild benchmark tracked separately

## Failure Modes

Each implementation phase should add tests or explicit handling for these
production failures:

| Codepath | Failure mode | Required handling |
|---|---|---|
| Document extraction | malformed note/event metadata | warn and skip bad record without poisoning the whole index |
| Document extraction | canonical file disappears mid-read during cloud sync | skip record, report warning, keep previous committed index if rebuild cannot complete |
| Index open | manifest schema or Tantivy schema mismatch | rebuild local cache |
| Index refresh | writer lock already held by another hook | coalesce/skip refresh and keep readers on previous committed index |
| Index refresh | rebuild fails halfway | leave previous committed index intact and report repairable cache failure |
| Hook retrieval | selected store needs full rebuild | do not rebuild by default; emit no new context or allowed stale fallback |
| Hook retrieval | retrieval exceeds budget | return valid JSON without `inject_context` plus warning |
| Human search | top hit canonical file missing or hash mismatch | skip or mark stale; never silently print misleading canonical content |
| Agent context | candidate canonical file missing/corrupt | exclude from context and report warning through hook JSON |
| Document extraction | oversized canonical note | truncate indexed body, set `body_truncated`, and warn through doctor |
| Promotion | raw note has a promotion event | suppress raw source by default and return the promoted logical record |
| Prompt-submit | prompt produces low-score or duplicate context | emit no context instead of churn |
| Cross-store search | large broad store dominates small project store | apply per-store caps before final rerank |
| Offline/cloud unavailable | selected store root missing | use labeled stale hook cache only when policy allows, otherwise report backend unavailable |
| Upgrade | existing search index built with old schema | rebuild local cache safely |

## Worktree Parallelization Strategy

The implementation has independent lanes after the shared interface/document
model lands.

| Step | Modules touched | Depends on |
|---|---|---|
| Spec/interface | docs, retrieval module skeleton | — |
| Document model | retrieval/document extraction, curated/note/event readers | Spec/interface |
| Tantivy backend | retrieval/search index, Cargo/dependency config | Document model |
| Refresh integration | refresh/main CLI, receipts, search index | Tantivy backend |
| Search integration | search CLI, retriever, ranking/snippets | Tantivy backend |
| Context integration | context assembler, hook context output | Search integration |
| Prompt-submit retrieval | hook prompt-submit, hook state/cache | Context integration |
| Doctor repair | doctor, search index status/repair | Tantivy backend |
| Performance coverage | perf tests/CI | Search + Context integration |
| Legacy cleanup | old JSONL/search paths, docs | Search + Context + Doctor stable |

Parallel lanes:

- Lane A: Tantivy backend -> refresh integration -> doctor repair.
- Lane B: search integration after Tantivy backend.
- Lane C: context integration -> prompt-submit retrieval after search
  integration.
- Lane D: performance coverage can begin with synthetic fixtures after Tantivy
  backend, then finalize after search/context integration.

Launch Spec/interface and Document model sequentially first. After the Tantivy
backend compiles and has basic tests, Lane A doctor work and Lane B search work
can proceed in parallel with careful coordination around the retrieval module.
Context and prompt-submit should remain sequential because they share hook state
and context-selection behavior.

## Implementation Tasks

Synthesized from this review. Each task should land with focused tests.

- [ ] **T1 (P1)** — docs — Update `SPEC.md` and `PLAN.md` for local full-text retrieval.
  - Surfaced by: scope/distribution review.
  - Files: `SPEC.md`, `PLAN.md`, `plans/full-text-retrieval.md`.
  - Verify: `sley ready --path SPEC.md --path PLAN.md --path plans/full-text-retrieval.md`.
- [ ] **T2 (P1)** — retrieval — Add the document extraction model for remembered, raw, and curated memory.
  - Surfaced by: architecture review.
  - Files: `src/retrieval*`, note/event/curated readers as needed.
  - Verify: unit tests for every document source and normalization case.
- [ ] **T3 (P1)** — retrieval — Add the Tantivy search index backend with manifest, locks, and rebuild.
  - Surfaced by: architecture/performance review.
  - Files: `Cargo.toml`, `src/retrieval*`, cache/index helpers.
  - Verify: unit tests for rebuild, stale schema, corrupt index, lock behavior.
- [ ] **T4 (P1)** — refresh — Wire `hm refresh` to maintain retrieval indexes.
  - Surfaced by: agent ergonomics review.
  - Files: refresh CLI path, receipts, retrieval index.
  - Verify: CLI test for `hm refresh --json` reporting retrieval index work.
- [ ] **T5 (P1)** — search — Route `hm search` through retrieval with policy post-filtering and reranking.
  - Surfaced by: quality/scaling review.
  - Files: search CLI/library, retriever, snippets.
  - Verify: CLI tests for ranking, project aliases, raw exclusion, audience filtering, stale hit handling.
- [ ] **T6 (P1)** — context — Make remembered-memory context retrieval-driven.
  - Surfaced by: architecture/agent ergonomics review.
  - Files: context assembler, retrieval selector.
  - Verify: context tests for large noisy stores, token budget, canonical validation.
- [ ] **T7 (P1)** — hooks — Add prompt-submit retrieval with relevance/change gates.
  - Surfaced by: prompt churn risk.
  - Files: hook prompt-submit, hook state/cache.
  - Verify: hook tests for no re-emit, low-score suppression, project switch, changed prompt context.
- [ ] **T8 (P2)** — doctor — Diagnose and repair retrieval indexes.
  - Surfaced by: cloud/offline and cache repair review.
  - Files: doctor, retrieval status/repair.
  - Verify: doctor tests for missing/stale/corrupt index and safe rebuild.
- [ ] **T9 (P2)** — perf — Add full CLI-path performance coverage.
  - Surfaced by: performance review.
  - Files: perf tests/CI.
  - Verify: 5k/50k warm search and prompt-submit budgets, cold rebuild measured separately.
- [ ] **T10 (P3)** — cleanup — Remove or narrow the JSONL triage index after retrieval paths are stable.
  - Surfaced by: code quality review.
  - Files: legacy index/search docs and code.
  - Verify: no command depends on JSONL-only behavior; full test suite passes.

## Implementation Phases

### Phase 1: Spec And Interface

- Update `SPEC.md` and `PLAN.md` to describe full-text retrieval.
- Add internal retrieval/index interfaces.
- Document that retrieval does not depend on compaction.

### Phase 2: Document Model

- Add search document extraction for remembered, raw, and curated memory.
- Include policy-relevant metadata in documents.
- Add unit tests for extraction and tokenizer-sensitive terms.

### Phase 3: Tantivy Backend

- Add Tantivy dependency.
- Build index schema.
- Implement full store rebuild.
- Write manifest after successful rebuild.
- Add status/freshness checks.

### Phase 4: Refresh Integration

- Teach `hm refresh` to refresh retrieval indexes.
- Include retrieval index work in JSON output.
- Keep legacy JSONL triage index during this phase.

### Phase 5: Search Integration

- Route `hm search` through Tantivy.
- Keep policy post-filtering in `hm`.
- Add reranking, snippets, and dedupe.
- Add CLI tests for ranking and filtering.

### Phase 6: Retrieval-Driven Context

- Keep curated memory direct/high-priority.
- Select remembered memory through retrieval.
- Preserve token-budget behavior and data-boundary escaping.
- Add context tests for noisy large stores.

### Phase 7: Prompt-Submit Retrieval

- Build prompt-aware retrieval query construction inside `hm`.
- Extend context cache keys with retrieval query fingerprints.
- Inject changed prompt-specific context from `hm hook prompt-submit`.
- Add relevance/change gates so prompt-submit does not spam context.

### Phase 8: Doctor And Repair

- Add retrieval index doctor checks.
- Add repair/rebuild behavior.
- Ensure repair never touches canonical memory.

### Phase 9: Performance Coverage

- Add 5k and 50k retrieval benchmarks.
- Keep cold rebuild metrics separate from warm query budgets.
- Wire CI budget checks where practical.

### Phase 10: Legacy Cleanup

- Decide whether the JSONL triage index is still needed.
- Remove legacy scan-based search path if redundant.
- Tighten docs around the final retrieval architecture.

## Risks

- Tantivy tokenizer defaults may mishandle coding-agent vocabulary.
- Index rebuild may be too slow on cloud-backed stores if freshness checks are
  too broad.
- Prompt-submit retrieval could spam context if cache keys are too sensitive.
- Ranking could over-prefer recency and bury stable old preferences.
- Index filters could accidentally be treated as policy boundaries.
- Search snippets could bypass existing prompt-injection escaping if not routed
  through context-safe output paths.
- Raw agent prompts could break advanced query parsers unless hook retrieval uses
  safe natural-text query mode.
- Two temporary indexes could drift during migration unless the JSONL triage
  index has a narrow, explicit responsibility.
- Large dependency/build changes could affect release packaging if not checked
  early.

## Success Criteria

- Agents get relevant memory automatically through hooks without new habits.
- Human `hm search` finds natural keyword queries over large stores quickly.
- `hm context` remains concise as remembered memory grows.
- Retrieval works with zero compaction.
- Search indexes are local, rebuildable, and safe to delete.
- Store, scope, project, and audience policy remain centralized in `hm`.
