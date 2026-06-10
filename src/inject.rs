//! Read-time inject classification for session-start context.
//!
//! Separates the records worth putting in front of an agent at the START of a
//! session from those that are better left to explicit recall via `hm search`.
//! This runs purely at read time over already-loaded index metadata and the
//! note body; it never writes, mutates, or reclassifies stored data, so it is
//! fully reversible and safe to run against a cloud-synced, append-only store.
//!
//! Why a content heuristic at all: the existing store has no per-record "kind"
//! signal, and we deliberately do NOT mutate it to add one (that would fight the
//! mtime-based index and multi-machine sync). So legacy records must be judged
//! from what they already carry. The heuristic is therefore intentionally
//! CONSERVATIVE and asymmetric: it only withholds a record when it is clearly
//! operational, and defaults to injecting otherwise. Dropping a preference the
//! agent needed is far worse than leaving one stale incident in the context, so
//! the classifier is tuned to never drop on ambiguity. A clearly marked
//! incident gets caught; a marker-less one is left in (a miss), not a
//! preference thrown out (a regression). An explicit `kind` field, added for
//! new writes in a later change, is the durable fix that closes the residual
//! gap safely; this heuristic is the bridge for untagged history.

use crate::note::EntryKind;

/// How a candidate memory should be treated at session-start injection time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectClass {
    /// Durable, behavior-shaping memory. Inject in every session.
    AlwaysOn,
    /// Memory about one project. Inject only when that project is active; the
    /// existing scope/project filter enforces the match.
    ProjectScoped,
    /// Operational or raw material. Do not auto-inject; reachable via search.
    SearchOnly,
}

/// Keyword signals that, combined with a date, mark a record as operational.
///
/// Kept as data so a later change can source these from config without
/// touching the classifier. The defaults describe postmortem/cleanup language;
/// matching is case-insensitive substring.
#[derive(Debug, Clone)]
pub struct IncidentMarkers {
    /// Operational keywords; presence of any one (with a date) flags a record.
    pub keywords: Vec<String>,
}

impl Default for IncidentMarkers {
    fn default() -> Self {
        Self {
            keywords: [
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
            ]
            .iter()
            .map(|keyword| (*keyword).to_owned())
            .collect(),
        }
    }
}

/// Inputs for classifying one candidate record.
#[derive(Debug, Clone, Copy)]
pub struct ClassifyInput<'a> {
    /// Memory scope (`global`, `project`, ...).
    pub scope: &'a str,
    /// Whether the record is a durable `remember` or a raw `note`.
    pub entry_kind: EntryKind,
    /// Canonical note body, used only for the conservative content signal.
    pub body: &'a str,
}

/// Classify a candidate for session-start injection.
///
/// Order matters and encodes the conservative bias:
/// 1. Raw `note` material is search-only (mirrors the default that `hm note`
///    entries do not auto-inject).
/// 2. Project-scoped records defer to the existing project filter.
/// 3. A global record is withheld ONLY when it reads as operational (a date
///    AND an operational keyword); otherwise it is always-on. Requiring both
///    signals keeps a dated *preference* from being misread as an incident.
pub fn classify(input: ClassifyInput<'_>, markers: &IncidentMarkers) -> InjectClass {
    if input.entry_kind == EntryKind::Note {
        return InjectClass::SearchOnly;
    }
    if input.scope == "project" {
        return InjectClass::ProjectScoped;
    }
    if looks_operational(input.body, markers) {
        return InjectClass::SearchOnly;
    }
    InjectClass::AlwaysOn
}

/// True when the body reads as an operational log: it carries an ISO date AND at
/// least one operational keyword. Both are required so durable guidance that
/// merely mentions a date is not mistaken for an incident.
fn looks_operational(body: &str, markers: &IncidentMarkers) -> bool {
    if !has_iso_date(body) {
        return false;
    }
    let lower = body.to_lowercase();
    markers
        .keywords
        .iter()
        .any(|keyword| lower.contains(&keyword.to_lowercase()))
}

/// Detect a `YYYY-MM-DD` date anywhere in the text without a regex dependency.
///
/// Deliberately loose on calendar validity (e.g. accepts `2026-13-40`): the goal
/// is a cheap "this looks like a dated log entry" signal, not date parsing.
fn has_iso_date(text: &str) -> bool {
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

    fn global(body: &str) -> ClassifyInput<'_> {
        ClassifyInput {
            scope: "global",
            entry_kind: EntryKind::Remember,
            body,
        }
    }

    #[test]
    fn raw_notes_are_search_only() {
        let input = ClassifyInput {
            scope: "global",
            entry_kind: EntryKind::Note,
            body: "a stray idea",
        };
        assert_eq!(
            classify(input, &IncidentMarkers::default()),
            InjectClass::SearchOnly
        );
    }

    #[test]
    fn project_scope_defers_to_project_filter() {
        let input = ClassifyInput {
            scope: "project",
            entry_kind: EntryKind::Remember,
            body: "repo alpha deploys on tag push",
        };
        assert_eq!(
            classify(input, &IncidentMarkers::default()),
            InjectClass::ProjectScoped
        );
    }

    #[test]
    fn plain_global_preference_is_always_on() {
        let body = "Prefer fd over find and rg over grep.";
        assert_eq!(
            classify(global(body), &IncidentMarkers::default()),
            InjectClass::AlwaysOn
        );
    }

    #[test]
    fn dated_and_operational_is_search_only() {
        let body = "2026-06-06 root cause: a cron job leaked session bus daemons.";
        assert_eq!(
            classify(global(body), &IncidentMarkers::default()),
            InjectClass::SearchOnly
        );
    }

    #[test]
    fn dated_preference_without_keyword_stays_always_on() {
        // The conservative guard: a preference that mentions a date must not be
        // mistaken for an incident, because no operational keyword is present.
        let body = "Since 2026-01-01, always run the formatter before committing.";
        assert_eq!(
            classify(global(body), &IncidentMarkers::default()),
            InjectClass::AlwaysOn
        );
    }

    #[test]
    fn marker_less_incident_is_left_in() {
        // Honest residual: an operational note with no date is NOT withheld. We
        // accept this miss rather than risk dropping real guidance; the explicit
        // kind field is what closes this gap safely.
        let body = "The installer now rebuilds when the recorded version is stale.";
        assert_eq!(
            classify(global(body), &IncidentMarkers::default()),
            InjectClass::AlwaysOn
        );
    }

    #[test]
    fn iso_date_detection() {
        assert!(has_iso_date("logged 2026-05-26 cleanup"));
        assert!(has_iso_date("2026-01-01 at the start"));
        assert!(!has_iso_date("version 1.2.3 released"));
        assert!(!has_iso_date("no date here at all"));
    }
}
