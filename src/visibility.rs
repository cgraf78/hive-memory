//! Shared visibility rules for indexed memory records.
//!
//! Search, context assembly, and hooks must agree on what an agent may see.
//! Keeping the audience rule here prevents subtle drift where one path
//! accidentally exposes an agent-private note another path would hide.

use crate::index::IndexEntry;

/// Return whether an indexed record is visible to the active agent.
///
/// Non-`agent-private` scopes are readable after store-level policy has already
/// selected an allowed store. `agent-private` records require an active agent
/// identity. Legacy/manual private records with an empty audience are readable
/// only by their writer; modern records should carry an explicit audience.
pub fn audience_allows(entry: &IndexEntry, agent_id: Option<&str>) -> bool {
    if entry.scope != "agent-private" {
        return true;
    }

    let Some(agent_id) = agent_id else {
        return false;
    };

    if entry.audience.is_empty() {
        return entry.agent_id == agent_id;
    }

    entry.audience.iter().any(|audience| audience == agent_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note;

    fn entry(scope: &str, audience: Vec<&str>, writer: &str) -> IndexEntry {
        IndexEntry {
            id: "id".to_owned(),
            store_id: "store-id".to_owned(),
            entry_kind: note::EntryKind::Remember,
            scope: scope.to_owned(),
            project_id: None,
            audience: audience.into_iter().map(str::to_owned).collect(),
            tags: Vec::new(),
            subject: None,
            confidence: note::Confidence::High,
            kind: None,
            classified: None,
            agent_id: writer.to_owned(),
            host_id: "taylor".to_owned(),
            created_at: "2026-05-16T00:00:00Z".to_owned(),
            body: "body".to_owned(),
            note_path: "inbox/notes/2026-05-16/id.md".to_owned(),
            event_path: None,
        }
    }

    #[test]
    fn non_private_records_do_not_require_agent_identity() {
        assert!(audience_allows(&entry("global", Vec::new(), "codex"), None));
    }

    #[test]
    fn private_records_require_matching_audience_or_legacy_writer() {
        assert!(audience_allows(
            &entry("agent-private", vec!["codex"], "claude"),
            Some("codex")
        ));
        assert!(!audience_allows(
            &entry("agent-private", vec!["claude"], "claude"),
            Some("codex")
        ));
        assert!(audience_allows(
            &entry("agent-private", Vec::new(), "codex"),
            Some("codex")
        ));
        assert!(!audience_allows(
            &entry("agent-private", Vec::new(), "claude"),
            Some("codex")
        ));
        assert!(!audience_allows(
            &entry("agent-private", vec!["codex"], "codex"),
            None
        ));
    }
}
