//! Write-time memory kind inference.
//!
//! Agents should not need to hand-pick durable metadata for ordinary memories.
//! This module keeps the first-pass inference deterministic and auditable: it
//! recognizes clear text shapes, returns a reason for JSON/debug output, and
//! leaves ambiguous writes untagged instead of pretending confidence we do not
//! have. Explicit CLI/API `kind` always remains authoritative.

use crate::note::MemoryKind;

/// Input for inferring a memory kind while writing a `remember` record.
#[derive(Debug, Clone, Copy)]
pub struct InferKindInput<'a> {
    /// Already resolved write scope.
    pub scope: &'a str,
    /// Resolved project id, when the caller supplied project context.
    pub project_id: Option<&'a str>,
    /// Markdown body being remembered.
    pub body: &'a str,
}

/// Inferred memory kind plus a stable machine-readable reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindInference {
    /// Selected kind.
    pub kind: MemoryKind,
    /// Stable reason key for explain/debug output.
    pub reason: &'static str,
}

/// Infer a memory kind from write context and text shape.
///
/// The ordering intentionally protects durable behavior guidance first:
/// preference language should not be demoted merely because it mentions a date
/// or project. Operational/reference records are search-only at startup, and
/// project facts are inferred only when the write is already project-scoped so
/// kind and scope cannot disagree.
pub fn infer_kind(input: InferKindInput<'_>) -> Option<KindInference> {
    let body = input.body.trim();
    if body.is_empty() {
        return None;
    }
    let lower = body.to_lowercase();

    if reads_as_preference(&lower) {
        return Some(KindInference {
            kind: MemoryKind::Preference,
            reason: "preference-language",
        });
    }
    if reads_as_incident(&lower) {
        return Some(KindInference {
            kind: MemoryKind::Incident,
            reason: "operational-language",
        });
    }
    if reads_as_reference(&lower) {
        return Some(KindInference {
            kind: MemoryKind::Reference,
            reason: "reference-language",
        });
    }
    if input.scope == "project" && input.project_id.is_some() {
        return Some(KindInference {
            kind: MemoryKind::ProjectFact,
            reason: "project-scoped-write",
        });
    }

    None
}

fn reads_as_preference(lower: &str) -> bool {
    lower.contains("prefer ")
        || lower.contains("prefers ")
        || lower.starts_with("always ")
        || lower.contains(" always ")
        || lower.starts_with("never ")
        || lower.contains(" never ")
        || lower.contains(" use ")
        || lower.starts_with("use ")
        || lower.contains(" should ")
}

fn reads_as_incident(lower: &str) -> bool {
    has_iso_date(lower)
        && [
            "root cause",
            "postmortem",
            "cleanup",
            "leaked",
            "exhaustion",
            "regression",
            "hotfix",
            "emergency",
            "incident",
            "outage",
            "fixed",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn reads_as_reference(lower: &str) -> bool {
    lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("see `")
        || lower.contains("read `")
        || lower.contains("see ~/.")
        || lower.contains("read ~/.")
        || lower.contains("see /")
        || lower.contains("read /")
        || lower.contains(".md")
}

fn has_iso_date(text: &str) -> bool {
    let bytes = text.as_bytes();
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

    fn infer(body: &str) -> Option<MemoryKind> {
        infer_kind(InferKindInput {
            scope: "global",
            project_id: None,
            body,
        })
        .map(|result| result.kind)
    }

    #[test]
    fn preference_language_wins() {
        assert_eq!(
            infer("Always run cargo test before saying done."),
            Some(MemoryKind::Preference)
        );
    }

    #[test]
    fn operational_language_becomes_incident() {
        assert_eq!(
            infer("2026-06-06 root cause: cron leaked dbus processes."),
            Some(MemoryKind::Incident)
        );
    }

    #[test]
    fn references_are_search_only_material() {
        assert_eq!(
            infer("For host details, read ~/.local/share/doc/dot/home-lab.md."),
            Some(MemoryKind::Reference)
        );
    }

    #[test]
    fn project_scope_defaults_to_project_fact() {
        let result = infer_kind(InferKindInput {
            scope: "project",
            project_id: Some("repo-alpha"),
            body: "The test workflow runs on Ubuntu and macOS.",
        });
        assert_eq!(
            result.map(|result| result.kind),
            Some(MemoryKind::ProjectFact)
        );
    }

    #[test]
    fn ambiguous_global_write_stays_untagged() {
        assert!(infer("Something worth revisiting later.").is_none());
    }
}
