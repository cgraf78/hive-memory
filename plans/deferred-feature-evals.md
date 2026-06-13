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

## Capturing Real Misses

Use `hm eval capture-miss` when a prompt should have recalled a memory but did
not:

```console
hm eval capture-miss \
  --prompt "Where are coding agent rules documented?" \
  --expected alpha-agent-rules-checkrun \
  --project-id project-alpha \
  --to tests/fixtures/deferred_feature_eval_corpus.toml
```

Use `hm eval capture-bad-hit` when recall included an irrelevant memory:

```console
hm eval capture-bad-hit \
  --prompt "Cargo.toml release tags" \
  --bad beta-cargo-publish \
  --to tests/fixtures/deferred_feature_eval_corpus.toml
```

Without `--to`, both commands print a TOML snippet instead of writing a file.
Every captured case should be reviewed before it becomes a gate.
