//! Lightweight entity extraction for durable recall.
//!
//! This deliberately starts with deterministic, local rules. The goal is not
//! to infer every possible real-world entity; it is to canonicalize the stable
//! identifiers agents repeatedly use in developer workflows so retrieval can
//! match aliases such as "agent rules" -> `AGENTS.md` or "pre-landing gate" ->
//! `sley ready` without relaxing project/scope filters.

use std::collections::BTreeSet;

/// Canonical entity id extracted from memory text or a recall query.
pub type EntityId = String;

struct EntityDef {
    id: &'static str,
    aliases: &'static [&'static str],
}

const ENTITIES: &[EntityDef] = &[
    EntityDef {
        id: "file:agents.md",
        aliases: &[
            "agents.md",
            "agent instructions",
            "agent rules",
            "coding agent instructions",
            "coding agent rules",
        ],
    },
    EntityDef {
        id: "tool:checkrun",
        aliases: &[
            "checkrun",
            "checkrun format",
            "checkrun lint",
            "chkfmt",
            "chklint",
            "format and lint",
            "validation command",
            "validation commands",
            "validate changes",
            "validates changes",
            "verification command",
            "verification commands",
        ],
    },
    EntityDef {
        id: "tool:sley",
        aliases: &[
            "sley",
            "sley ready",
            "commit gate",
            "landing gate",
            "pre landing gate",
            "pre-landing gate",
            "pre landing verification",
            "pre-landing verification",
            "ready gate",
        ],
    },
    EntityDef {
        id: "file:cargo.toml",
        aliases: &[
            "cargo.toml",
            "cargo metadata",
            "crate metadata",
            "rust crate metadata",
        ],
    },
];

/// Extract canonical entities from free text.
pub fn extract(text: &str) -> Vec<EntityId> {
    let lower = text.to_ascii_lowercase();
    let mut ids = BTreeSet::new();

    for def in ENTITIES {
        if def
            .aliases
            .iter()
            .any(|alias| contains_phrase(&lower, alias))
        {
            ids.insert(def.id.to_owned());
        }
    }

    for token in lower.split_whitespace().filter_map(normalize_token) {
        if token.contains('.') {
            match token.as_str() {
                "agents.md" => {
                    ids.insert("file:agents.md".to_owned());
                }
                "cargo.toml" => {
                    ids.insert("file:cargo.toml".to_owned());
                }
                _ => {
                    if is_likely_filename(&token) {
                        ids.insert(format!("file:{token}"));
                    }
                }
            }
        }
        if let Some(number) = token.strip_prefix('#').filter(|value| {
            !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit())
        }) {
            ids.insert(format!("issue:{number}"));
        }
    }

    ids.into_iter().collect()
}

/// Extract from multiple text fields as one canonical set.
pub fn extract_fields<'a>(fields: impl IntoIterator<Item = &'a str>) -> Vec<EntityId> {
    let mut ids = BTreeSet::new();
    for field in fields {
        ids.extend(extract(field));
    }
    ids.into_iter().collect()
}

fn normalize_token(token: &str) -> Option<String> {
    let normalized = token
        .trim_matches(|ch: char| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/' | '#'))
        })
        .to_ascii_lowercase();
    (!normalized.is_empty()).then_some(normalized)
}

fn is_likely_filename(token: &str) -> bool {
    matches!(
        token.rsplit_once('.').map(|(_, ext)| ext),
        Some("md" | "toml" | "json" | "yaml" | "yml" | "rs" | "sh" | "py")
    )
}

fn contains_phrase(lower: &str, phrase: &str) -> bool {
    let mut offset = 0usize;
    while let Some(relative_index) = lower[offset..].find(phrase) {
        let index = offset + relative_index;
        let end = index + phrase.len();
        if boundary_allows(lower.as_bytes(), index, end) {
            return true;
        }
        offset = end;
    }
    false
}

fn boundary_allows(bytes: &[u8], start: usize, end: usize) -> bool {
    let left_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
    let right_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
    left_ok && right_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_known_agent_workflow_entities() {
        let ids = extract("Run chkfmt and chklint, then check AGENTS.md.");

        assert!(ids.contains(&"tool:checkrun".to_owned()));
        assert!(ids.contains(&"file:agents.md".to_owned()));
    }

    #[test]
    fn extracts_entities_from_query_aliases() {
        let ids = extract("what is the pre landing verification gate?");

        assert_eq!(ids, vec!["tool:sley".to_owned()]);
    }

    #[test]
    fn respects_phrase_boundaries() {
        let ids = extract("the word agents.mdx is not the AGENTS.md file");

        assert!(ids.contains(&"file:agents.md".to_owned()));
        assert!(!ids.contains(&"file:agents.mdx".to_owned()));
    }
}
