# Deferred Feature Eval Gates

Hive Memory should only take on heavier Mem0-style mechanisms when they beat
the current lexical, project-scoped hook recall path on labeled workflows. The
deferred feature corpus in `tests/fixtures/deferred_feature_eval_corpus.toml`
is the scoreboard for that decision.

## Candidate Gates

- Semantic recall must improve paraphrase `recall@5` without adding forbidden
  hits or network calls to hooks.
- Supersession must suppress stale replaced facts while preserving an explicit
  audit trail and a manual recovery path.
- Entity handling must improve project/person/tool scoping without piercing
  existing project filters.
- Extraction must keep stable facts and reject speculation, secrets, and
  temporary task state.

## Metrics

The eval runner reports:

- `recall@5`
- `precision@5`
- MRR
- forbidden hit count
- p95 search latency

The initial baseline intentionally exposes known gaps. Follow-up feature
branches should update the runner with candidate implementations and only ship
when the candidate clears the gate that motivated it.
