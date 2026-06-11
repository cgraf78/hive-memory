//! Shared text-shape signals for memory classification.
//!
//! Write-time kind inference (`write_classify`) and read-time inject
//! selection (`inject`) must judge the same text the same way: a record that
//! write inference would tag `incident` must also be withheld by the read-time
//! heuristic when it arrives untagged from another machine or an older writer.
//! That only holds if both sides consume one vocabulary and one date signal,
//! so this module is the single owner of both. Do not re-declare operational
//! keywords or date detection in classifier code; extend the list here.

/// Operational keywords marking postmortem/cleanup/fix language.
///
/// Presence of any one of these together with an ISO date flags a record as an
/// operational log (search-only at session start). Kept as data so a later
/// change can source the list from config without touching the classifiers.
pub const OPERATIONAL_KEYWORDS: &[&str] = &[
    "root cause",
    "postmortem",
    "cleanup",
    "freed",
    "leaked",
    "exhaustion",
    "regression",
    "hotfix",
    "emergency",
    "incident",
    "outage",
    "fixed",
];

/// True when the body reads as an operational log: it carries an ISO date AND
/// at least one operational keyword. Both are required so durable guidance
/// that merely mentions a date is not mistaken for an incident. Matching is
/// case-insensitive on both sides.
pub fn looks_operational<'a, I>(body: &str, keywords: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    if !has_iso_date(body) {
        return false;
    }
    let lower = body.to_lowercase();
    keywords
        .into_iter()
        .any(|keyword| lower.contains(&keyword.to_lowercase()))
}

/// Detect a `YYYY-MM-DD` date anywhere in the text without a regex dependency.
///
/// Deliberately loose on calendar validity (e.g. accepts `2026-13-40`): the
/// goal is a cheap "this looks like a dated log entry" signal, not date
/// parsing.
pub fn has_iso_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    // A date needs 10 chars: 4 digits, '-', 2 digits, '-', 2 digits.
    bytes.windows(10).any(|window| {
        window[..4].iter().all(u8::is_ascii_digit)
            && window[4] == b'-'
            && window[5..7].iter().all(u8::is_ascii_digit)
            && window[7] == b'-'
            && window[8..10].iter().all(u8::is_ascii_digit)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_date_detection() {
        assert!(has_iso_date("logged 2026-05-26 cleanup"));
        assert!(has_iso_date("2026-01-01 at the start"));
        assert!(!has_iso_date("version 1.2.3 released"));
        assert!(!has_iso_date("no date here at all"));
    }

    #[test]
    fn operational_requires_date_and_keyword() {
        let keywords = OPERATIONAL_KEYWORDS.iter().copied();
        assert!(looks_operational(
            "2026-06-06 root cause: cron leaked daemons",
            keywords
        ));
        // A keyword without a date is not operational (durable guidance may
        // mention fixes), and a date without a keyword is not operational
        // (dated preferences must survive).
        assert!(!looks_operational(
            "always document the root cause",
            OPERATIONAL_KEYWORDS.iter().copied()
        ));
        assert!(!looks_operational(
            "since 2026-01-01 prefer rg over grep",
            OPERATIONAL_KEYWORDS.iter().copied()
        ));
    }

    #[test]
    fn keyword_matching_is_case_insensitive() {
        assert!(looks_operational(
            "2026-06-06 ROOT CAUSE: leaked daemons",
            OPERATIONAL_KEYWORDS.iter().copied()
        ));
    }
}
