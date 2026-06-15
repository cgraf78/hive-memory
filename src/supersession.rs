//! Conservative stale-memory suppression.
//!
//! This module does not rewrite canonical memory. It only answers whether a
//! newer indexed record is strong enough evidence to hide an older record from
//! broad recall results. There are two clearly separated layers:
//!
//! 1. **Explicit `supersedes` links** are authoritative. When one record's
//!    `supersedes` list names another record, the named target is suppressed
//!    regardless of scope or entry kind. This is the durable contract a writer
//!    opts into; a correction written as a `note`, or one that moves scope or
//!    project, still hides the fact it explicitly replaces.
//! 2. **The natural-language heuristic** is a strictly lower-confidence
//!    fallback used only when there is no explicit link. It stays narrow
//!    (same scope AND both records `Remember`, plus stale/replacement markers
//!    and topic-word overlap) so it never silently rewrites memory on a guess.
//!    The heuristic MUST NOT relax the explicit-link rules; it only adds
//!    suppression the explicit layer did not already provide.
//!
//! Both layers keep one shared exception: when a query explicitly names a token
//! that lives only in the older fact, historical recall wins and the older
//! record stays visible. Context has no query, so that exception is always
//! inactive there and current truth is always what gets injected.

use crate::{index::IndexEntry, note};
use std::collections::BTreeSet;
use time::OffsetDateTime;

/// Compute the set of record ids that should be hidden from broad recall.
///
/// This is the single source of truth shared by `hm search` and `hm context`.
/// Search passes the lowercased query phrase so the historical-recall exception
/// can keep an explicitly named old fact visible; context passes `None` because
/// broad context must always reflect current truth.
///
/// The returned ids are a subset of the input entries' ids. Callers filter
/// their own result lists against this set, so the function never reorders or
/// clones the caller's records.
#[must_use]
pub fn suppressed_ids(entries: &[&IndexEntry], query: Option<&str>) -> BTreeSet<String> {
    let mut suppressed = BTreeSet::new();
    for older in entries {
        for newer in entries {
            if should_suppress_older(older, newer, query) {
                suppressed.insert(older.id.clone());
                break;
            }
        }
    }
    suppressed
}

/// Return whether `older` should be suppressed when `newer` is also present.
///
/// Resolution order:
///
/// 1. An explicit `supersedes` link is authoritative across scope and entry
///    kind, subject only to the historical-query exception and to cycle
///    resolution (a reciprocal A↔B link suppresses only the deterministic
///    loser, never both members).
/// 2. Otherwise the conservative natural-language heuristic applies, gated on
///    same scope, both records being `Remember`, stale/replacement markers, and
///    topic-word overlap.
#[must_use]
pub fn should_suppress_older(older: &IndexEntry, newer: &IndexEntry, query: Option<&str>) -> bool {
    if older.id == newer.id {
        return false;
    }

    if newer.supersedes.iter().any(|id| id == &older.id) {
        return suppress_for_explicit_link(older, newer, query);
    }

    suppress_for_heuristic(older, newer, query)
}

/// Decide suppression for an explicit `supersedes` link.
///
/// Authoritative across scope and entry kind. Two guards apply:
///
/// - the historical-query exception keeps the older record when the query names
///   a token unique to it; and
/// - cycle resolution: when the link is reciprocal (`older` also supersedes
///   `newer`), suppress only the deterministic loser so a hand-edited or
///   imported A↔B cycle can never erase both records.
fn suppress_for_explicit_link(older: &IndexEntry, newer: &IndexEntry, query: Option<&str>) -> bool {
    if explicitly_searches_old_fact(
        &older.body.to_ascii_lowercase(),
        &newer.body.to_ascii_lowercase(),
        query,
    ) {
        return false;
    }

    let reciprocal = older.supersedes.iter().any(|id| id == &newer.id);
    if reciprocal {
        // A cycle would otherwise make this true in both directions and drop
        // every member. Keep exactly one deterministic winner: the newer record
        // by timestamp, tie-broken by larger id. Suppress only the loser.
        return &older.id != cycle_winner_id(older, newer);
    }

    true
}

/// Pick the surviving record id for a reciprocal explicit cycle.
///
/// Newer-by-timestamp wins; ties break toward the lexicographically larger id
/// so the choice is deterministic regardless of input ordering.
fn cycle_winner_id<'a>(left: &'a IndexEntry, right: &'a IndexEntry) -> &'a String {
    let left_rank = timestamp_rank(&left.created_at);
    let right_rank = timestamp_rank(&right.created_at);
    match left_rank.cmp(&right_rank) {
        std::cmp::Ordering::Greater => &left.id,
        std::cmp::Ordering::Less => &right.id,
        std::cmp::Ordering::Equal => {
            if left.id >= right.id {
                &left.id
            } else {
                &right.id
            }
        }
    }
}

/// Decide suppression for the lower-confidence natural-language heuristic.
///
/// This path never fires when an explicit link exists; it only adds suppression
/// the explicit layer could not. It stays deliberately conservative so a guess
/// from prose markers cannot hide a fact across scope or entry-kind boundaries.
fn suppress_for_heuristic(older: &IndexEntry, newer: &IndexEntry, query: Option<&str>) -> bool {
    if older.entry_kind != note::EntryKind::Remember
        || newer.entry_kind != note::EntryKind::Remember
        || !same_scope(older, newer)
        || !is_newer(newer, older)
    {
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

    #[test]
    fn explicit_link_suppresses_across_scope() {
        // The older record lives in a different scope/project than the newer
        // one. The natural-language heuristic would refuse (different scope),
        // but an explicit link is authoritative and must still suppress.
        let mut old = entry(
            "old",
            "Deploys use the manual checklist.",
            "2026-06-01T00:00:00Z",
        );
        old.scope = "global".to_owned();
        old.project_id = None;
        let mut new = entry(
            "new",
            "Deploys use the checkrun gate.",
            "2026-06-02T00:00:00Z",
        );
        new.supersedes = vec!["old".to_owned()];

        assert!(should_suppress_older(&old, &new, Some("deploys")));
    }

    #[test]
    fn explicit_link_suppresses_across_entry_kind() {
        // A correction written as a `note` (not `remember`) must still suppress
        // its explicit target. The heuristic would refuse on entry kind alone.
        let mut old = entry("old", "Release uses cargo-dist.", "2026-06-01T00:00:00Z");
        old.entry_kind = note::EntryKind::Remember;
        let mut new = entry("new", "Release uses cargo-release.", "2026-06-02T00:00:00Z");
        new.entry_kind = note::EntryKind::Note;
        new.supersedes = vec!["old".to_owned()];

        assert!(should_suppress_older(&old, &new, Some("release")));
    }

    #[test]
    fn explicit_cycle_keeps_deterministic_winner() {
        // Reciprocal A↔B links from a hand-edit/import. Exactly one record must
        // survive: the newer by timestamp.
        let mut older = entry("a", "Fact A.", "2026-06-01T00:00:00Z");
        let mut newer = entry("b", "Fact B.", "2026-06-02T00:00:00Z");
        older.supersedes = vec!["b".to_owned()];
        newer.supersedes = vec!["a".to_owned()];

        // The older loses to the newer winner.
        assert!(should_suppress_older(&older, &newer, None));
        // The newer winner is never suppressed by the older loser.
        assert!(!should_suppress_older(&newer, &older, None));
    }

    #[test]
    fn explicit_cycle_tie_breaks_on_id() {
        // Identical timestamps: the larger id wins deterministically so the
        // cycle still drops exactly one record.
        let mut a = entry("a", "Fact A.", "2026-06-01T00:00:00Z");
        let mut b = entry("b", "Fact B.", "2026-06-01T00:00:00Z");
        a.supersedes = vec!["b".to_owned()];
        b.supersedes = vec!["a".to_owned()];

        // "b" > "a", so "b" wins and "a" is suppressed.
        assert!(should_suppress_older(&a, &b, None));
        assert!(!should_suppress_older(&b, &a, None));
    }

    #[test]
    fn suppressed_ids_drops_only_cycle_loser() {
        let mut a = entry("a", "Fact A.", "2026-06-01T00:00:00Z");
        let mut b = entry("b", "Fact B.", "2026-06-02T00:00:00Z");
        a.supersedes = vec!["b".to_owned()];
        b.supersedes = vec!["a".to_owned()];
        let entries = [&a, &b];

        let suppressed = suppressed_ids(&entries, None);

        assert_eq!(suppressed.len(), 1);
        assert!(suppressed.contains("a"));
        assert!(!suppressed.contains("b"));
    }
}
