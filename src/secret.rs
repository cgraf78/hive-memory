//! Deterministic secret detectors.
//!
//! Hive Memory is durable by design, so write-time detection must be local,
//! cheap, and conservative. Findings expose detector IDs only; matched secret
//! values must never be returned to CLI errors, hook JSON, or doctor output.

use serde::Serialize;
use std::collections::BTreeSet;

/// One likely-secret detector hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecretFinding {
    /// Stable detector id suitable for CLI output and tests.
    pub detector_id: String,
}

/// Detect likely secrets in text without returning the matched values.
///
/// V1 intentionally favors obvious credentials over broad entropy matching to
/// avoid blocking normal technical notes. More detectors can be added behind
/// this API without changing the write-path privacy contract.
pub fn detect(text: &str) -> Vec<SecretFinding> {
    let mut detectors = BTreeSet::new();

    if looks_like_private_key(text) {
        detectors.insert("private-key");
    }
    if contains_aws_access_key_id(text) {
        detectors.insert("aws-access-key-id");
    }
    if contains_github_token(text) {
        detectors.insert("github-token");
    }
    if contains_assignment_secret(text) {
        detectors.insert("secret-assignment");
    }

    detectors
        .into_iter()
        .map(|detector_id| SecretFinding {
            detector_id: detector_id.to_owned(),
        })
        .collect()
}

fn looks_like_private_key(text: &str) -> bool {
    text.contains("-----BEGIN ") && text.contains("PRIVATE KEY-----")
}

fn contains_aws_access_key_id(text: &str) -> bool {
    tokens(text).any(|token| {
        token.len() == 20
            && (token.starts_with("AKIA") || token.starts_with("ASIA"))
            && token
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
    })
}

fn contains_github_token(text: &str) -> bool {
    tokens(text).any(|token| {
        ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
            .iter()
            .any(|prefix| token.starts_with(prefix))
            && token.len() >= 30
    })
}

fn contains_assignment_secret(text: &str) -> bool {
    text.lines().any(|line| {
        let Some((key, value)) = split_assignment(line) else {
            return false;
        };
        // Assignment-style detection is intentionally key-driven. Broad
        // entropy checks would catch too much ordinary technical prose and make
        // memory writes frustrating, while obvious credential keys give useful
        // protection without pretending to be a full DLP scanner.
        //
        // Derive both `key` and `value` from the same `split_assignment` so they
        // stay consistent. Recomputing `key` with a different split (e.g. on the
        // earliest of `:`/`=`) can pair a sensitive value with a non-sensitive
        // key prefix and miss the secret entirely.
        let key = key.to_ascii_lowercase();
        let sensitive_key = [
            "api_key",
            "api-key",
            "secret_key",
            "secret-key",
            "access_token",
            "access-token",
            "auth_token",
            "auth-token",
            "password",
        ]
        .iter()
        .any(|needle| key.contains(needle));
        sensitive_key && value_looks_real(value)
    })
}

fn split_assignment(line: &str) -> Option<(&str, &str)> {
    line.split_once('=').or_else(|| line.split_once(':'))
}

fn value_looks_real(value: &str) -> bool {
    let value = value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('`');
    if value.len() < 8 {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    ![
        "example",
        "placeholder",
        "changeme",
        "redacted",
        "<token>",
        "<secret>",
        "your_",
        "dummy",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.'))
        .filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(text: &str) -> Vec<String> {
        detect(text)
            .into_iter()
            .map(|finding| finding.detector_id)
            .collect()
    }

    #[test]
    fn detects_obvious_secret_assignments_without_values() {
        let findings = ids("api_key = \"localvalueforsecretdetection\"");
        assert_eq!(findings, vec!["secret-assignment"]);
    }

    #[test]
    fn ignores_placeholder_assignments() {
        assert!(ids("api_key = \"example-token\"").is_empty());
    }

    #[test]
    fn detects_private_keys_and_known_token_shapes() {
        assert_eq!(
            ids("-----BEGIN OPENSSH PRIVATE KEY-----\nbody"),
            vec!["private-key"]
        );
        assert_eq!(
            ids("token ghp_abcdefghijklmnopqrstuvwxyz1234567890"),
            vec!["github-token"]
        );
        let aws_key = ["AKIA", "ABCDEFGHIJKLMNOP"].concat();
        assert_eq!(ids(&aws_key), vec!["aws-access-key-id"]);
    }

    #[test]
    fn detects_secret_when_value_split_precedes_key_separator() {
        // Regression: `value` is derived from the `=` split, but `key` used to be
        // recomputed by splitting on the earliest of `:`/`=`. For this line the
        // value (`supersecret...`) is sensitive yet the recomputed key was only
        // `note`, so the finding was missed. Both must come from the same split.
        assert_eq!(
            ids("note: the password=supersecretvalue123"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_all_github_token_prefixes() {
        for prefix in ["gho_", "ghu_", "ghs_", "ghr_"] {
            // 4-char prefix + 36 chars comfortably clears the >= 30 length floor.
            let token = format!("token {prefix}abcdefghijklmnopqrstuvwxyz1234567890");
            assert_eq!(ids(&token), vec!["github-token"], "prefix {prefix}");
        }
    }

    #[test]
    fn detects_asia_aws_prefix() {
        // `ASIA` temporary credentials must be caught alongside `AKIA`.
        let aws_key = ["ASIA", "ABCDEFGHIJKLMNOP"].concat();
        assert_eq!(ids(&aws_key), vec!["aws-access-key-id"]);
    }

    #[test]
    fn returns_sorted_findings_for_multiple_secrets() {
        // BTreeSet ordering makes the detector-id set deterministic and sorted.
        let text =
            "-----BEGIN OPENSSH PRIVATE KEY-----\ntoken ghp_abcdefghijklmnopqrstuvwxyz1234567890";
        assert_eq!(ids(text), vec!["github-token", "private-key"]);
    }

    #[test]
    fn ignores_twenty_char_token_without_aws_prefix() {
        // Exactly 20 chars but no `AKIA`/`ASIA` prefix must not match, so ordinary
        // tokens in notes stay unblocked.
        let token = "ZZZZABCDEFGHIJKLMNOP";
        assert_eq!(token.len(), 20);
        assert!(ids(token).is_empty());
    }

    #[test]
    fn ignores_short_github_token() {
        // `ghp_` prefix but below the >= 30 length floor.
        assert!(ids("token ghp_short").is_empty());
    }

    #[test]
    fn ignores_assignment_with_short_value() {
        // Sensitive key but value under the 8-char realism floor.
        assert!(ids("api_key = short").is_empty());
    }

    #[test]
    fn ignores_assignment_with_redaction_markers() {
        // Sensitive key paired with values that signal an intentional non-secret.
        assert!(ids("api_key = thisisredacted").is_empty());
        assert!(ids("api_key = <token>").is_empty());
        assert!(ids("api_key = your_token_here").is_empty());
    }

    #[test]
    fn ignores_private_key_marker_missing_closing_delimiter() {
        // Needs both the opening `-----BEGIN ` and the `PRIVATE KEY-----` tail.
        assert!(ids("-----BEGIN OPENSSH PRIVATE KEY").is_empty());
    }
}
