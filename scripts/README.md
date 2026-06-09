# Release Scripts

These scripts support maintainer release work for Hive Memory. Runtime behavior
belongs in the Rust crate under `src/`.

- `cargo-version.sh` reads the crate version used for packaging.
- `package-release.sh` builds release artifacts.
- `smoke-release.sh` validates a packaged release.
- `release.sh` composes the local release flow.

Keep packaging assumptions explicit here and mirror any release archive shape
changes in CI or smoke tests.
