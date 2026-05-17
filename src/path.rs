//! Store-relative path normalization.
//!
//! Canonical memory is plain files, but paths still cross operating systems,
//! filesystems, and sync tools. This module keeps the v1 metadata spelling in
//! one place so notes, events, indexes, and generated context do not each invent
//! their own string form. The normalized form is a metadata contract, not a
//! filesystem lookup contract: forward slashes, NFC Unicode, and lowercase
//! spelling only when the store is known to be case-insensitive.

use std::path::{Component, Path};
use unicode_normalization::UnicodeNormalization;

/// Case behavior for metadata path normalization.
///
/// The value is resolved once by callers that know the store root and then
/// passed through lower-level APIs. That keeps filesystem probing out of hot
/// loops and makes tests explicit about the path contract they are exercising.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathCase {
    /// Preserve path component case.
    Sensitive,
    /// Lowercase path components before comparing or serializing metadata.
    Insensitive,
}

/// Resolve `[storage].case_sensitive` into concrete path behavior.
///
/// `auto` performs a cheap probe inside the store root when possible. If the
/// root is unavailable, fall back to the platform default so offline reads do
/// not fail just because a cloud mount is temporarily missing.
pub fn resolve_case(mode: &str, root: &Path) -> PathCase {
    match mode {
        "false" => PathCase::Insensitive,
        "true" => PathCase::Sensitive,
        _ => detect_case(root).unwrap_or_else(platform_default_case),
    }
}

/// Return a normalized store-relative string for a filesystem path.
///
/// Only normal components are preserved. Callers that accept user-supplied
/// paths should validate absolutes, `..`, and prefixes before using this helper;
/// writer/index paths are already rooted under the store. This API intentionally
/// does not touch the filesystem because generated metadata must be stable even
/// when a store is offline or represented by a staged outbox payload.
pub fn relative_string(path: &Path, case: PathCase) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .map(|component| normalize_component(component, case))
        .collect::<Vec<_>>()
        .join("/")
}

/// Normalize an already-serialized relative path for comparison.
///
/// Event validation uses this to reject alternate spellings at write time. That
/// keeps canonical JSONL metadata deterministic instead of relying on readers to
/// accept every possible Unicode or separator variant forever.
pub fn relative_str(value: &str, case: PathCase) -> String {
    value
        .replace('\\', "/")
        .split('/')
        .filter(|component| !component.is_empty())
        .map(|component| normalize_component(component, case))
        .collect::<Vec<_>>()
        .join("/")
}

fn normalize_component(component: &str, case: PathCase) -> String {
    let normalized = component.nfc().collect::<String>();
    match case {
        PathCase::Sensitive => normalized,
        PathCase::Insensitive => normalized.to_lowercase(),
    }
}

fn detect_case(root: &Path) -> Option<PathCase> {
    if !root.is_dir() {
        return None;
    }
    // Probe with a disposable mixed-case filename inside the actual store root:
    // mount-level behavior is what matters, not the OS default for some other
    // path. A failed probe is non-fatal because `auto` should not make read-only
    // or temporarily unavailable stores unusable.
    let token = format!(".hm-case-probe-{}-A", std::process::id());
    let probe = root.join(&token);
    let lower = root.join(token.to_ascii_lowercase());
    std::fs::write(&probe, b"").ok()?;
    let insensitive = lower.exists() && lower != probe;
    let _ = std::fs::remove_file(&probe);
    Some(if insensitive {
        PathCase::Insensitive
    } else {
        PathCase::Sensitive
    })
}

fn platform_default_case() -> PathCase {
    if cfg!(windows) {
        PathCase::Insensitive
    } else {
        PathCase::Sensitive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn relative_string_uses_forward_slashes_and_nfc() {
        let decomposed = "Cafe\u{301}.md";
        let path = PathBuf::from("inbox").join("notes").join(decomposed);

        let normalized = relative_string(&path, PathCase::Sensitive);

        assert_eq!(normalized, "inbox/notes/Café.md");
    }

    #[test]
    fn relative_str_lowercases_when_case_insensitive() {
        let normalized = relative_str(r"Inbox\Notes\CAFÉ.md", PathCase::Insensitive);

        assert_eq!(normalized, "inbox/notes/café.md");
    }
}
