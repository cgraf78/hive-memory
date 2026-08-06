# Scripts

These scripts support maintainer and evaluation workflows for Hive Memory.
Runtime behavior belongs in the Rust crate under `src/`.

## Release

`release-lib.sh`, `release-version.sh`, `release-tag.sh`, `package-release.sh`,
`smoke-release.sh`, and `release.sh` are **vendored verbatim** from
`cgraf78/actions` (`release-scripts/`), which is shared with `shdeps` and
`grafhome-ca`. Do not edit them here: the `Release script sync` CI job verifies
that the action lock, every literal `cgraf78/actions` ref, and the vendored
scripts all describe one reviewed actions commit.

To change shared behavior, edit it in `cgraf78/actions`, check out the reviewed
commit there, and run `consumer-ci/sync.sh <hive-memory-checkout>` from that
clean checkout. GitHub requires literal refs in workflow YAML, so the sync
command updates those generated copies, the authoritative lock, and the
vendored files together; hand-editing only a SHA or only a script would
recreate drift.

The sync also owns `.release-scripts.manifest`. That small generated list lets
CI notice when a formerly shared script was removed upstream without guessing
that repo-specific files such as `release.conf` should be deleted too.

Repo-owned pieces:

- `release.conf` declares the env namespace, archive naming, and the payload
  Hive Memory ships.
- `release-smoke-hook.sh` holds the runtime assertions that need to execute the
  packaged `hm`. It is skipped for cross-built `android-*` archives.

Keep scripts deterministic and friendly to CI. If a script needs a generated
artifact, make the artifact path explicit and avoid depending on untracked local
state. Payload changes should be covered by `tests/shell/release-scripts-test`;
the shared scripts are covered by `test/release-scripts-test` in `actions`.

## Public Evals

- `download-longmemeval-fixture` downloads the external LongMemEval-S JSON used
  by the ignored public retrieval eval.
