//! Read-time inject classification for session-start context.
//!
//! Separates the records worth putting in front of an agent at the START of a
//! session from those that are better left to explicit recall via `hm search`.
//! This runs purely at read time over already-loaded index metadata and the
//! note body; it never writes, mutates, or reclassifies stored data, so it is
//! fully reversible and safe to run against a cloud-synced, append-only store.
//!
//! Why a content heuristic at all: the existing store has legacy records with
//! no per-record "kind" signal, and we deliberately do NOT mutate them in
//! place (that would fight the mtime-based index and multi-machine sync). So
//! legacy records must be judged from what they already carry.
//!
//! The current asymmetric rule is: keep legacy global records only when they
//! clearly read as durable behavior guidance, keep project-scoped records in
//! their own project, and leave ambiguous global facts searchable rather than
//! injecting them into every startup. Explicit `kind` remains authoritative and
//! is the preferred long-term path for new writes.

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
    /// Project identity attached to the record, when present.
    pub project_id: Option<&'a str>,
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
/// Order matters and encodes the startup-context bias:
/// 1. Raw `note` material is search-only (mirrors the default that `hm note`
///    entries do not auto-inject).
/// 2. Point-in-time workflow state is search-only even if a legacy write tagged
///    it otherwise; stale "do not merge" context is worse than having to search
///    for old PR coordination.
/// 3. An explicit `kind` is authoritative for search-only kinds and for
///    project-scoped facts. Legacy preference tags attached to a specific
///    project still need to read like behavior guidance; older inference was
///    broader and mis-tagged some repo facts as always-on preferences.
/// 4. Otherwise fall back: project-scoped records defer to the project filter,
///    global records that clearly read as behavior guidance stay always-on, and
///    all other ambiguous global records remain available through search.
pub fn classify(input: ClassifyInput<'_>, markers: &IncidentMarkers) -> InjectClass {
    if input.entry_kind == EntryKind::Note {
        return InjectClass::SearchOnly;
    }
    let lower = input.body.to_lowercase();
    if signals::looks_transient_status(&lower) {
        return InjectClass::SearchOnly;
    }
    if let Some(kind) = input.kind {
        return match kind {
            MemoryKind::Preference
                if input.project_id.is_none() || signals::reads_as_preference(&lower) =>
            {
                InjectClass::AlwaysOn
            }
            MemoryKind::Preference => InjectClass::SearchOnly,
            MemoryKind::ProjectFact if input.scope == "project" => InjectClass::ProjectScoped,
            MemoryKind::ProjectFact => InjectClass::SearchOnly,
            MemoryKind::Incident | MemoryKind::Reference => InjectClass::SearchOnly,
        };
    }
    if input.scope == "project" {
        return InjectClass::ProjectScoped;
    }
    if looks_operational(input.body, markers) {
        return InjectClass::SearchOnly;
    }
    if signals::reads_as_preference(&lower) {
        return InjectClass::AlwaysOn;
    }
    InjectClass::SearchOnly
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
            project_id: None,
            entry_kind: EntryKind::Remember,
            kind: None,
            body,
        }
    }

    #[test]
    fn raw_notes_are_search_only() {
        let input = ClassifyInput {
            scope: "global",
            project_id: None,
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
            project_id: Some("repo-alpha"),
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
            project_id: None,
            entry_kind: EntryKind::Remember,
            kind: Some(MemoryKind::Incident),
            body: "the installer now rebuilds when the version is stale",
        };
        assert_eq!(classify(tagged_incident, &markers), InjectClass::SearchOnly);

        // An explicit preference stays always-on even if it reads operational.
        let tagged_pref = ClassifyInput {
            scope: "global",
            project_id: None,
            entry_kind: EntryKind::Remember,
            kind: Some(MemoryKind::Preference),
            body: "2026-06-06 root cause taught us to always run the linter",
        };
        assert_eq!(classify(tagged_pref, &markers), InjectClass::AlwaysOn);
    }

    #[test]
    fn global_project_fact_kind_is_search_only() {
        let tagged_global_fact = ClassifyInput {
            scope: "global",
            project_id: Some("repo-alpha"),
            entry_kind: EntryKind::Remember,
            kind: Some(MemoryKind::ProjectFact),
            body: "Project alpha deploys on tag push.",
        };
        assert_eq!(
            classify(tagged_global_fact, &IncidentMarkers::default()),
            InjectClass::SearchOnly
        );
    }

    #[test]
    fn stale_inferred_preference_with_project_id_is_search_only() {
        let stale_inferred = ClassifyInput {
            scope: "global",
            project_id: Some("repo-alpha"),
            entry_kind: EntryKind::Remember,
            kind: Some(MemoryKind::Preference),
            body: "Project alpha PR #8 fixes nested hook state attribution.",
        };
        assert_eq!(
            classify(stale_inferred, &IncidentMarkers::default()),
            InjectClass::SearchOnly
        );
    }

    #[test]
    fn transient_pr_status_is_search_only() {
        let stale_status = ClassifyInput {
            scope: "project",
            project_id: Some("repo-alpha"),
            entry_kind: EntryKind::Remember,
            kind: Some(MemoryKind::ProjectFact),
            body: "PR stack is all CI-green, OPEN/MERGEABLE, UNMERGED pending review; do NOT merge without review.",
        };
        assert_eq!(
            classify(stale_status, &IncidentMarkers::default()),
            InjectClass::SearchOnly
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
    fn untagged_design_sketch_is_search_only() {
        // A long design sketch carries project/tool detail but does not tell
        // the agent how to behave in every future session. Keep it searchable
        // unless it is explicitly scoped/tagged for startup injection.
        let body = "Detailed design sketch for an automated review gate in CI \
                    (discussed 2026-06-09, not built): a reusable workflow each \
                    repo opts into, gating on severity.";
        assert_eq!(
            classify(global(body), &IncidentMarkers::default()),
            InjectClass::SearchOnly
        );
    }

    #[test]
    fn marker_less_incident_is_search_only() {
        // Ambiguous global facts are now withheld by default. This catches the
        // common legacy shape where an incident/fix lacks enough markers for
        // the date-plus-keyword operational rule.
        let body = "The installer now rebuilds when the recorded version is stale.";
        assert_eq!(
            classify(global(body), &IncidentMarkers::default()),
            InjectClass::SearchOnly
        );
    }

    #[test]
    fn untagged_global_project_fact_is_search_only() {
        let body = "Project shdeps rebuilds source-checkout binaries when the recorded version no longer matches checkout HEAD.";
        assert_eq!(
            classify(global(body), &IncidentMarkers::default()),
            InjectClass::SearchOnly
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
