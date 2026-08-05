# Scripts

These scripts support maintainer and evaluation workflows for Hive Memory.
Runtime behavior belongs in the Rust crate under `src/`.

## Release

`release-lib.sh`, `release-version.sh`, `release-tag.sh`, `package-release.sh`,
`smoke-release.sh`, and `release.sh` are **vendored verbatim** from
`cgraf78/actions` (`release-scripts/`), which is shared with `shdeps` and
`grafhome-ca`. Do not edit them here: the `Vendored release scripts` CI job
fails on any divergence.

To change shared behavior, edit it in `cgraf78/actions`, then bump the pinned
SHA in `.github/workflows/ci.yml` and run `release-scripts/sync.sh` from an
`actions` checkout in the same commit.

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
