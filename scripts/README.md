# Release Scripts

These scripts support maintainer release work for Hive Memory. Runtime behavior
belongs in the Rust crate under `src/`.

- `release-version.sh` computes the release version from repo state.
- `release-tag.sh` creates or validates the tag used for a release.
- `package-release.sh` builds release artifacts.
- `smoke-release.sh` validates a packaged release.
- `release.sh` composes the local release flow.

Keep scripts deterministic and friendly to CI. If a script needs a generated
artifact, make the artifact path explicit and avoid depending on untracked local
state. Release archive shape changes should be covered by
`tests/shell/release-scripts-test`.
