# Memory Improvements Roadmap

Goal: make Hive Memory's behavior as seamless, automatic, and useful as
possible, with every shipped change backed by hard data against the existing
eval harness.

This roadmap follows the project's own rule from `deferred-feature-evals.md`:
heavier mechanisms only ship when they beat the current path on labeled
workflows. It records the baselines measured on 2026-06-14, the metric
philosophy we optimize and gate against, and a ranked list of experiments.

## Metric Philosophy

- **End-to-end answer accuracy is the release gate.** "Useful" ultimately means
  the agent recalled the memory it needed to answer correctly. This is the
  headline metric for shipping a major change.
- **Deterministic proxies steer day-to-day iteration.** `recall@5`, `MRR`,
  injection `precision`, forbidden/non-answer hits, and `p95` are cheap,
  reproducible, and CI-friendly. End-to-end answer grading is nondeterministic,
  slow, and token-costly, so it is a periodic gate, not an inner-loop signal.
- **Validate the proxy once.** Before trusting `recall@5` of the answer session
  as a stand-in for answer correctness, build a real QA grader and confirm the
  two correlate (see Experiment B).
- **Every recall change is paired with a precision/token guardrail.** The
  cheapest way to win recall is to stuff context, which destroys token cost and
  the "seamless" feel. No recall improvement ships without showing injection
  precision and injected-token count did not regress.
- **Automatic capture is staging plus gated promotion, never direct-to-canonical.**
  The SPEC's "hooks must not auto-write canonical memory" invariant protects
  clean canonical memory, not automation as such. Auto-capture may write to the
  low-trust inbox only; promotion to durable/curated memory is what an eval-gated
  classifier and mem0-style ADD/UPDATE/DELETE control.

## Baselines (2026-06-14)

All numbers from `cargo test` on the committed harnesses and the LongMemEval-S
cleaned fixture (100-case smoke subset unless noted). Reproduce with the
commands in `tests/public_evals/README.md` and `plans/deferred-feature-evals.md`.

### LongMemEval-S retrieval (lexical, session ingest)

| segment | recall@5 | precision@5 | MRR | non-answer hits | p95 |
| --- | --- | --- | --- | --- | --- |
| overall | 0.781 | 0.535 | 0.815 | 156 | 321ms |
| multi-session | 0.571 | 0.553 | 0.833 | 52 | 267ms |
| single-session-user | 0.871 | 0.527 | 0.808 | 104 | 331ms |

### Memory decomposition granularity (LongMemEval-S, overall / multi-session)

| ingest mode | recall@5 | multi-session recall@5 | precision@5 | p95 |
| --- | --- | --- | --- | --- |
| session (baseline) | 0.781 | 0.571 | 0.535 | 321ms |
| exchange | 0.643 | 0.276 | 0.518 | 115ms |
| turn | 0.600 | 0.232 | 0.470 | 120ms |

Negative result: finer decomposition badly hurts recall, especially
multi-session (0.571 -> 0.23). Storing atomic fragments is the wrong lever under
lexical retrieval; coarser records retrieve better. Multi-session recall needs
better retrieval (semantic/hybrid), not finer chunks.

### Injection precision (injection_eval corpus)

| strategy | precision | recall | true pos | false pos |
| --- | --- | --- | --- | --- |
| recency (default) | 0.500 | 1.000 | 19 | 19 |
| relevance (with kind) | 1.000 | 1.000 | 19 | 0 |

The relevance strategy roughly doubles injection precision at no recall cost on
labeled data, but it is off by default and is conservative on UNTAGGED records
(it routes ambiguous global text to search-only), so its recall on real
untagged stores is unproven. See Experiment C.

### Deferred-feature retrieval scoreboard

| case type | no-entity recall@5 | entity-linked recall@5 |
| --- | --- | --- |
| entity | 0.417 | 1.000 |
| semantic | 0.750 | 1.000 |
| scope | 1.000 | 1.000 |

Entity linking already earns its keep. The headroom is paraphrase/semantic and
multi-session recall.

## Where the Headroom Is

1. **Multi-session recall (0.571)** is the single largest, most valuable gap and
   is unreachable by decomposition. Needs semantic/hybrid retrieval and better
   query construction. (Experiment D.)
2. **Injection precision (~0.5 under the default strategy)** is context noise the
   agent pays for on every turn. The fix mostly exists but is unproven on
   untagged stores and off by default. (Experiment C.)
3. **Automatic capture** is the biggest "seamless/automatic" lever and is unbuilt
   by design; it must arrive as staging plus gated promotion. (Experiment E.)

## Ranked Experiments

Each experiment states a hypothesis, the gate it must clear, the method, and
effort/risk. They are ordered by leverage-per-effort given the baselines.

### Experiment A — Injected-token guardrail metric (infrastructure)

- **Hypothesis:** we cannot safely tune recall without a standing precision/token
  guardrail; adding injected-token accounting to the eval harness makes
  context-stuffing regressions visible.
- **Gate:** none (enabling infrastructure). Becomes the guardrail every later
  experiment reports.
- **Method:** extend the injection/LongMemEval harnesses to report injected
  token count and non-answer token share alongside precision.
- **Effort/risk:** low / low. No new dependencies.

### Experiment B — End-to-end QA grader and proxy validation

- **Hypothesis:** `recall@5` of the labeled answer session correlates with actual
  answer correctness, so the cheap proxy is trustworthy for iteration.
- **Gate:** correlation high enough to rely on the proxy; otherwise iteration
  must use the grader more often.
- **Method:** add an opt-in harness that answers LongMemEval questions from the
  retrieved context using the existing local LLM backend abstraction
  (`src/llm.rs`) plus an LLM judge, and reports answer accuracy. Run once across
  the corpus; compare against retrieval recall. Keep out of CI.
- **Effort/risk:** medium / medium (nondeterministic, needs a backend on PATH).

### Experiment C — Relevance inject strategy: default and guardrail

- **Hypothesis:** the relevance strategy's precision win generalizes without a
  material recall loss once paired with the token guardrail; it should be the
  default or an adaptive strategy.
- **Gate:** injection precision up and injected tokens down, with `recall@5`
  non-regression on an UNTAGGED corpus (not just the tagged injection corpus).
- **Method:** add an untagged variant to the injection eval; measure relevance
  vs recency recall on it; if recall drops, design an adaptive strategy (e.g.,
  relevance when records carry `kind`, recency fallback otherwise).
- **Effort/risk:** low-medium / medium (changing a default is a behavior change
  with SPEC implications).

### Experiment D — Hybrid/semantic retrieval for multi-session recall

- **Hypothesis:** local full-text (BM25 via Tantivy) plus optional local
  embeddings with RRF fusion lifts multi-session and paraphrase recall@5 over the
  lexical baseline while staying within p95 budgets and adding no network calls
  to hooks.
- **Gate (from `deferred-feature-evals.md`):** paraphrase/multi-session recall@5
  improves with zero new forbidden hits and no hook network calls; p95 stays
  within the budgets in `plans/full-text-retrieval.md`.
- **Method:** implement the `plans/full-text-retrieval.md` design in phases
  (document model -> Tantivy backend -> search/context integration), measuring
  recall at each phase. Add embeddings + RRF only if BM25 alone leaves a gap and
  it clears the gate. Canonical files stay plain; the index is a rebuildable
  cache.
- **Effort/risk:** high / medium. Largest single body of work; multiple PRs.

### Experiment E — Auto-capture to staging plus gated promotion

- **Hypothesis:** auto-extracting facts to the inbox and promoting via mem0-style
  ADD/UPDATE/DELETE/NOOP improves end-to-end accuracy and reduces the user's
  manual `hm remember` burden, without polluting canonical memory.
- **Gate:** extraction precision/recall clears the deferred extraction eval
  (keeps stable facts; rejects speculation, secrets, transient task state); the
  promotion step improves end-to-end answer accuracy without an injection
  precision loss. Default off.
- **Method:** add an opt-in capture path that writes inbox notes only; add a
  promotion command that retrieves top-k similar durable memories and applies an
  LLM-chosen ADD/UPDATE/DELETE/NOOP, reusing supersession for the
  invalidate-don't-delete path. Reuse `src/llm.rs`; never send secret or
  agent-private content to a backend.
- **Effort/risk:** high / high. Behavior, safety, and noise sensitive.

## Out of Scope (for this roadmap)

- Hosted-service patterns (Neo4j graph store, Postgres server, memory router
  proxy). They trade away the local/plain-files/vendor-neutral properties.
- At-rest encryption (SPEC defers to v2).
- Making any index canonical; canonical memory stays plain files.

## Tracking

Experiment status is tracked as code lands. Each shipped experiment updates its
baseline table above with the new numbers and a one-line verdict (shipped /
rejected with data). Negative results are kept — the decomposition result above
is the first.
