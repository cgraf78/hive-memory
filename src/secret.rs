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
        let Some((_key, value)) = split_assignment(line) else {
            return false;
        };
        // Assignment-style detection is intentionally key-driven. Broad
        // entropy checks would catch too much ordinary technical prose and make
        // memory writes frustrating, while obvious credential keys give useful
        // protection without pretending to be a full DLP scanner.
        let key = line
            .split(['=', ':'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
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
}
