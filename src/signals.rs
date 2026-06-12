//! Shared text-shape signals for memory classification.
//!
//! Write-time kind inference (`write_classify`) and read-time inject
//! selection (`inject`) must judge the same text the same way: a record that
//! write inference would tag `incident` must also be withheld by the read-time
//! heuristic when it arrives untagged from another machine or an older writer,
//! and a record that write inference would tag `preference` must survive
//! stricter startup filtering. That only holds if both sides consume one
//! vocabulary and one date signal, so this module is the single owner of both.
//! Do not re-declare durable text-shape keywords in classifier code; extend the
//! helpers here.

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

/// True when text reads as durable behavior guidance.
///
/// This is intentionally narrower than "contains should". Startup context can
/// tolerate a missed legacy project fact because it remains searchable, but a
/// generic "repo X should..." fact should not become an always-on preference in
/// every future session. Keep this aligned with the examples in
/// `write_classify` tests and with real global preference memories such as
/// "the maintainer prefers...", "Always...", and "When working..., prefer...".
pub fn reads_as_preference(lower: &str) -> bool {
    lower.starts_with("prefer ")
        || (lower.starts_with("when ") && lower.contains(" prefer "))
        || lower.contains(" prefers ")
        || lower.starts_with("always ")
        || lower.contains(" always ")
        || lower.starts_with("never ")
        || lower.contains(" never ")
        || lower.starts_with("use ")
        || lower.starts_with("avoid ")
        || lower.contains(" avoid ")
        || lower.starts_with("add comments")
        || lower.contains(" add comments")
        || (lower.starts_with("write ") && lower.contains("comments"))
        || lower.starts_with("do not ")
        || lower.contains(" do not ")
        || lower.starts_with("don't ")
        || lower.contains(" don't ")
        || lower.contains(" wants coding agents ")
        || lower.contains(" wants agents ")
        || lower.contains(" wants the agent ")
        || lower.contains("agent should ")
        || lower.contains("agents should ")
        || lower.contains("coding agents should ")
        || lower.contains("default to ")
}

/// True when text is a point-in-time task/PR status rather than durable
/// context.
///
/// These records can still be useful through search, but injecting stale
/// "unmerged" or "do not merge" instructions at session start is actively
/// misleading after the work lands. Keep this vocabulary deliberately narrow:
/// it should catch workflow-state snapshots, not normal project facts.
pub fn looks_transient_status(lower: &str) -> bool {
    lower.contains("open/mergeable")
        || lower.contains("unmerged")
        || lower.contains("do not merge")
        || lower.contains("pending review")
        || lower.contains("pending owner review")
        || lower.contains("pending human review")
}

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

    #[test]
    fn preference_language_is_behavioral() {
        assert!(reads_as_preference(
            "the maintainer prefers agent-agnostic tools"
        ));
        assert!(reads_as_preference("always run the formatter"));
        assert!(reads_as_preference("when working in shell, prefer rg"));
        assert!(reads_as_preference(
            "coding agents should write down durable facts"
        ));
        assert!(reads_as_preference(
            "the user wants coding agents to remember durable facts"
        ));
        assert!(reads_as_preference(
            "add comments that capture non-obvious rationale"
        ));
        assert!(reads_as_preference(
            "write generous code comments that explain why"
        ));

        // A project/tool fact can contain "should" without being a global
        // behavior preference. This shape should stay searchable unless it is
        // explicitly tagged or scoped to a project.
        assert!(!reads_as_preference(
            "dotfiles ci should keep workflow-installed packages minimal"
        ));
        assert!(!reads_as_preference(
            "shared ci plan says callers use main and prefer named setup modes"
        ));
    }

    #[test]
    fn transient_status_language_is_narrow() {
        assert!(looks_transient_status(
            "all ci-green, open/mergeable, unmerged pending review"
        ));
        assert!(looks_transient_status("do not merge without review"));

        assert!(!looks_transient_status(
            "do not store secrets or credentials"
        ));
        assert!(!looks_transient_status(
            "project releases are cut from main after ci passes"
        ));
    }
}
