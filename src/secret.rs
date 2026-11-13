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
        let lower = line.to_ascii_lowercase();
        if !sensitive_name(&lower) {
            return false;
        }
        // A placeholder anywhere in a value suffix masks that suffix under the
        // existing policy. Cache its last position instead of rescanning the
        // whole remaining line for each assignment in a pasted config snippet.
        let last_placeholder = PLACEHOLDERS
            .iter()
            .filter_map(|word| lower.rfind(word))
            .max();
        let value_end = line
            .trim_end()
            .trim_end_matches('"')
            .trim_end_matches('\'')
            .trim_end_matches('`')
            .len();
        // Malformed brackets/quotes can make adjacent-key scans overlap. Bound
        // total work, not input length, and fail conservatively on exhaustion:
        // dropping the rest of the line could hide a later real credential.
        let mut work = line.len().saturating_mul(8);
        line.match_indices(['=', ':']).any(|(offset, _)| {
            let key = assignment_key(&line[..offset], &mut work);
            if work == 0 {
                return true;
            }
            let Some(key) = key else { return false };
            if !spend(&mut work, key.len()) {
                return true;
            }
            // Keep every target paired with its own value, including a real
            // credential after an earlier prose label or placeholder.
            sensitive_name(&key.to_ascii_lowercase())
                && value_looks_real(line, offset + 1, value_end, last_placeholder)
        })
    })
}

fn sensitive_name(lower: &str) -> bool {
    [
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
    .any(|needle| lower.contains(needle))
}

/// Charge overlapping lexical work against a per-line budget.
fn spend(work: &mut usize, bytes: usize) -> bool {
    *work = work.saturating_sub(bytes);
    *work != 0
}

/// Read the lexical assignment target immediately before a separator.
///
/// Stop at prose boundaries rather than borrowing a distant credential name.
/// Keep sigils, formatting, and indexing because legitimate assignments also
/// appear in shell arguments, PowerShell/Ruby snippets, and structured config.
/// Single/double-quoted config keys may contain punctuation; backticks instead
/// mark code identifiers, so a backticked file path remains a prose label.
fn assignment_key<'a>(prefix: &'a str, work: &mut usize) -> Option<&'a str> {
    let trimmed = prefix.trim_end();
    if !spend(work, prefix.len() - trimmed.len()) {
        return None;
    }
    let prefix = trimmed;
    let mut opening = prefix.len();
    let mut quote = None;
    let mut brackets = Vec::new();
    for (offset, ch) in prefix.char_indices().rev() {
        if !spend(work, ch.len_utf8()) {
            return None;
        }
        if let Some(delimiter) = quote {
            if delimiter == '`' && matches!(ch, '/' | '\\') {
                return None;
            }
            opening = offset;
            if ch == delimiter {
                let escapes = prefix[..offset]
                    .bytes()
                    .rev()
                    .take_while(|byte| *byte == b'\\')
                    .count();
                if !spend(work, escapes) {
                    return None;
                }
                if escapes % 2 == 0 {
                    quote = None;
                }
            }
            continue;
        }
        if matches!(ch, '\'' | '"' | '`') {
            // An opening quote around an entire shell argument is a boundary
            // for a bare target, not an unmatched quoted configuration key.
            if !spend(work, prefix.len() - opening) {
                return None;
            }
            let formatting_only = prefix[opening..]
                .chars()
                .all(|ch| matches!(ch, '*' | '_' | '~'));
            if brackets.is_empty() && opening != prefix.len() && !formatting_only {
                break;
            }
            quote = Some(ch);
            opening = offset;
            continue;
        }
        match ch {
            ']' | ')' | '}' => brackets.push(ch),
            '[' | '(' | '{' => {
                let Some(closing) = brackets.pop() else {
                    break;
                };
                if !matches!((ch, closing), ('[', ']') | ('(', ')') | ('{', '}')) {
                    return None;
                }
            }
            _ if !brackets.is_empty() => {}
            _ if target_character(ch) => {}
            _ if ch.is_whitespace() && prefix[opening..].starts_with('[') => {}
            // A compound assignment can place punctuation between the name
            // and '='. Retain the old guard's conservative treatment without
            // maintaining a language-specific list of assignment operators.
            _ if opening == prefix.len() && !matches!(ch, '/' | '\\' | ':' | '=') => continue,
            _ => break,
        }
        opening = offset;
    }
    if quote.is_some() || !brackets.is_empty() || opening == prefix.len() {
        return None;
    }
    let before = &prefix[..opening];
    // Other punctuation can delimit assignments (for example URL query
    // parameters). Only a continued bare path disqualifies the extracted name.
    if before.ends_with(['/', '\\']) {
        return None;
    }
    Some(&prefix[opening..])
}

fn target_character(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '$' | '@' | '%' | '*' | '~')
}

const PLACEHOLDERS: &[&str] = &[
    "example",
    "placeholder",
    "changeme",
    "redacted",
    "<token>",
    "<secret>",
    "your_",
    "dummy",
];

/// Apply the existing trimmed-value policy using cached suffix information.
fn value_looks_real(
    line: &str,
    offset: usize,
    value_end: usize,
    last_placeholder: Option<usize>,
) -> bool {
    let value = line[offset..]
        .trim_start()
        .trim_start_matches('"')
        .trim_start_matches('\'')
        .trim_start_matches('`');
    let value_start = line.len() - value.len();
    value_end.saturating_sub(value_start) >= 8
        && last_placeholder.is_none_or(|position| position < value_start)
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
    fn detects_inline_assignment_after_prose_label() {
        // The earlier prose label must not hide the credential's own key and
        // value, even when the assignment is embedded in an ordinary sentence.
        assert_eq!(
            ids("note: the password=supersecretvalue123"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn ignores_password_reference_before_unrelated_prose_colon() {
        assert!(
            ids(
                "Keep the service password in the protected credential file. Recovery procedure: \
             retrieve the escrow copy through the approved maintenance process."
            )
            .is_empty()
        );
    }

    #[test]
    fn ignores_password_reference_before_unrelated_assignment() {
        assert!(ids(
            "The service-password.env file is escrowed separately; retention_days = thirty days."
        )
        .is_empty());
    }

    #[test]
    fn ignores_password_file_path_used_as_prose_label() {
        assert!(ids("/vault/service-password.env: keep the recovery copy current.").is_empty());
    }

    #[test]
    fn ignores_code_quoted_password_file_path_used_as_prose_label() {
        assert!(ids("`/vault/service-password.env`: keep the recovery copy current.").is_empty());
    }

    #[test]
    fn detects_later_assignment_after_harmless_assignment() {
        assert_eq!(
            ids("note: username=service-user; password=syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_later_assignment_after_placeholder() {
        assert_eq!(
            ids("api_key=placeholder; password=syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_quoted_json_key_after_another_property() {
        assert_eq!(
            ids(r#"{"username":"service-user","password":"syntheticcredentialvalue"}"#),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_quoted_key_with_spaces() {
        assert_eq!(
            ids("'database password': syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_indexed_credential_assignment() {
        assert_eq!(
            ids(r#"config["password"] = "syntheticcredentialvalue""#),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_environment_credential_with_case_and_suffix() {
        assert_eq!(
            ids("export SERVICE_PASSWORD_VALUE=syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_credential_whose_value_looks_like_a_file_path() {
        assert_eq!(
            ids("api_key = /vault/service-credential.env"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_sigil_prefixed_password_assignment() {
        assert_eq!(
            ids("$password = syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_sigil_prefixed_credential_property() {
        assert_eq!(
            ids("$obj.password=syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_instance_variable_password_assignment() {
        assert_eq!(
            ids("@password = syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_braced_password_variable() {
        assert_eq!(
            ids("${password} = syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_numerically_indexed_password_assignment() {
        assert_eq!(
            ids("password[0]=syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_nested_password_indexing() {
        assert_eq!(
            ids("password[0][1]=syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_password_indexing_after_whitespace() {
        assert_eq!(
            ids("password [0]=syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_bold_password_label() {
        assert_eq!(
            ids("**password**: syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_path_punctuation_in_quoted_config_key() {
        assert_eq!(
            ids(r#"{"service/password":"syntheticcredentialvalue"}"#),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_colon_in_quoted_config_key() {
        assert_eq!(
            ids(r#"{"password:slot":"syntheticcredentialvalue"}"#),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_sigil_in_quoted_config_key() {
        assert_eq!(
            ids(r#"{"service@password":"syntheticcredentialvalue"}"#),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_punctuation_in_indexed_config_key() {
        assert_eq!(
            ids(r#"config["service/password"]="syntheticcredentialvalue""#),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_escaped_quote_in_credential_key() {
        assert_eq!(
            ids(r#"'service\'s password': syntheticcredentialvalue"#),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_assignment_inside_a_quoted_shell_argument() {
        assert_eq!(
            ids(r#"echo "password=syntheticcredentialvalue""#),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_credential_in_first_url_query_parameter() {
        assert_eq!(
            ids("https://service.invalid/?api_key=syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_credential_in_later_url_query_parameter() {
        assert_eq!(
            ids("https://service.invalid/?mode=read&password=syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_conditional_credential_assignment() {
        assert_eq!(
            ids("PASSWORD ?= syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn detects_append_credential_assignment() {
        assert_eq!(
            ids("PASSWORD+=syntheticcredentialvalue"),
            vec!["secret-assignment"]
        );
    }

    #[test]
    fn ignores_password_reference_in_an_unrelated_url_parameter_value() {
        assert!(ids("https://service.invalid/?label=service-password&color=bluevalue").is_empty());
    }

    #[test]
    fn accepts_long_line_of_placeholder_assignments() {
        assert!(ids(&"password=placeholder;".repeat(20_000)).is_empty());
    }

    #[test]
    fn accepts_malformed_assignment_syntax_without_credential_names() {
        assert!(ids(&"]:".repeat(2_000)).is_empty());
    }

    #[test]
    fn refuses_credential_after_long_malformed_assignment_prefix() {
        let text = format!("{} password=syntheticcredentialvalue", "]:".repeat(2_000));
        assert_eq!(ids(&text), vec!["secret-assignment"]);
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
