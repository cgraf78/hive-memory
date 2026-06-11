//! Build-version reporting for the `hm` binary.
//!
//! The public version contract is intentionally small: report the same
//! generated version string used by release tags, archive names, and release
//! metadata, with the store schema appended for CLI support/debug output.

/// Git commit embedded into the binary at build time.
pub const COMMIT: &str = env!("HIVE_MEMORY_BUILD_COMMIT");

/// Public version embedded into the binary at build time.
///
/// The format is `YYYYMMDD-HHMMSS-<8hex>`. The timestamp makes release assets
/// human-sortable and easy to inspect, while the hash suffix preserves the
/// concrete commit identity.
pub const VERSION: &str = env!("HIVE_MEMORY_BUILD_VERSION");

/// Full `hm --version` payload without the leading binary name.
pub const CLI_VERSION: &str = env!("HM_CLI_VERSION");

/// Returns the embedded git commit hash.
#[must_use]
pub const fn commit() -> &'static str {
    COMMIT
}

/// Returns the public generated version string.
#[must_use]
pub const fn version() -> &'static str {
    VERSION
}

/// Returns the full CLI version payload without the leading binary name.
#[must_use]
pub const fn cli() -> &'static str {
    CLI_VERSION
}

#[cfg(test)]
mod tests {
    use super::{cli, commit, version};

    #[test]
    fn embedded_commit_is_concrete() {
        let commit = commit();

        assert!(commit.len() >= 8);
        assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(commit, "unknown");
    }

    #[test]
    fn public_version_is_readable_and_traceable() {
        let version = version();
        let parts = version.split('-').collect::<Vec<_>>();

        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 6);
        assert_eq!(parts[2].len(), 8);
        assert!(parts[0].bytes().all(|byte| byte.is_ascii_digit()));
        assert!(parts[1].bytes().all(|byte| byte.is_ascii_digit()));
        assert!(parts[2].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(parts[2], &commit()[..8]);
        assert_ne!(version, "unknown");
    }

    #[test]
    fn cli_version_includes_public_version_and_schema() {
        assert!(cli().starts_with(version()));
        assert!(cli().contains("schema 1"));
    }
}
