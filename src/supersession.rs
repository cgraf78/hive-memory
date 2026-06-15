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
use std::collections::{BTreeSet, HashMap};
use time::OffsetDateTime;

/// Upper bound on how many candidates the natural-language heuristic compares
/// pairwise. Explicit `supersedes` links are always resolved across the full set
/// (an O(n) id lookup); only the inherently O(n²) heuristic is windowed, mirroring
/// how `hm search` already caps its scan. Without this bound, `hm context` over a
/// large store ran the full O(n²) heuristic over every candidate (≈9.6s at 5000
/// notes). The window is generous enough to cover search's own ≤128 pre-window.
const HEURISTIC_SCAN_WINDOW: usize = 256;

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

    // Phase 1 — explicit `supersedes` links: authoritative and resolved across
    // the FULL set via direct id lookup, so a superseded record is hidden no
    // matter how large the store is or where the records rank. O(n + links).
    let by_id: HashMap<&str, &IndexEntry> = entries
        .iter()
        .map(|entry| (entry.id.as_str(), *entry))
        .collect();
    // Map every node that participates in an explicit-supersedes cycle (an SCC of
    // size >= 2, or a self-loop) to that cycle's single deterministic winner.
    // Cycles of any length must keep exactly one member, never erase all of them.
    let cycle_winners = cycle_winners(entries, &by_id);
    for newer in entries {
        for target_id in &newer.supersedes {
            if let Some(older) = by_id.get(target_id.as_str()) {
                if explicitly_searches_old_fact(
                    &older.body.to_ascii_lowercase(),
                    &newer.body.to_ascii_lowercase(),
                    query,
                ) {
                    continue;
                }
                // A node in a cycle is suppressed iff it is not its cycle's
                // winner; this generalizes the reciprocal A<->B special case to
                // cycles of any length. Acyclic links suppress their target.
                if let Some(winner) = cycle_winners.get(older.id.as_str()) {
                    if older.id.as_str() != *winner {
                        suppressed.insert(older.id.clone());
                    }
                } else {
                    suppressed.insert(older.id.clone());
                }
            }
        }
    }

    // Phase 2 — natural-language heuristic: inherently pairwise, so it is bounded
    // to the top window of candidates (already priority/recency ordered by the
    // caller). This is a best-effort lower-confidence layer; the authoritative
    // explicit links above are unaffected by the bound. Pairs already joined by
    // an explicit link are skipped here since Phase 1 owns them.
    let window = &entries[..entries.len().min(HEURISTIC_SCAN_WINDOW)];
    for older in window {
        if suppressed.contains(&older.id) {
            continue;
        }
        for newer in window {
            if newer.supersedes.iter().any(|id| id == &older.id) {
                continue;
            }
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

/// Map each entry that participates in an explicit-supersedes cycle to that
/// cycle's single surviving id.
///
/// The cycle is taken over the explicit `supersedes` graph restricted to the
/// present entries: an edge `newer -> older` exists when `newer.supersedes`
/// names a present `older`. A node is "in a cycle" when it belongs to a strongly
/// connected component of size >= 2, or it supersedes itself (a self-loop). The
/// winner is the maximum member by `(created_at, id)` — newest wins, ties broken
/// by the lexicographically larger id — so the choice is deterministic
/// regardless of input ordering. Acyclic nodes are absent from the map.
///
/// This generalizes the reciprocal A<->B guard to cycles of any length so a
/// hand-edited or imported cycle (A->B->C->A) can never erase all its members.
/// The graph is sparse, so this is cheap even at large entry counts.
fn cycle_winners(
    entries: &[&IndexEntry],
    by_id: &HashMap<&str, &IndexEntry>,
) -> HashMap<String, String> {
    let sccs = strongly_connected_components(entries, by_id);
    let mut winners = HashMap::new();
    for scc in sccs {
        // A trivial SCC (single node) is only a cycle when it has a self-loop.
        let is_cycle = scc.len() >= 2 || {
            let id = scc[0];
            by_id
                .get(id)
                .is_some_and(|entry| entry.supersedes.iter().any(|target| target == id))
        };
        if !is_cycle {
            continue;
        }
        let winner = scc
            .iter()
            .filter_map(|id| by_id.get(*id).copied())
            .max_by(|left, right| {
                timestamp_rank(&left.created_at)
                    .cmp(&timestamp_rank(&right.created_at))
                    .then_with(|| left.id.cmp(&right.id))
            })
            .map(|entry| entry.id.clone());
        if let Some(winner) = winner {
            for id in scc {
                winners.insert(id.to_owned(), winner.clone());
            }
        }
    }
    winners
}

/// Compute strongly connected components of the present-entry explicit-supersedes
/// graph using an iterative Tarjan to avoid recursion blowups on deep chains.
///
/// Returns one `Vec` of node ids per component. Only edges into present entries
/// are followed, so the graph is exactly the subgraph the resolver can act on.
fn strongly_connected_components<'a>(
    entries: &[&'a IndexEntry],
    by_id: &HashMap<&str, &'a IndexEntry>,
) -> Vec<Vec<&'a str>> {
    #[derive(Clone, Copy)]
    struct NodeState {
        index: usize,
        lowlink: usize,
        on_stack: bool,
    }

    // Stable node ordering keeps SCC output deterministic across runs.
    let nodes: Vec<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
    let mut state: HashMap<&str, NodeState> = HashMap::with_capacity(nodes.len());
    let mut stack: Vec<&str> = Vec::new();
    let mut next_index = 0usize;
    let mut components: Vec<Vec<&str>> = Vec::new();

    // Successors of `node`: the present records it explicitly supersedes.
    let successors = |node: &str| -> Vec<&'a str> {
        by_id
            .get(node)
            .map(|entry| {
                entry
                    .supersedes
                    .iter()
                    .filter_map(|target| by_id.get(target.as_str()).map(|found| found.id.as_str()))
                    .collect()
            })
            .unwrap_or_default()
    };

    // Explicit DFS frame stack: `(node, next successor index)`.
    for &root in &nodes {
        if state.contains_key(root) {
            continue;
        }
        let mut frames: Vec<(&str, usize, Vec<&str>)> = vec![(root, 0, successors(root))];
        state.insert(
            root,
            NodeState {
                index: next_index,
                lowlink: next_index,
                on_stack: true,
            },
        );
        next_index += 1;
        stack.push(root);

        while let Some((node, child_idx, succs)) = frames.last_mut() {
            if *child_idx < succs.len() {
                let child = succs[*child_idx];
                *child_idx += 1;
                if let Some(child_state) = state.get(child).copied() {
                    if child_state.on_stack {
                        let node_index = state[*node].index;
                        let candidate = child_state.index.min(node_index);
                        let entry = state.get_mut(node).expect("node state present");
                        entry.lowlink = entry.lowlink.min(candidate);
                    }
                } else {
                    state.insert(
                        child,
                        NodeState {
                            index: next_index,
                            lowlink: next_index,
                            on_stack: true,
                        },
                    );
                    next_index += 1;
                    stack.push(child);
                    let child_succs = successors(child);
                    frames.push((child, 0, child_succs));
                }
            } else {
                let node = *node;
                let node_state = state[node];
                if node_state.lowlink == node_state.index {
                    let mut component = Vec::new();
                    while let Some(top) = stack.pop() {
                        if let Some(entry) = state.get_mut(top) {
                            entry.on_stack = false;
                        }
                        component.push(top);
                        if top == node {
                            break;
                        }
                    }
                    components.push(component);
                }
                frames.pop();
                // Propagate this node's lowlink up to its parent frame.
                if let Some((parent, _, _)) = frames.last() {
                    let parent = *parent;
                    let node_lowlink = state[node].lowlink;
                    let parent_entry = state.get_mut(parent).expect("parent state present");
                    parent_entry.lowlink = parent_entry.lowlink.min(node_lowlink);
                }
            }
        }
    }

    components
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

    #[test]
    fn three_cycle_keeps_exactly_one_deterministic_winner() {
        // A 3-node explicit cycle A->B->C->A must not erase every member; the
        // pairwise reciprocal guard only caught 2-cycles. Exactly one survives:
        // the newest by (created_at, id) over the whole cycle.
        let mut a = entry("a", "Fact A.", "2026-06-01T00:00:00Z");
        let mut b = entry("b", "Fact B.", "2026-06-02T00:00:00Z");
        let mut c = entry("c", "Fact C.", "2026-06-03T00:00:00Z");
        a.supersedes = vec!["b".to_owned()];
        b.supersedes = vec!["c".to_owned()];
        c.supersedes = vec!["a".to_owned()];
        let entries = [&a, &b, &c];

        let suppressed = suppressed_ids(&entries, None);

        // "c" is newest, so it wins; "a" and "b" are suppressed.
        assert_eq!(suppressed.len(), 2);
        assert!(suppressed.contains("a"));
        assert!(suppressed.contains("b"));
        assert!(!suppressed.contains("c"));
    }

    #[test]
    fn three_chain_suppresses_all_but_head() {
        // An acyclic chain A->B->C (A supersedes B, B supersedes C) is NOT a
        // cycle: A is the live head and both B and C are superseded.
        let mut a = entry("a", "Fact A.", "2026-06-03T00:00:00Z");
        let mut b = entry("b", "Fact B.", "2026-06-02T00:00:00Z");
        let c = entry("c", "Fact C.", "2026-06-01T00:00:00Z");
        a.supersedes = vec!["b".to_owned()];
        b.supersedes = vec!["c".to_owned()];
        let entries = [&a, &b, &c];

        let suppressed = suppressed_ids(&entries, None);

        assert_eq!(suppressed.len(), 2);
        assert!(suppressed.contains("b"));
        assert!(suppressed.contains("c"));
        assert!(!suppressed.contains("a"));
    }

    #[test]
    fn explicit_link_suppresses_beyond_heuristic_window() {
        // Phase 1 (explicit links) is unbounded: an explicit correction whose
        // target sits past HEURISTIC_SCAN_WINDOW still suppresses it. Build a
        // list where the superseded target is the very last entry, well beyond
        // the window, and the newer record naming it is first.
        let count = HEURISTIC_SCAN_WINDOW + 50;
        let mut owned = Vec::with_capacity(count);
        // index 0 is the newer correction; the last index is its explicit target.
        let target_id = format!("filler-{}", count - 1);
        let mut newer = entry("newer", "Replacement fact.", "2026-06-02T00:00:00Z");
        newer.supersedes = vec![target_id.clone()];
        owned.push(newer);
        for i in 1..count {
            owned.push(entry(
                &format!("filler-{i}"),
                "Unrelated filler body.",
                "2026-06-01T00:00:00Z",
            ));
        }
        let entries: Vec<&IndexEntry> = owned.iter().collect();

        let suppressed = suppressed_ids(&entries, None);

        assert!(suppressed.contains(target_id.as_str()));
    }

    #[test]
    fn heuristic_is_bounded_to_scan_window() {
        // Phase 2 (NL heuristic) is bounded. A clear stale/replacement pair whose
        // older record sits just beyond the window is NOT suppressed; the same
        // pair within the window IS. This proves the bound is real and that the
        // explicit-link unbounded case above is genuinely Phase 1, not Phase 2.
        let old_body = "Project alpha used to run cargo fmt before committing.";
        let new_body = "Project alpha now uses checkrun format before committing.";

        // Out of window: place the older record past the window, the newer first.
        let count = HEURISTIC_SCAN_WINDOW + 10;
        let mut owned = Vec::with_capacity(count);
        owned.push(entry("newer", new_body, "2026-06-02T00:00:00Z"));
        for i in 1..(count - 1) {
            owned.push(entry(
                &format!("filler-{i}"),
                "Unrelated filler body.",
                "2026-06-01T00:00:00Z",
            ));
        }
        owned.push(entry("older", old_body, "2026-06-01T00:00:00Z"));
        let entries: Vec<&IndexEntry> = owned.iter().collect();
        let suppressed = suppressed_ids(&entries, None);
        assert!(
            !suppressed.contains("older"),
            "heuristic must not reach beyond the scan window"
        );

        // Within window: the same pair adjacent at the top IS suppressed.
        let newer = entry("newer", new_body, "2026-06-02T00:00:00Z");
        let older = entry("older", old_body, "2026-06-01T00:00:00Z");
        let near = [&newer, &older];
        let near_suppressed = suppressed_ids(&near, None);
        assert!(near_suppressed.contains("older"));
    }
}
