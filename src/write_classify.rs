//! Write-time memory kind inference.
//!
//! Agents should not need to hand-pick durable metadata for ordinary memories.
//! This module keeps the first-pass inference deterministic and auditable: it
//! recognizes clear text shapes, returns a reason for JSON/debug output, and
//! leaves ambiguous writes untagged instead of pretending confidence we do not
//! have. Explicit CLI/API `kind` always remains authoritative.

use crate::note::MemoryKind;
use crate::signals;

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

/// Input for inferring write scope before kind inference runs.
#[derive(Debug, Clone, Copy)]
pub struct InferScopeInput<'a> {
    /// Resolved project id, when the caller supplied project context.
    pub project_id: Option<&'a str>,
    /// Explicit kind supplied by the caller, if any.
    pub explicit_kind: Option<MemoryKind>,
    /// Markdown body being remembered.
    pub body: &'a str,
}

/// Inferred memory scope plus a stable machine-readable reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeInference {
    /// Selected scope.
    pub scope: &'static str,
    /// Stable reason key for explain/debug output.
    pub reason: &'static str,
}

/// Infer write scope when the caller did not provide `--scope`.
///
/// This deliberately only promotes to `project`; it never infers private or
/// global from scratch. The configured default remains authoritative unless a
/// project id is available and either the explicit kind or text clearly says the
/// memory belongs to the active repo.
pub fn infer_scope(input: InferScopeInput<'_>) -> Option<ScopeInference> {
    input.project_id?;
    if input.explicit_kind == Some(MemoryKind::ProjectFact) {
        return Some(ScopeInference {
            scope: "project",
            reason: "explicit-project-fact",
        });
    }

    let lower = input.body.trim().to_lowercase();
    if lower.is_empty() {
        return None;
    }
    if reads_as_repo_local(&lower) {
        return Some(ScopeInference {
            scope: "project",
            reason: "repo-local-language",
        });
    }

    None
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
    signals::looks_operational(lower, signals::OPERATIONAL_KEYWORDS.iter().copied())
}

// Reference language requires an actual pointer: a URL, or a "see"/"read"
// directive aimed at a path. A bare file-name mention (e.g. a fact that cites
// `docs/adr/0001.md`) is NOT a pointer — tagging it reference would persist a
// search-only verdict and silently hide a durable fact from session start, so
// ambiguity here must fall through to the other rules or stay untagged.
fn reads_as_reference(lower: &str) -> bool {
    lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("see `")
        || lower.contains("read `")
        || lower.contains("see ~/.")
        || lower.contains("read ~/.")
        || lower.contains("see /")
        || lower.contains("read /")
}

fn reads_as_repo_local(lower: &str) -> bool {
    lower.contains("this repo")
        || lower.contains("this project")
        || lower.contains("this checkout")
        || lower.contains("the repo ")
        || lower.contains("repo ")
        || lower.contains("repository")
        || lower.contains("workflow")
        || lower.contains("ci ")
        || lower.contains("deploy")
        || lower.contains("test command")
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
    fn dated_freed_resources_becomes_incident() {
        // "freed" is part of the shared operational vocabulary; the read-time
        // classifier already withholds this shape, so write-time inference must
        // tag it the same way or the verdict depends on which side judged it.
        assert_eq!(
            infer("2026-05-26 freed 40GB on the root partition after relocating containers."),
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
    fn doc_citing_project_fact_stays_project_fact() {
        // A fact that merely mentions a Markdown file is not a pointer record.
        // Tagging it `reference` would persist a search-only verdict and the
        // fact would silently stop injecting in its own project's sessions —
        // a recall loss the conservative bias exists to prevent.
        let result = infer_kind(InferKindInput {
            scope: "project",
            project_id: Some("repo-alpha"),
            body: "Architecture decisions live in docs/adr/0001-store-layout.md.",
        });
        assert_eq!(
            result.map(|result| result.kind),
            Some(MemoryKind::ProjectFact)
        );
    }

    #[test]
    fn doc_mention_without_pointer_stays_untagged() {
        // Mentioning a file name in passing is not reference language; an
        // untagged global write keeps injecting (conservative direction).
        assert!(infer("The guidelines file is AGENTS.md, kept at the repo root.").is_none());
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

    #[test]
    fn explicit_project_fact_infers_project_scope() {
        let result = infer_scope(InferScopeInput {
            project_id: Some("repo-alpha"),
            explicit_kind: Some(MemoryKind::ProjectFact),
            body: "The test workflow runs on Ubuntu and macOS.",
        });
        assert_eq!(result.map(|result| result.scope), Some("project"));
    }

    #[test]
    fn repo_language_infers_project_scope() {
        let result = infer_scope(InferScopeInput {
            project_id: Some("repo-alpha"),
            explicit_kind: None,
            body: "This repo deploys from tags.",
        });
        assert_eq!(result.map(|result| result.scope), Some("project"));
    }

    #[test]
    fn generic_preference_does_not_infer_project_scope() {
        assert!(
            infer_scope(InferScopeInput {
                project_id: Some("repo-alpha"),
                explicit_kind: None,
                body: "Chris prefers deterministic agent tooling.",
            })
            .is_none()
        );
    }
}
