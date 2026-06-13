# Mem0-Inspired Memory Improvements

> **For agentic workers:** This is an implementation plan, not a speculative
> design note. Work it in order unless a task explicitly says it can be
> parallelized. Keep each landed change small, update this file's checkbox state
> as you go, and preserve Hive Memory's core invariant: canonical memory is
> human-readable plain files; indexes and model-derived structures are disposable.

## Goal

Adopt the useful mechanisms from Mem0 without turning Hive Memory into an app
SDK, hosted service, or opaque vector database.

The Mem0 mechanisms worth borrowing are:

- layered memory semantics: conversation/session/user/org maps to
  scratch/session/project/global in Hive terms
- single-pass ADD-only extraction: derive new durable candidates without asking
  the model to mutate old memory
- hybrid retrieval: lexical search, optional semantic similarity, and entity
  boosts
- entity linking as a ranking signal, not a graph database dependency
- graceful degradation: every optional retrieval signal can disappear without
  breaking `hm context`, hooks, or search
- MCP access: agents can call the same controlled memory API from non-shell
  clients

Do not copy Mem0's center of gravity. Hive Memory remains a local-first,
vendor-neutral, shell-native memory control plane with explicit provenance,
project identity, store policy, and reviewable files.

## Adversarial Framing

This plan should be rejected if it becomes "build Mem0 inside Hive Memory."
Hive's existing advantage is operator trust: clear files, explicit writes,
project policy, shell-native hooks, and predictable startup context. Any proposed
feature must strengthen that advantage or prove, with an eval, that it fixes a
real recall failure.

The high-risk failure modes are:

- **Context spam:** prompt-submit retrieval could inject memory too often and
  make agents less focused.
- **Schema churn:** entity, supersession, and extraction metadata could make the
  plain-file store harder to understand before their value is proven.
- **Hot-path latency:** retrieval could silently move expensive index rebuilds,
  embeddings, or canonical file scans into hooks.
- **Opaque ranking:** hybrid signals could make it hard to explain why a memory
  appeared.
- **False confidence from evals:** tiny hand-picked corpora could pass while
  real stores still produce noisy recall.
- **Product drift:** MCP, extraction, and semantic retrieval could distract from
  the core job: better local recall for existing CLI/hooks.

The core MVP is therefore narrow:

1. build eval/perf coverage,
2. land local lexical retrieval,
3. add prompt-aware hook recall with strict no-spam gates,
4. prove end-to-end usefulness through the real CLI/hook path.

Everything after that is conditional.

## Current Baseline

Hive Memory already has several pieces that overlap with Mem0's value:

- canonical Markdown/TOML notes and JSON event sidecars
- curated memory under `memories/`, `people/`, and `rules/`
- remembered vs raw notes, with promotion workflow
- project identity, aliases, store affinity, audience policy, sensitivity policy
- JSONL triage index with body cache
- explicit `MemoryKind`
- durable `classified` provenance
- optional background `hm classify`
- relevance-based startup injection
- `hm context --explain`

The plan therefore focuses on recall quality, extraction quality, and lifecycle
hygiene rather than replacing the storage model.

## Non-Goals

- Do not make embeddings, Tantivy, sqlite, or any entity index canonical memory.
- Do not send hook-time reads to an LLM or network service.
- Do not auto-delete or auto-update canonical memories based on model output.
- Do not add a required daemon.
- Do not put policy decisions into MCP clients, shell hooks, or index query
  syntax.
- Do not expose chain-of-thought, hidden prompts, or raw tool logs as durable
  memory.
- Do not add semantic retrieval, LLM entity extraction, or ADD-only extraction
  before lexical prompt recall has shipped and produced a measured gap.
- Do not persist new canonical metadata unless a read path uses it and tests
  prove it changes behavior for the better.
- Do not add a new user-facing command when an existing command plus a flag or
  subcommand covers the use case cleanly.

## Utility Gates

Each phase must pass these usefulness gates before implementation continues:

- **User-visible improvement:** identify the workflow that gets better. For MVP,
  that workflow is "a long-lived coding agent asks about a project/task and
  receives relevant memory that startup context correctly withheld."
- **Eval delta:** show at least one retrieval/eval case that fails before the
  phase and passes after it.
- **Noise budget:** show forbidden/duplicate/stale context does not increase.
- **Latency budget:** full CLI/hook p95 stays inside the configured budget.
- **Explainability:** JSON/debug output exposes stable reason keys and selected
  ids so a human can understand the result without reading scorer internals.
- **Rollback:** deleting generated caches or disabling optional config returns
  Hive to lexical/plain-file behavior without data loss.

If a phase cannot satisfy these gates, defer it and leave a note describing the
missing evidence.

## Architecture Target

```text
canonical store
  Markdown notes + TOML front matter + JSON events + curated Markdown

derived local indexes
  lexical index        required, local, rebuildable
  entity index         conditional, local, rebuildable
  embedding index      conditional, local or configured backend, rebuildable

workers
  classifier           already exists; tags injection behavior
  extractor            proposes/promotes durable facts from notes/session logs
  index refresh        maintains lexical/entity/embedding sidecars

read paths
  hm search            human/debug search
  hm context/hook      policy-filtered, token-budgeted, trust-wrapped context
  hm recall            do not add unless hook recall needs a debug surface

write paths
  hm remember/note     explicit canonical writes
  hm extract           conditional proposal workflow, not MVP
  hm promote/retag     human/agent correction path with provenance
```

## Test And Performance Strategy

This work must be test-led. Memory recall changes are easy to make plausible and
hard to notice when they regress, so every phase needs both correctness tests and
latency gates.

Use the existing test structure:

- `tests/cli.rs` for command and hook contract tests.
- `tests/injection_eval.rs` for startup-context relevance behavior.
- `tests/classify_eval.rs` as the pattern for model-backed evals and
  skip-if-no-backend ignored tests.
- `tests/perf_budget.rs` for CI-enforced full CLI-path performance budgets.
- `tests/cloud_sync_sim.rs` for file-sync edge cases when schema or cache state
  changes.
- `tests/fixtures/*.toml` for deterministic corpora.

Performance tests must measure the user-visible command path, not just library
functions. Hook budgets include process startup, config load, project
resolution, store policy, index open, retrieval, policy filtering, canonical
validation for selected records, context rendering, hook-state read/write, and
JSON serialization.

Suggested budget classes:

- **warm startup context:** keep the existing `hm context` 5k-note budget.
- **warm search:** keep the existing `hm search` 5k-note budget and add a 50k
  retrieval budget after the Tantivy index lands.
- **warm prompt recall:** add `hm hook prompt-submit --json` budgets for 5k and
  50k remembered notes.
- **cold index rebuild:** measure separately from warm hook/search budgets.
  Cold rebuild can be slower, but the hook path must not unexpectedly do it.
- **degraded hook path:** index unavailable, stale, locked, or over budget must
  return valid hook JSON quickly with no `inject_context` action and a stable
  reason key.
- **optional semantic/entity sidecars:** cache-miss and provider-failure paths
  must stay within lexical-only budgets by skipping optional signals.

CI should continue running `cargo test --release --test perf_budget -- --ignored
--nocapture` as a separate job with `HIVE_MEMORY_PERF_BUDGET_MULTIPLIER` for
shared runners. Add new ignored perf tests there rather than to the default
unit-test suite.

## Phase 1: Lock The Evaluation Harness

**Purpose:** Improve retrieval without arguing from anecdotes.

- [ ] Create `tests/fixtures/retrieval_corpus.toml`.
  Include at least:
  - global preferences that should inject at startup
  - project facts with active/inactive project aliases
  - stale operational incidents
  - duplicate and near-duplicate facts
  - facts requiring lexical terms (`AGENTS.md`, `Cargo.toml`, `sley`, `checkrun`)
  - facts requiring semantic recall where the query does not share exact words
  - entity-heavy facts involving repo names, hosts, tools, branches, and people

- [ ] Add a retrieval eval test module.
  Suggested file: `tests/retrieval_eval.rs`.
  It should create a temp store, write corpus records through public APIs, run
  retrieval/context commands, and assert expected ids appear or do not appear.

- [ ] Add end-to-end hook eval fixtures.
  The retrieval corpus should include enough metadata to run a full sequence:
  `session-start`, `prompt-submit`, `tool-complete`, second `prompt-submit`,
  and `stop`.

- [ ] Define metrics in code, not prose.
  Track:
  - recall@k for expected memories
  - forbidden-hit count for memories that must not inject
  - duplicate collapse correctness
  - context token budget compliance
  - warm/cold latency budget
  - hook action count and action kinds
  - hook recall reason keys
  - emitted memory ids across startup and prompt recall

- [ ] Add `hm context --explain` assertions for startup context.
  This prevents retrieval improvements from reintroducing noisy always-on
  memories.

- [ ] Add a benchmark fixture large enough to catch scaling mistakes.
  This can be synthetic, but it must include enough distractors that ranking
  quality matters.

- [ ] Add a "current behavior" baseline report.
  Capture which eval cases pass with today's substring search/relevance
  behavior. This prevents the new retrieval stack from taking credit for cases
  that were already working.

- [ ] Add adversarial corpus cases.
  Include:
  - prompt injection text inside memory bodies
  - stale "do not merge" operational instructions
  - same fact under active and inactive project ids
  - generic global memories that share many prompt terms but should not win
  - very long notes that should be indexed only through bounded text
  - private audience records that must not leak into another agent's recall

**Done when:** retrieval changes can fail a test before they reach users.

## Phase 1A: Add Performance Fixtures Before Retrieval Changes

**Purpose:** Make performance regressions visible as soon as retrieval code
starts moving.

- [ ] Refactor `tests/perf_budget.rs` fixture helpers.
  Reuse synthetic store generation across:
  - context/search budget tests
  - prompt-submit hook budget tests
  - future 50k-note retrieval tests

- [ ] Add synthetic project-scoped data generation.
  The current perf fixture is global-only. Prompt recall needs:
  - active project memories
  - inactive project distractors
  - global preference distractors
  - raw inbox distractors
  - duplicate subjects/content hashes
  - path-heavy terms such as `AGENTS.md`, `Cargo.toml`, and repo slugs

- [ ] Add ignored baseline test for current `hm hook prompt-submit`.
  Before retrieval lands, this should assert the existing no-recall path stays
  cheap. After retrieval lands, extend the same test to include recall.

- [ ] Add explicit budget constants in `tests/perf_budget.rs`.
  Suggested starting constants:

  ```rust
  const HOOK_PROMPT_WARM_5K_BUDGET_MS: u128 = 300;
  const HOOK_PROMPT_WARM_50K_BUDGET_MS: u128 = 500;
  const HOOK_PROMPT_DEGRADED_BUDGET_MS: u128 = 200;
  ```

  Calibrate after the first implementation pass, but keep the invariant that
  CI enforces a p95 budget and local development can use a stricter multiplier.

- [ ] Add cold rebuild timing as reporting first.
  Print cold rebuild p95/pmax in `--nocapture`, but do not fail CI until the
  index design stabilizes. Warm hook/search budgets should fail immediately.

**Done when:** the perf suite has hooks and large-store scaffolding before the
retrieval backend is swapped.

## Phase 2: Land Lexical Retrieval Properly

**Purpose:** Complete the existing full-text plan before adding semantic pieces.

Reuse `plans/full-text-retrieval.md` as the controlling implementation plan for
this phase.

- [ ] Add narrow retrieval interfaces:
  - `DocumentExtractor`
  - `SearchIndex`
  - `QueryBuilder`
  - `Retriever`
  - `ContextSelector`

- [ ] Add a Tantivy-backed lexical index behind `SearchIndex`.
  Keep the current JSONL triage index until Tantivy-backed search/context is
  stable.

- [ ] Prove Tantivy is worth its dependency cost.
  Before adopting it permanently, compare against the current JSONL path on the
  retrieval corpus and perf fixture. Keep Tantivy only if it materially improves
  one of:
  - recall/ranking quality for code-heavy terms
  - warm latency on large stores
  - ability to support prompt recall without scanning canonical files

- [ ] Index curated memory and remembered notes as the same logical document
  shape.

- [ ] Implement natural-text query parsing for code-heavy prompts.
  Add tokenizer tests for:
  - `AGENTS.md`
  - `Cargo.toml`
  - `dot update`
  - `sley ready`
  - `g<C-x>`
  - repo slugs and file paths

- [ ] Implement reranking independent of Tantivy score.
  Required ranking preferences:
  - active project over global for comparable hits
  - curated over remembered over raw
  - subject/tag hits over body hits
  - high confidence over lower confidence
  - exact phrase over loose term match
  - recency as bounded tie-breaker only

- [ ] Implement deterministic duplicate collapse.
  Start with exact mechanisms only:
  - same id
  - same source ref
  - same normalized subject
  - same content hash
  - promoted raw source suppressed by promoted target

- [ ] Keep ranking explainable.
  Return score components in debug/test-only structures or JSON `--explain`
  output. Tests should assert relative reasons like `project-boost` or
  `duplicate-suppressed`, not opaque floating-point totals.

- [ ] Preserve the trust boundary.
  Agent context must re-read canonical files and route through existing
  context escaping. Indexed snippets are display/debug material only.

**Done when:** `hm search` and `hm context` can use a real local full-text index
without embeddings or LLM calls.

## Phase 3: Add Prompt-Aware Recall

**Purpose:** Bring Mem0-style query-time recall to agents without dumping memory
at session start.

The current hook system already has the right shape:

- `hm hook session-start` injects startup context and records a context
  selection key.
- `hm hook prompt-submit` receives prompt text, validates project/store policy,
  emits context only when the selection changes, and records durable-memory
  intent.
- `hm hook tool-complete` performs receipt-aware refresh and clears memory debt
  after successful memory writes.
- `hm hook stop` reminds on unresolved memory debt and may spawn the classifier.
- `HookState` already stores `memory_pending`, `refreshed_receipts`, and
  `context_key`.
- agentguard already treats `hm hook --json` as the stable adapter contract and
  applies `inject_context` / `remind` actions generically.

Build prompt-aware recall by extending this system, not by adding another hook
adapter path.

### Phase 3A: Hook State Contract

- [ ] Extend `hook::HookState`.
  Add fields:

  ```rust
  pub startup_context_key: Option<String>,
  pub startup_memory_ids: Vec<String>,
  pub prompt_recall_key: Option<String>,
  pub prompt_recall_memory_ids: Vec<String>,
  pub prompt_recall_updated_at: Option<String>,
  ```

  Keep `context_key` for compatibility during the migration. Either map it to
  `startup_context_key` internally or keep it as the broad project/store
  selection cursor until a later cleanup.

  Do not store prompt text in hook state. Store only fingerprints and selected
  memory ids. Hook state may live in local files visible to humans and should
  not accumulate user prompts.

- [ ] Define stable cursor semantics.
  `startup_context_key` tracks broad selection identity:
  agent, stores, project id/path hint, scopes, sources, inbox/search-only policy,
  and context strategy.

  `prompt_recall_key` tracks prompt-specific retrieval identity:
  normalized prompt query fingerprint, resolved project id, path terms, selected
  stores, source/scope policy, retriever schema version, and top selected memory
  ids.

- [ ] Add helper APIs in `src/hook.rs`.
  Suggested functions:
  - `mark_startup_context(...)`
  - `mark_prompt_recall(...)`
  - `last_prompt_recall(...)`
  - `known_session_memory_ids(...)`

  Do not let `main.rs` manipulate hook-state fields directly beyond these
  helpers.

- [ ] Preserve JSON compatibility.
  Old hook state files missing these fields must deserialize through
  `#[serde(default)]` and behave as if no prompt recall has been emitted.

### Phase 3B: Retrieval Query From Hook Prompt

- [ ] Add `Retriever::retrieve_prompt`.
  Input should include:
  - prompt text
  - active path/project hint
  - resolved project id and aliases
  - active agent id
  - selected readable stores
  - selected source/scope policy
  - already-emitted memory ids from hook state
  - token budget for prompt recall

- [ ] Add `QueryBuilder::from_prompt`.
  It should produce:
  - natural-text query terms from the prompt
  - deterministic file/path terms from `--project` or `HIVE_MEMORY_PROJECT`
  - project alias/repo slug terms
  - optional command/tool terms when supplied later
  - stable query fingerprint

- [ ] Keep prompt retrieval local and bounded.
  Prompt-submit must not call LLMs, embedding commands, network services, or
  slow rebuilds synchronously. Optional semantic retrieval may participate only
  when the local semantic cache is already available and fresh enough.

- [ ] Add timeout/budget behavior.
  If retrieval cannot finish inside the hook budget, return valid hook JSON with
  no `inject_context` action and a stable warning/reason key. Do not block the
  agent prompt.

- [ ] Add a feature/config gate for prompt recall.
  Default to off or conservative "lexical-only" until perf and E2E tests are
  green. The rollout should be reversible by config without deleting indexes.

### Phase 3C: Prompt Recall Selection Gates

- [ ] Add a `PromptRecallSelector`.
  It should sit after retrieval/reranking and before hook response emission.

- [ ] Required gates:
  - minimum final relevance score
  - max prompt-recall sections per hook call
  - max prompt-recall token budget, separate from startup context budget
  - exclude raw inbox by default
  - exclude search-only memory unless strongly relevant
  - suppress memories already emitted in startup context or prior prompt recall
  - suppress output when only ordering changed
  - suppress output when the same prompt recall key was already emitted
  - suppress low-confidence remembered hits unless exact/entity/project signals
    justify them

- [ ] Add hard anti-spam limits.
  Enforce:
  - no more than one prompt recall injection per prompt-submit event
  - no more than a small configured active prompt-recall section count per
    session
  - no prompt recall injection when startup context already contains all
    selected ids
  - no injection solely because a new lower-ranked candidate appeared

- [ ] Render prompt recall through the existing trust boundary.
  Reuse context escaping and data-boundary rendering. The output action can
  still be `inject_context`; the body should have a header that distinguishes
  prompt-specific recall from startup context so agents understand why it
  appeared.

- [ ] Include selected memory ids in the hook-state update.
  This makes future prompt/tool hooks able to avoid re-emitting the same memory.

### Phase 3D: `hm hook prompt-submit` Behavior

- [ ] Keep existing behavior:
  - validate read policy
  - inject broad context only when project/store selection changed
  - detect durable-memory intent and emit `remind`
  - mark `memory_pending`

- [ ] Add prompt recall after selection-change handling.
  Order should be:
  1. resolve config, agent, session, project/path hint
  2. validate read policy
  3. emit broad context if project/store selection changed
  4. run prompt retrieval and gates
  5. emit prompt-specific `inject_context` only if useful
  6. run memory-intent reminder logic
  7. save prompt recall cursor and emitted ids

- [ ] Add hook JSON metadata.
  Extend `HookResponse` with optional structured fields:

  ```rust
  recall: Option<HookRecallReport>
  ```

  Suggested report fields:
  - `query_fingerprint`
  - `candidate_count`
  - `selected_count`
  - `selected_ids`
  - `reason`
  - `reused_previous`
  - `timed_out`
  - `retrieval_ms`

  Use stable reason keys such as `selected`, `below-threshold`,
  `unchanged`, `no-session-id`, `no-project-policy`, `index-unavailable`,
  `timed-out`, and `budget-empty`.

- [ ] Keep failure semantics boring.
  Retrieval failure must not fail the hook unless store/config policy itself is
  invalid. A ranking/index/cache problem should degrade to `recall.reason` plus
  warnings; the agent prompt must continue.

- [ ] Preserve action compatibility.
  Do not require agentguard to understand `recall` metadata. Existing
  action-processing should keep working because `inject_context` and `remind`
  remain the only action kinds for this phase.

### Phase 3E: `hm hook session-start` Behavior

- [ ] Continue emitting broad startup context once per session selection.
- [ ] Save emitted startup memory ids in hook state.
  `ContextOutput.sections` already has ids; persist them so prompt recall can
  avoid repeats.
- [ ] Do not run prompt retrieval at session start.
  There is no task prompt yet. Session start may use broad project terms only
  after Phase 2 retrieval is available, but it should stay conservative.

### Phase 3F: `hm hook tool-complete` Behavior

- [ ] Keep projectless tool-complete cheap.
  Existing behavior deliberately omits project context for high-frequency tool
  events. Preserve that default.

- [ ] Continue receipt-aware refresh after successful tools.
  If a successful tool produced `hm remember` / `hm note` receipts, refresh
  indexes and clear memory debt exactly as today.

- [ ] After refresh, invalidate prompt recall only when needed.
  If write receipts were consumed, update hook state so a future prompt can
  recall newly written memories. Do not immediately inject prompt recall from
  projectless `tool-complete`.

- [ ] If a tool-complete event includes a precise project/path hint, keep the
  existing selection-change context behavior. Do not run prompt retrieval unless
  the hook event also carries prompt text or explicit recent file/tool context
  in a future schema.

### Phase 3G: `hm hook stop` Behavior

- [ ] Keep pending-memory reminder behavior.
- [ ] Keep classifier spawn behavior local and non-blocking.
- [ ] Do not run extraction or summarization automatically on stop in this
  phase.
  ADD-only extraction proposals belong in Phase 6 and must be opt-in or
  explicitly configured.
- [ ] If Phase 6 later adds stop-time extraction, gate it behind a separate
  config key and use a detached worker following the classifier privacy model.

### Phase 3H: Agentguard / Dotfiles Adapter Work

- [ ] Verify existing action loop needs no change.
  agentguard currently reads `.warnings[]` and `.actions[]`, handles
  `inject_context` and `remind`, and warns on unknown actions. Prompt recall
  should reuse these action kinds.

- [ ] Add adapter tests only for behavior visible to the shell layer:
  - prompt-submit receives a second `inject_context` when `hm` returns one
  - unknown `recall` metadata is ignored
  - warning handling still works
  - projectless `tool-complete` still does not infer a project

- [ ] Do not make shell hooks parse recall metadata.
  Store policy, retrieval policy, and reason keys stay in `hm`.

- [ ] Keep `AGENTGUARD_HIVE_MEMORY_HOOK_ACTIVE=1` / `HIVE_MEMORY_HOOK_ACTIVE=1`
  recursion guards intact.

### Phase 3I: End-To-End Hook Tests

- [ ] Add CLI tests in `tests/cli.rs`.
  Cover:
  - session-start records startup ids
  - prompt-submit with relevant project memory emits prompt recall
  - repeated same prompt does not re-emit recall
  - prompt with only below-threshold hits emits no context and reports reason
  - prompt recall does not include raw inbox by default
  - prompt recall respects audience and project policy
  - missing `HIVE_MEMORY_SESSION_ID` returns valid JSON and does not emit repeat
    suppression state
  - successful tool-complete refresh makes newly written memory eligible for a
    later prompt
  - failed tool-complete does not refresh or clear memory debt
  - hook JSON includes `recall.reason` with a stable key for every no-context
    path
  - hook JSON remains parseable when retrieval times out or the retrieval index
    is unavailable
  - startup ids suppress duplicate prompt recall ids
  - prompt recall ids persist across process invocations through hook state

- [ ] Add retrieval eval coverage for hook prompt recall.
  The eval should assert selected ids, forbidden ids, and duplicate suppression.

- [ ] Add performance tests.
  Extend `tests/perf_budget.rs` for:
  - warm `hm hook prompt-submit` on 5k remembered notes
  - warm `hm hook prompt-submit` on 50k remembered notes
  - index-unavailable degradation path
  - repeated same-prompt no-op path
  - prompt after receipt-driven refresh
  - project switch prompt-submit path

- [ ] Add agentguard tests if adapter behavior changes.
  If the action contract remains `inject_context` / `remind`, this can be a
  small regression test rather than a full adapter rewrite.

- [ ] Add a scripted full lifecycle test.
  Suggested file: `tests/hook_lifecycle_e2e.rs` or a dedicated section in
  `tests/cli.rs`. It should run the real `hm` binary against a temp store:

  1. initialize store and config
  2. write global preference, active project fact, inactive project fact, raw
     note, duplicate remembered note
  3. run `hm refresh --force --quiet`
  4. run `hm hook session-start --project <active repo> --json`
  5. assert exactly one startup `inject_context` action and expected startup ids
  6. run `hm hook prompt-submit --project <active repo> --text <query> --json`
  7. assert prompt recall injects the active project fact and excludes inactive,
     raw, duplicate, and already-startup ids
  8. repeat the same prompt and assert no second prompt recall injection
  9. run a prompt containing durable-memory intent and assert `remind` plus
     `memory_pending = true`
  10. run `hm remember` with the same `HIVE_MEMORY_SESSION_ID`
  11. run `hm hook tool-complete --status 0 --json`
  12. assert receipts were consumed, refresh ran, and memory debt cleared
  13. run a new prompt that should recall the newly written memory
  14. run `hm hook stop --json` and assert no pending-memory reminder remains

  This test is the minimum "fully implemented" acceptance gate.

**Done when:** a long-lived agent can ask a project-specific question and get
relevant memory on that prompt without broad startup injection, without duplicate
context spam, and without changing shell hook policy code beyond thin adapter
tests.

## Phase 4: Add Entity Extraction And Linking

**Purpose:** Borrow Mem0's entity boost while avoiding a graph database.

Entity linking should improve ranking, not become a separate source of truth.

This phase is conditional. Start it only if Phase 1-3 evals show lexical prompt
recall misses important cases because the same entity is described with
different wording.

- [ ] Start with a derived entity index, not persisted front matter.
  Build deterministic entities from canonical notes during index refresh and
  store them in the rebuildable cache. Do not write entity metadata back to
  canonical notes until there is a clear audit or cross-machine need.

- [ ] Add persisted optional entity metadata only after the derived index proves
  useful.
  Suggested front matter shape:

  ```toml
  [[entities]]
  name = "hive-memory"
  kind = "repo"
  normalized = "github.com/cgraf78/hive-memory"
  source = "llm"
  confidence = "high"
  ```

- [ ] Add deterministic entity extraction first.
  Extract obvious entities without an LLM:
  - git remote slugs
  - repo basenames
  - hostnames
  - file names
  - command names
  - issue/PR refs
  - paths and package names

- [ ] Extend `hm classify` or create `hm extract-entities`.
  If LLM-backed entity extraction is added, it must follow classifier rules:
  - never hot path
  - bounded batch
  - secret stores excluded
  - agent-private records excluded unless the backend identity is allowed
  - strict JSON output
  - no model-driven deletion/update of existing canonical memory

  Prefer deterministic extraction for the first pass. LLM-backed extraction is
  a separate opt-in worker and should not be bundled with the entity index MVP.

- [ ] Add a rebuildable entity index under `cache_dir`.
  Keep it local and disposable. Store canonical entity metadata only when it is
  useful for auditability or cross-machine continuity.

- [ ] Use entities as a score boost in `Retriever`.
  Query entities should boost matching memories; entity matches must not bypass
  scope, source, audience, or project policy.

- [ ] Add entity eval cases.
  Example: a query mentioning `checkrun` should find memories that mention the
  repo/tool relationship even when exact phrasing differs.

**Done when:** entity matches improve ranking in the retrieval eval without
adding a graph dependency or changing canonical memory authority.

## Phase 5: Add Optional Semantic Retrieval

**Purpose:** Cover synonym/paraphrase recall gaps that lexical/entity retrieval
cannot cover.

This is optional and must degrade to lexical-only behavior.

Do not implement this phase just because Mem0 uses semantic search. Implement it
only if:

- lexical + deterministic entity retrieval fails named eval cases,
- the failed cases matter to real agent workflows,
- semantic retrieval fixes those cases without adding unacceptable latency or
  noise, and
- the optional provider/cache can be disabled with no behavior breakage.

- [ ] Add `[retrieval.semantic]` config.
  Suggested shape:

  ```toml
  [retrieval.semantic]
  mode = "off"          # off|local|command
  command = []
  model = ""
  dimensions = 0
  batch_limit = 100
  timeout_seconds = 60
  ```

- [ ] Define an `EmbeddingProvider` trait.
  Implement command-backed embeddings first if no stable local Rust embedding
  dependency fits the project. The provider is a batch worker, not a hook-time
  dependency.

- [ ] Add a no-network hot-path invariant test.
  Hook prompt-submit must not spawn embedding commands or call a provider when
  the local semantic cache is cold, stale, or missing.

- [ ] Store embedding vectors only in a rebuildable local cache.
  Canonical notes should store content hashes and extraction provenance, not
  vectors.

- [ ] Fuse scores in a testable `Ranker`.
  Start with:

  ```text
  final_score =
      lexical_score
    + semantic_score
    + entity_boost
    + source_boost
    + project_boost
    + confidence_boost
    + bounded_recency_boost
    - duplicate_penalty
  ```

- [ ] Add graceful degradation tests.
  If semantic config is missing, command fails, or vectors are stale, search and
  context must still work through lexical/entity retrieval.

- [ ] Keep semantic recall out of startup injection until prompt retrieval is
  stable.

**Done when:** semantic retrieval improves paraphrase recall in evals and can be
disabled without behavioral breakage.

## Phase 6: Add ADD-Only Memory Extraction

**Purpose:** Borrow Mem0's single-pass extraction while preserving Hive's review
and provenance model.

The extractor should create candidates. It must not rewrite or delete old
memory. Stale/conflicting memories are handled by retrieval ranking,
supersession metadata, and explicit curation.

This phase is not part of the retrieval MVP. Start it only after prompt recall
is useful enough that the next bottleneck is memory capture quality rather than
memory retrieval quality.

- [ ] Add an extraction candidate schema.
  Suggested canonical target: inbox note with `entry_kind = "note"` and
  `source_kind = "extractor"`, or a new proposal directory if review UX needs
  stronger separation.

  Candidate metadata should include:
  - extracted text
  - proposed scope
  - proposed kind
  - proposed project id
  - subject/tags
  - confidence
  - source refs
  - related existing memory ids
  - dedupe hash

- [ ] Add `hm extract`.
  Inputs:
  - explicit text
  - transcript/session file
  - recent inbox records
  - source ref
  - project/path hint

  Before adding a new command, evaluate whether this should be `hm inbox
  propose`, `hm note --source-kind extractor`, or an extension of `hm promote`.
  Avoid command-surface growth unless the workflow is materially clearer.

- [ ] Implement single-pass JSON extraction.
  The model should output only additive candidates. It may mention related ids
  for dedupe context, but it must not command UPDATE/DELETE.

- [ ] Retrieve related memories before extraction.
  Pass top related memories as dedupe context, mirroring Mem0's approach, but
  use them only to avoid duplicate candidates.

- [ ] Add hash-based exact dedupe for candidates.
  Use normalized text + project/scope/kind, not raw model output.

- [ ] Add `hm inbox promote --candidate` or extend `hm promote`.
  Candidate promotion should preserve source provenance and link back to the raw
  session/transcript/note.

- [ ] Add tests for poison resistance.
  Memory bodies and transcripts are data. They must be delimited and ignored as
  instructions in extraction prompts.

- [ ] Add a human-review default.
  Extracted candidates should land as raw/proposed records by default, not
  always-on memories. Promotion to injected memory requires explicit kind/scope
  classification and the same review/correction path as other memory.

**Done when:** Hive can propose durable facts from a session or note backlog
without auto-mutating existing memory.

## Phase 7: Add Supersession And Conflict Hygiene

**Purpose:** Make ADD-only memory safe over years.

Mem0's ADD-only model relies on retrieval surfacing current facts. Hive should
make that auditable.

This phase should follow real stale-memory pain, not anticipation. If exact
duplicate collapse and curated promotion solve the immediate issue, defer
canonical supersession metadata.

- [ ] Start with derived supersession candidates.
  Use retrieval/dedupe to report likely stale or conflicting memories in
  `hm doctor` / `hm search --explain` before adding canonical front matter.

- [ ] Add optional front matter fields only after candidate reporting proves
  useful:

  ```toml
  supersedes = ["old-memory-id"]
  superseded_by = ["new-memory-id"]
  contradicts = ["other-memory-id"]
  valid_from = "2026-06-01T00:00:00Z"
  valid_until = "2026-07-01T00:00:00Z"
  ```

- [ ] Add `hm supersede <new-id> <old-id>` and `hm contradict <a> <b>`.
  These commands should rewrite canonical metadata using the same safe rewrite
  path as `retag`.

  Before adding top-level commands, evaluate whether these belong under
  `hm retag`, `hm promote`, or `hm inbox` to avoid command sprawl.

- [ ] Add ranker rules:
  - demote superseded memories by default
  - keep superseded memories searchable with an explicit flag
  - prefer current valid time window
  - surface conflicts in `hm search --json` and `hm context --explain`

- [ ] Add `hm doctor` checks:
  - dangling supersession ids
  - cycles
  - bidirectional metadata drift
  - expired memories that still inject

**Done when:** ADD-only accumulation has an explicit, auditable correction model.

## Phase 8: Add MCP Facade

**Purpose:** Make Hive Memory available to MCP-native clients without losing
Hive's policy controls.

This is valuable only if there is a concrete client integration ready to use it.
Do not land an MCP server before the CLI/hook retrieval path is stable.

- [ ] Add `hm mcp serve`.
  Keep it as a thin adapter over existing command APIs.

- [ ] Start read-only unless write policy is fully specified.
  First MCP milestone should expose `search` and `context`. Add `remember`,
  `note`, `promote`, and `retag` only after write authentication/audience policy
  and tests are explicit.

- [ ] Expose read-only tools first:
  - `search`
  - `context`

- [ ] Add write tools only after write policy is explicit:
  - `remember`
  - `note`
  - `promote`
  - `retag`
  - `extract` when Phase 6 lands

- [ ] Use structured tool schemas.
  Tool inputs should expose stable enum keys, not prose-driven control flow.

- [ ] Enforce the same store affinity, sensitivity, audience, and secret policy
  as the CLI.

- [ ] Include provenance in MCP results.
  Agents should see enough metadata to cite source ids and avoid blindly
  rewriting memory.

- [ ] Add integration tests with a small MCP client harness.

**Done when:** Claude/Codex/Cursor/goose-style clients can share Hive Memory via
MCP while CLI and hooks remain first-class.

## Phase 9: Documentation And Migration

- [ ] Update `SPEC.md` for every schema or public command change.
- [ ] Update `README.md` with the memory-layer mapping:
  - Mem0 conversation memory -> agent runtime context, not Hive canonical store
  - Mem0 session memory -> `hm note`/future session proposals
  - Mem0 user memory -> global/person/rules curated memory
  - Mem0 org memory -> shared store/global curated memory
  - Hive project memory -> first-class project scope
- [ ] Update `src/README.md` module map.
- [ ] Add a migration note for cache deletion/rebuild.
- [ ] Add examples for:
  - prompt recall
  - extraction proposals
  - superseding stale facts
  - MCP client setup

## Phase 10: Final End-To-End Acceptance Gate

This phase is not optional. Do not call the Mem0-inspired integration complete
until these pass against the fully implemented system.

- [ ] Run the default suite:

  ```sh
  cargo test --locked
  ```

- [ ] Run retrieval and hook evals with visible output:

  ```sh
  cargo test --locked --test retrieval_eval -- --nocapture
  cargo test --locked --test injection_eval -- --nocapture
  cargo test --locked --test cli hook -- --nocapture
  ```

- [ ] Run performance budgets in release mode:

  ```sh
  HIVE_MEMORY_PERF_BUDGET_MULTIPLIER=4 \
    cargo test --release --test perf_budget -- --ignored --nocapture
  ```

- [ ] Run cloud sync simulation after schema/cache changes:

  ```sh
  cargo test --test cloud_sync_sim -- --ignored --nocapture
  ```

- [ ] Run a manual local hook smoke test from a real repo.
  Use the installed launcher path so agent/session env inference is exercised:

  ```sh
  HIVE_MEMORY_AGENT_ID=codex \
  HIVE_MEMORY_SESSION_ID=manual-hook-smoke \
  hm hook session-start --project /home/chris/git/hive-memory --json

  HIVE_MEMORY_AGENT_ID=codex \
  HIVE_MEMORY_SESSION_ID=manual-hook-smoke \
  hm hook prompt-submit \
    --project /home/chris/git/hive-memory \
    --text "what should I remember about Hive Memory hook retrieval?" \
    --json
  ```

- [ ] Verify CI includes the new performance tests.
  The existing `performance-budget` job already runs ignored release-mode
  budget tests; new prompt-recall tests should be part of that job.

- [ ] Verify generated/cached artifacts are disposable.
  Delete retrieval/entity/semantic cache directories and confirm:
  - `hm doctor` reports repairable cache state
  - `hm refresh --force --quiet` rebuilds what is needed
  - warm hook/search budgets pass after rebuild

**Done when:** correctness, lifecycle, performance, degradation, and cache
rebuild behavior are all exercised through the real CLI and hook interfaces.

## Suggested Landing Order

1. Retrieval eval harness.
2. Performance fixture scaffolding.
3. Tantivy lexical retrieval.
4. Prompt-aware hook recall.
5. Ship/evaluate lexical prompt recall in real use.
6. Deterministic entity extraction/linking only if evals show lexical gaps.
7. Supersession/conflict metadata only if stale-memory pain remains.
8. Optional semantic retrieval only if lexical+entity still misses useful cases.
9. ADD-only extraction proposals only if capture quality becomes the bottleneck.
10. MCP facade only when a concrete MCP client needs it.
11. Final end-to-end acceptance gate.

Semantic retrieval can move earlier if evals show lexical/entity recall is
insufficient, but do not add it before there is a benchmark proving the gap.

## Review Checklist

Every PR in this stream should answer:

- Does canonical memory remain plain, inspectable files?
- Can every new index/cache be deleted and rebuilt?
- Are secret and agent-private records excluded from model-backed workers?
- Does retrieval obey project, source, scope, audience, and store policy after
  any index-level filtering?
- Are model outputs strict JSON and treated as untrusted data?
- Does `hm context` re-read canonical files before agent injection?
- Does the retrieval eval improve or stay flat?
- Does this PR remove or defer any unproven complexity it no longer needs?
- Is the feature reversible by config or by deleting generated caches?
- Does the plan still solve a real observed workflow problem?
- Did `SPEC.md` change when public behavior or schema changed?
