# BM25 Retrieval Experiment

Status: experiment complete, recommends product integration.

## Question

Multi-session recall is the largest retrieval gap (lexical `recall@5` 0.571 on
LongMemEval-S). Does statistical term weighting (Okapi BM25) beat the shipped
lexical coverage heuristic enough to justify a real full-text engine (Tantivy)
as a product dependency?

## Method

A dependency-free BM25 ranker (k1=1.2, b=0.75) was added to the LongMemEval-S
harness as a selectable retriever (`HIVE_MEMORY_LONGMEMEVAL_RETRIEVER=bm25`),
ranking the same per-case haystack items the lexical path sees and scored
identically at the session level. This isolates the ranking-algorithm question
from the engine question: if hand-rolled BM25 wins, Tantivy (BM25 plus better
tokenization and reranking) will do at least as well.

Reproduce:

```console
scripts/download-longmemeval-fixture
HIVE_MEMORY_LONGMEMEVAL_S_JSON=target/public-evals/longmemeval_s_cleaned.json \
  HIVE_MEMORY_LONGMEMEVAL_RETRIEVER=bm25 \
  cargo test --test public_longmemeval -- --ignored --nocapture
```

## Result (100-case smoke, session ingest)

| segment | lexical recall@5 | BM25 recall@5 | lexical MRR | BM25 MRR | lexical p95 | BM25 p95 |
| --- | --- | --- | --- | --- | --- | --- |
| overall | 0.781 | 0.904 | 0.815 | 0.920 | 321ms | 107ms |
| multi-session | 0.571 | 0.714 | 0.833 | 0.869 | 267ms | 110ms |
| single-session-user | 0.871 | 0.986 | 0.808 | 0.942 | 331ms | 107ms |

Precision@5 dropped (overall 0.535 -> 0.280; non-answer hits 156 -> 360).

## Reading

- BM25 is a decisive recall and ranking win: +0.12 overall recall, +0.14 on the
  hardest multi-session segment, MRR ~0.92 (the answer session lands at rank 1-2
  almost always), at roughly one third the latency.
- The precision drop is largely an artifact of always returning five sessions
  when most questions have a single answer session. With MRR ~0.92 the correct
  session is nearly always on top, so a rank cutoff or reranking recovers
  precision without losing the recall gain. This is what the injected-token /
  precision guardrail is for.
- The win holds without any network calls and well within the p95 budget
  (110ms vs the 500ms hook budget), clearing the `deferred-feature-evals.md`
  gate for semantic/statistical recall.

## Recommendation

Promote BM25 retrieval to the product via the `plans/full-text-retrieval.md`
Tantivy design, in gated phases:

1. Document model + Tantivy index backend (rebuildable cache; canonical files
   stay plain).
2. Route `hm search` and `hm context` candidate generation through it, keeping
   `hm` policy post-filtering and reranking.
3. Add a precision-preserving rank cutoff / rerank and report injected-token
   cost so the recall gain does not regress precision in shipped context.

This experiment harness stays as a regression scoreboard: the lexical path is
unchanged and remains the default retriever, so existing CI behavior is
untouched.
