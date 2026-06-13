# Public Memory Evals

These tests adapt public memory benchmarks to Hive Memory without committing
the external datasets. They are ignored by default because they download or
consume larger corpora and are meant for local comparison runs, not every unit
test cycle.

## LongMemEval-S

Download the cleaned LongMemEval-S fixture:

```console
scripts/download-longmemeval-fixture
```

Run the retrieval-only eval:

```console
HIVE_MEMORY_LONGMEMEVAL_S_JSON=target/public-evals/longmemeval_s_cleaned.json \
  cargo test --test public_longmemeval -- --ignored --nocapture
```

The adapter ingests unique haystack sessions as remembered records, restricts
each query to the benchmark-provided haystack session ids, and scores whether
Hive Memory retrieves the labeled answer sessions. It reports:

- recall@5
- precision@5
- MRR
- non-answer hits
- p95 latency

Set `HIVE_MEMORY_LONGMEMEVAL_MAX_CASES=0` to run all cases. The default is a
100-case smoke subset so local iteration stays quick.
