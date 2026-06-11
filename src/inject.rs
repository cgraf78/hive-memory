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

use crate::note::{EntryKind, MemoryKind};
use crate::signals;

/// Session-start selection strategy, chosen by `context_strategy` config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Legacy behavior: include everything in scope, ordered by recency. No
    /// inject classification. This is the default so nothing changes until a
    /// store opts in.
    #[default]
    Recency,
    /// Apply the inject classifier: search-only candidates are withheld so
    /// session-start context favors durable, project-relevant memory.
    Relevance,
}

impl Strategy {
    /// Resolve the strategy from its config string, defaulting to `Recency` on
    /// anything unrecognized. Unknown values must not fail the latency-sensitive
    /// hook path, so an unexpected string degrades to today's behavior.
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_lowercase().as_str() {
            "relevance" => Self::Relevance,
            _ => Self::Recency,
        }
    }
}

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
/// touching the classifier. The defaults come from the shared
/// `signals::OPERATIONAL_KEYWORDS` vocabulary so read-time selection and
/// write-time inference cannot drift apart; matching is case-insensitive
/// substring.
#[derive(Debug, Clone)]
pub struct IncidentMarkers {
    /// Operational keywords; presence of any one (with a date) flags a record.
    pub keywords: Vec<String>,
}

impl Default for IncidentMarkers {
    fn default() -> Self {
        Self {
            keywords: signals::OPERATIONAL_KEYWORDS
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
    /// Explicit memory kind when the writer set one. Authoritative over the
    /// content heuristic; absent for legacy records.
    pub kind: Option<MemoryKind>,
    /// Canonical note body, used only for the conservative content signal.
    pub body: &'a str,
}

/// Classify a candidate for session-start injection.
///
/// Order matters and encodes the conservative bias:
/// 1. Raw `note` material is search-only (mirrors the default that `hm note`
///    entries do not auto-inject).
/// 2. An explicit `kind` is authoritative: the writer told us what this is, so
///    trust it and skip the heuristic entirely. This is the safe way to catch
///    records the content heuristic cannot (a marker-less incident, a plain
///    reference) without guessing.
/// 3. Otherwise fall back: project-scoped records defer to the project filter,
///    and a global record is withheld ONLY when it reads as operational (a date
///    AND an operational keyword), so a dated *preference* is not misread.
pub fn classify(input: ClassifyInput<'_>, markers: &IncidentMarkers) -> InjectClass {
    if input.entry_kind == EntryKind::Note {
        return InjectClass::SearchOnly;
    }
    if let Some(kind) = input.kind {
        return match kind {
            MemoryKind::Preference => InjectClass::AlwaysOn,
            MemoryKind::ProjectFact => InjectClass::ProjectScoped,
            MemoryKind::Incident | MemoryKind::Reference => InjectClass::SearchOnly,
        };
    }
    if input.scope == "project" {
        return InjectClass::ProjectScoped;
    }
    if looks_operational(input.body, markers) {
        return InjectClass::SearchOnly;
    }
    InjectClass::AlwaysOn
}

/// True when the body reads as an operational log; see `signals` for the
/// shared date-plus-keyword rule.
fn looks_operational(body: &str, markers: &IncidentMarkers) -> bool {
    signals::looks_operational(body, markers.keywords.iter().map(String::as_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global(body: &str) -> ClassifyInput<'_> {
        ClassifyInput {
            scope: "global",
            entry_kind: EntryKind::Remember,
            kind: None,
            body,
        }
    }

    #[test]
    fn raw_notes_are_search_only() {
        let input = ClassifyInput {
            scope: "global",
            entry_kind: EntryKind::Note,
            kind: None,
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
            kind: None,
            body: "repo alpha deploys on tag push",
        };
        assert_eq!(
            classify(input, &IncidentMarkers::default()),
            InjectClass::ProjectScoped
        );
    }

    #[test]
    fn explicit_kind_overrides_heuristic() {
        let markers = IncidentMarkers::default();
        // A marker-less incident the heuristic cannot catch is correctly
        // withheld once tagged.
        let tagged_incident = ClassifyInput {
            scope: "global",
            entry_kind: EntryKind::Remember,
            kind: Some(MemoryKind::Incident),
            body: "the installer now rebuilds when the version is stale",
        };
        assert_eq!(classify(tagged_incident, &markers), InjectClass::SearchOnly);

        // An explicit preference stays always-on even if it reads operational.
        let tagged_pref = ClassifyInput {
            scope: "global",
            entry_kind: EntryKind::Remember,
            kind: Some(MemoryKind::Preference),
            body: "2026-06-06 root cause taught us to always run the linter",
        };
        assert_eq!(classify(tagged_pref, &markers), InjectClass::AlwaysOn);
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
    fn dated_fix_log_is_search_only() {
        // "fixed" is part of the shared operational vocabulary used by both
        // write-time inference and this read-time classifier. A drift between
        // the two sides would let the same text be judged differently at write
        // and read time.
        let body = "2026-06-02 fixed the stale launcher by rebuilding on version mismatch.";
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
    fn strategy_from_config_defaults_safely() {
        assert_eq!(Strategy::from_config("relevance"), Strategy::Relevance);
        assert_eq!(Strategy::from_config("  Relevance "), Strategy::Relevance);
        assert_eq!(Strategy::from_config("recency"), Strategy::Recency);
        // Unknown values degrade to legacy behavior rather than erroring.
        assert_eq!(Strategy::from_config("bogus"), Strategy::Recency);
        assert_eq!(Strategy::from_config(""), Strategy::Recency);
    }

    #[test]
    fn default_markers_match_shared_vocabulary() {
        // Anti-drift lock: read-time selection must judge untagged text with
        // exactly the vocabulary write-time inference uses, or a record can be
        // injected here that another machine would have tagged search-only.
        assert_eq!(
            IncidentMarkers::default().keywords,
            signals::OPERATIONAL_KEYWORDS
                .iter()
                .map(|keyword| (*keyword).to_owned())
                .collect::<Vec<_>>()
        );
    }
}
