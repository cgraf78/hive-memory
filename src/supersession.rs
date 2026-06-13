//! Conservative stale-memory suppression.
//!
//! This module does not rewrite canonical memory. It only answers whether a
//! newer indexed record is strong enough evidence to hide an older record from
//! broad recall results. The rules are intentionally narrow so historical
//! searches can still find the old fact when the query names it directly.

use crate::{index::IndexEntry, note};
use std::collections::BTreeSet;
use time::OffsetDateTime;

/// Return whether `older` should be suppressed when `newer` is also present.
pub fn should_suppress_older(older: &IndexEntry, newer: &IndexEntry, query: Option<&str>) -> bool {
    if older.id == newer.id
        || older.entry_kind != note::EntryKind::Remember
        || newer.entry_kind != note::EntryKind::Remember
        || !same_scope(older, newer)
    {
        return false;
    }
    if newer.supersedes.iter().any(|id| id == &older.id) {
        return !explicitly_searches_old_fact(
            &older.body.to_ascii_lowercase(),
            &newer.body.to_ascii_lowercase(),
            query,
        );
    }
    if !is_newer(newer, older) {
        return false;
    }

    let old_body = older.body.to_ascii_lowercase();
    let new_body = newer.body.to_ascii_lowercase();
    if !has_stale_marker(&old_body) || !has_replacement_marker(&new_body) {
        return false;
    }
    if explicitly_searches_old_fact(&old_body, &new_body, query) {
        return false;
    }

    topic_overlap(&old_body, &new_body) >= 2
}

fn same_scope(left: &IndexEntry, right: &IndexEntry) -> bool {
    left.scope == right.scope && left.project_id == right.project_id
}

fn is_newer(candidate: &IndexEntry, older: &IndexEntry) -> bool {
    timestamp_rank(&candidate.created_at) > timestamp_rank(&older.created_at)
}

fn timestamp_rank(value: &str) -> i128 {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map(|timestamp| timestamp.unix_timestamp_nanos())
        .unwrap_or_default()
}

fn has_stale_marker(body: &str) -> bool {
    contains_phrase(body, &["used", "to"])
        || contains_word(body, "previously")
        || contains_word(body, "formerly")
        || contains_word(body, "old")
        || contains_phrase(body, &["no", "longer"])
}

fn has_replacement_marker(body: &str) -> bool {
    contains_word(body, "now")
        || contains_word(body, "instead")
        || contains_word(body, "replaces")
        || contains_word(body, "replaced")
        || contains_word(body, "current")
}

fn contains_word(body: &str, word: &str) -> bool {
    body_tokens(body).any(|token| token == word)
}

fn contains_phrase(body: &str, words: &[&str]) -> bool {
    if words.is_empty() {
        return false;
    }
    let tokens = body_tokens(body).collect::<Vec<_>>();
    tokens.windows(words.len()).any(|window| window == words)
}

fn body_tokens(body: &str) -> impl Iterator<Item = &str> {
    body.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
        .filter(|token| !token.is_empty())
}

fn explicitly_searches_old_fact(old_body: &str, new_body: &str, query: Option<&str>) -> bool {
    let Some(query) = query else {
        return false;
    };
    query_tokens(query).into_iter().any(|term| {
        old_body.contains(&term) && !new_body.contains(&term) && !is_query_stopword(&term)
    })
}

fn topic_overlap(left: &str, right: &str) -> usize {
    let left = topic_tokens(left);
    let right = topic_tokens(right);
    left.intersection(&right).count()
}

fn topic_tokens(input: &str) -> BTreeSet<String> {
    query_tokens(input)
        .into_iter()
        .filter(|term| term.len() >= 4 && !is_query_stopword(term) && !is_marker_word(term))
        .collect()
}

fn query_tokens(input: &str) -> BTreeSet<String> {
    input
        .split_whitespace()
        .filter_map(|token| {
            let normalized = token
                .trim_matches(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
                .to_ascii_lowercase();
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect()
}

fn is_marker_word(term: &str) -> bool {
    matches!(
        term,
        "current"
            | "instead"
            | "longer"
            | "now"
            | "old"
            | "previously"
            | "replaced"
            | "replaces"
            | "used"
    )
}

fn is_query_stopword(term: &str) -> bool {
    matches!(
        term,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "before"
            | "for"
            | "from"
            | "how"
            | "i"
            | "in"
            | "is"
            | "it"
            | "of"
            | "on"
            | "or"
            | "should"
            | "the"
            | "to"
            | "what"
            | "when"
            | "where"
            | "which"
            | "with"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexEntry;

    fn entry(id: &str, body: &str, created_at: &str) -> IndexEntry {
        IndexEntry {
            id: id.to_owned(),
            store_id: "store".to_owned(),
            entry_kind: note::EntryKind::Remember,
            scope: "project".to_owned(),
            project_id: Some("project-alpha".to_owned()),
            audience: Vec::new(),
            tags: Vec::new(),
            subject: Some(id.to_owned()),
            confidence: note::Confidence::High,
            valid_from: None,
            valid_to: None,
            supersedes: Vec::new(),
            kind: Some(note::MemoryKind::ProjectFact),
            entities: Vec::new(),
            classified: None,
            agent_id: "eval".to_owned(),
            host_id: "ci".to_owned(),
            created_at: created_at.to_owned(),
            body: body.to_owned(),
            note_path: format!("inbox/notes/{id}.md"),
            event_path: None,
        }
    }

    #[test]
    fn suppresses_broad_recall_of_clear_replacement() {
        let old = entry(
            "old",
            "Project alpha used to run cargo fmt before committing.",
            "2026-06-01T00:00:00Z",
        );
        let new = entry(
            "new",
            "Project alpha now uses checkrun format and checkrun lint before committing.",
            "2026-06-02T00:00:00Z",
        );

        assert!(should_suppress_older(&old, &new, Some("before committing")));
    }

    #[test]
    fn keeps_old_record_when_query_names_old_fact() {
        let old = entry(
            "old",
            "Project alpha used to run cargo fmt before committing.",
            "2026-06-01T00:00:00Z",
        );
        let new = entry(
            "new",
            "Project alpha now uses checkrun format and checkrun lint before committing.",
            "2026-06-02T00:00:00Z",
        );

        assert!(!should_suppress_older(&old, &new, Some("cargo fmt")));
    }

    #[test]
    fn ignores_marker_substrings_inside_unrelated_words() {
        let old = entry(
            "old",
            "Project alpha told maintainers to run cargo fmt before committing.",
            "2026-06-01T00:00:00Z",
        );
        let new = entry(
            "new",
            "Project alpha currently asks maintainers to run checkrun format before committing.",
            "2026-06-02T00:00:00Z",
        );

        assert!(!should_suppress_older(
            &old,
            &new,
            Some("before committing")
        ));
    }
}
