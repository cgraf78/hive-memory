# Automatic recall quality

Automatic injection must carry memories supported by the current task, including
short continuations, rather than advancing through unrelated unseen search hits.
Ordinary interactive search, startup selection, security filters, and delivery
lifetimes keep their existing contracts.

## Design

- Keep bounded meaningful terms from the prompt instead of the first two plain
  words. Reuse search-owned normalization and lexical/entity matching.
- Gate automatic candidates on topic evidence before session deduplication and
  the three-result cap. Retrieval and project ranking alone do not establish
  sufficient evidence. An empty useful result is valid.
- Store one bounded normalized topic, bound to its store and project, in local
  hook state. Generic continuations can reuse it; explicit subjects replace it.
  No raw conversation is persisted. Context resets clear the topic, and updates
  use the existing state lock and epoch guard.
- Apply the same selection policy to untracked, persistent, and turn-lifetime
  hooks without changing which delivered memories remain visible.
- Gate deferred startup context as well as recall at prompt time; unrelated
  startup overflow must not drain into later turns. Keep normal session-start
  selection broad, and leave withheld sections eligible for a later matching
  topic instead of marking the entire base selection complete.

## Implementation and verification

1. Freeze baseline executables and a private corpus/prompt replay snapshot.
   Label actual sessions independently before candidate results are available.
2. Add failing real-CLI regressions for late subjects, weak novel hits,
   continuations, topic changes, lifecycle boundaries, aliases, and mandatory
   policy filters.
3. Keep evidence semantics in `src/search.rs`, topic state in `src/hook.rs`, and
   hook orchestration in `src/cli/hook.rs`. Update the normative hook contract.
4. Replay the same real sessions against installed, latest-main, and candidate
   binaries. Report useful, irrelevant, uncertain, and unlabeled outputs,
   positive-case recall, abstentions, output bytes, and latency separately.
5. Require fewer clearly irrelevant injections, preservation of useful positive
   controls, no forbidden hits, and the existing hook latency budgets. Keep
   held-out results distinct from development cases and disclose coverage limits.
6. Run the full Rust and shell validation, independent review, exact-head CI,
   release verification, and installed-consumer checks before claiming rollout.

The private corpus and raw prompts must never enter the public repository. No
canonical memory content, scopes, classifications, or classifier backlog is
changed by this work.

## Measured result

The frozen replay used 428 current notes and 37 real prompts from five sessions,
with 114 reviewed candidate notes per prompt. Labels were frozen before any
candidate output. Twelve prompt cases were concealed during tuning and disclosed
after policy freeze; they share session history with development cases and are
not independent held-out sessions. Five alternating release-binary passes give
185 latency samples per arm, while quality still has only 37 unique cases.

| Prompt-time measurement | Latest-main baseline | Candidate |
| --- | ---: | ---: |
| Labeled irrelevant deliveries | 127 | 17 |
| Labeled useful deliveries | 11 | 10 |
| Unlabeled deliveries | 9 | 6 |
| Startup-overflow deliveries | 57 | 0 |
| Context bytes | 119,464-119,472 | 45,205 |
| Cases with some useful context visible | 24/25 | 24/25 |
| Correct abstention on no-recall controls | 2/11 | 7/11 |
| CLI process latency p50 / p95 | 11.3 / 16.2 ms | 24.3 / 34.2 ms |

The candidate reduces labeled irrelevant deliveries by 86.6% and context bytes
by 62.2%. The concealed cases improve from 54 irrelevant deliveries to seven,
with useful context available in all five cases that have labeled useful notes.
On the fixed four baseline cases needing new useful context, availability
improves from three to four.

These aggregate gains do not mean that every useful note was preserved. Exact
baseline useful-note retention is 5/11; alternative useful notes are often
already visible or selected instead. In particular, the initial fleet-health
request loses a specific prior health-probe baseline despite keeping general
hardware/access priors. An adjacent interface note is also missed. Residual
errors include generic recommendations, login-flow follow-ups, and CI-status
questions receiving neighboring or unrelated infrastructure notes. These are
known retrieval limits, not claimed fixes.

The replay evaluates prompt delivery, not startup precision, and uses the
current frozen projection rather than reconstructing each historical store.
Unknown results remain unlabeled. Actual compaction and tool-write sequences
are covered by public regression tests rather than this persistent Claude/Codex
replay. Manual search, canonical note contents, and classifier state are unchanged.

All 824 default Rust tests, 13 strict release performance tests with a 5,000-note
fixture, four cloud-sync simulations, and 59 shell assertions pass. Formatting,
Clippy with warnings denied, documentation with missing docs denied, Checkrun
lint, and the pinned ShellCheck inventory pass. Cargo Audit is unavailable on the
local host and remains an authoritative CI gate.

The opt-in `automatic_recall_eval` test accepts `HM_RECALL_EVAL_DATASET`,
`HM_RECALL_EVAL_OUTPUT`, and JSON `HM_RECALL_EVAL_ARMS` containing binary names and
paths. `HM_RECALL_EVAL_REPEATS=5` alternates arm order; disclosure defaults to
development-only, and `HM_RECALL_EVAL_DISCLOSE=all` reveals concealed prompt cases.
The dataset, freeze manifest, corpus, and output belong in one external
owner-private directory. The test verifies their hashes and each materialized
note's body/policy metadata, sandboxes every arm, and fingerprints executable
bytes in the report. No private dataset is published with this change.
