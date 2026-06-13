//! Lightweight entity extraction for durable recall.
//!
//! This deliberately starts with deterministic, local rules. The goal is not
//! to infer every possible real-world entity; it is to canonicalize the stable
//! identifiers agents repeatedly use in developer workflows so retrieval can
//! match aliases such as "agent rules" -> `AGENTS.md` or "pre-landing gate" ->
//! `sley ready` without relaxing project/scope filters.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Canonical entity id extracted from memory text or a recall query.
pub type EntityId = String;

/// User-editable entity alias registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRegistry {
    entities: Vec<EntityDef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EntityDef {
    id: String,
    aliases: Vec<String>,
}

/// Failure while loading a user-editable entity alias registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityRegistryError {
    /// Registry file could not be read from disk.
    Read {
        /// Registry path that failed to load.
        path: PathBuf,
        /// Human-readable read failure.
        message: String,
    },
    /// Registry TOML could not be parsed.
    Parse {
        /// Registry path that failed to parse.
        path: PathBuf,
        /// Human-readable parser failure.
        message: String,
    },
    /// Registry contents were syntactically valid but semantically unusable.
    Invalid {
        /// Registry path that contained invalid data.
        path: PathBuf,
        /// Human-readable validation failure.
        message: String,
    },
}

impl Display for EntityRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, message } => {
                write!(
                    f,
                    "failed to read entity registry {}: {message}",
                    path.display()
                )
            }
            Self::Parse { path, message } => {
                write!(
                    f,
                    "failed to parse entity registry {}: {message}",
                    path.display()
                )
            }
            Self::Invalid { path, message } => {
                write!(f, "invalid entity registry {}: {message}", path.display())
            }
        }
    }
}

impl Error for EntityRegistryError {}

const BUILTIN_ENTITIES: &[(&str, &[&str])] = &[
    (
        "file:agents.md",
        &[
            "agents.md",
            "agent instructions",
            "agent rules",
            "coding agent instructions",
            "coding agent rules",
        ],
    ),
    (
        "tool:checkrun",
        &[
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
    ),
    (
        "tool:sley",
        &[
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
    ),
    (
        "file:cargo.toml",
        &[
            "cargo.toml",
            "cargo metadata",
            "crate metadata",
            "rust crate metadata",
        ],
    ),
];

impl EntityRegistry {
    /// Built-in aliases for common agent workflow entities.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            entities: BUILTIN_ENTITIES
                .iter()
                .map(|(id, aliases)| EntityDef {
                    id: (*id).to_owned(),
                    aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
                })
                .collect(),
        }
    }

    /// Load the built-in registry plus optional `entities.toml` from a store.
    pub fn load_for_store(store_root: &Path) -> Result<Self, EntityRegistryError> {
        let mut registry = Self::builtin();
        let path = store_root.join("entities.toml");
        registry.extend_from_optional_file(&path)?;
        Ok(registry)
    }

    fn extend_from_optional_file(&mut self, path: &Path) -> Result<(), EntityRegistryError> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(EntityRegistryError::Read {
                    path: path.to_path_buf(),
                    message: err.to_string(),
                });
            }
        };
        let parsed = toml::from_str::<EntityRegistryFile>(&contents).map_err(|err| {
            EntityRegistryError::Parse {
                path: path.to_path_buf(),
                message: err.to_string(),
            }
        })?;
        if parsed.schema_version != 1 {
            return Err(EntityRegistryError::Invalid {
                path: path.to_path_buf(),
                message: format!("unsupported schema_version {}", parsed.schema_version),
            });
        }
        for entity in parsed.entity {
            let id = entity.id.trim();
            if id.is_empty() {
                return Err(EntityRegistryError::Invalid {
                    path: path.to_path_buf(),
                    message: "entity id must not be empty".to_owned(),
                });
            }
            let aliases = entity
                .aliases
                .into_iter()
                .map(|alias| alias.trim().to_ascii_lowercase())
                .filter(|alias| !alias.is_empty())
                .collect::<Vec<_>>();
            if aliases.is_empty() {
                return Err(EntityRegistryError::Invalid {
                    path: path.to_path_buf(),
                    message: format!("entity {id} must define at least one alias"),
                });
            }
            self.entities.push(EntityDef {
                id: id.to_owned(),
                aliases,
            });
        }
        self.normalize();
        Ok(())
    }

    fn normalize(&mut self) {
        self.entities.sort_by(|left, right| left.id.cmp(&right.id));
        for entity in &mut self.entities {
            entity.aliases.sort();
            entity.aliases.dedup();
        }
        self.entities
            .dedup_by(|left, right| left.id == right.id && left.aliases == right.aliases);
    }
}

#[derive(Debug, Deserialize)]
struct EntityRegistryFile {
    schema_version: u32,
    #[serde(default)]
    entity: Vec<EntityRegistryEntry>,
}

#[derive(Debug, Deserialize)]
struct EntityRegistryEntry {
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
}

/// Extract canonical entities from free text.
pub fn extract(text: &str) -> Vec<EntityId> {
    extract_with_registry(text, &EntityRegistry::builtin())
}

/// Extract canonical entities from free text with the supplied registry.
pub fn extract_with_registry(text: &str, registry: &EntityRegistry) -> Vec<EntityId> {
    let lower = text.to_ascii_lowercase();
    let mut ids = BTreeSet::new();

    for def in &registry.entities {
        if def
            .aliases
            .iter()
            .any(|alias| contains_phrase(&lower, alias.as_str()))
        {
            ids.insert(def.id.clone());
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
        if let Some(number) = token
            .strip_prefix('#')
            .filter(|value| !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()))
        {
            ids.insert(format!("issue:{number}"));
        }
    }
    ids.extend(extract_quoted_phrases(text));
    ids.extend(extract_proper_name_phrases(text));

    ids.into_iter().collect()
}

/// Extract from multiple text fields as one canonical set.
pub fn extract_fields<'a>(fields: impl IntoIterator<Item = &'a str>) -> Vec<EntityId> {
    extract_fields_with_registry(fields, &EntityRegistry::builtin())
}

/// Extract from multiple text fields as one canonical set with a registry.
pub fn extract_fields_with_registry<'a>(
    fields: impl IntoIterator<Item = &'a str>,
    registry: &EntityRegistry,
) -> Vec<EntityId> {
    let mut ids = BTreeSet::new();
    for field in fields {
        ids.extend(extract_with_registry(field, registry));
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

fn extract_quoted_phrases(text: &str) -> Vec<EntityId> {
    let mut ids = BTreeSet::new();
    for quote in ['\'', '"'] {
        let mut remainder = text;
        while let Some(start) = remainder.find(quote) {
            let after_start = &remainder[start + quote.len_utf8()..];
            let Some(end) = after_start.find(quote) else {
                break;
            };
            insert_phrase_entity(&mut ids, &after_start[..end]);
            remainder = &after_start[end + quote.len_utf8()..];
        }
    }
    ids.into_iter().collect()
}

fn extract_proper_name_phrases(text: &str) -> Vec<EntityId> {
    let mut ids = BTreeSet::new();
    let mut current = Vec::<String>::new();
    let mut significant = 0usize;

    for raw in text.split_whitespace() {
        let Some(token) = phrase_token(raw) else {
            flush_phrase_entity(&mut ids, &mut current, &mut significant);
            continue;
        };
        if is_capitalized_phrase_token(&token) {
            current.push(token);
            significant += 1;
        } else if is_name_connector(&token) && !current.is_empty() {
            current.push(token);
        } else {
            flush_phrase_entity(&mut ids, &mut current, &mut significant);
        }
    }
    flush_phrase_entity(&mut ids, &mut current, &mut significant);

    ids.into_iter().collect()
}

fn flush_phrase_entity(
    ids: &mut BTreeSet<EntityId>,
    current: &mut Vec<String>,
    significant: &mut usize,
) {
    while current.last().is_some_and(|token| is_name_connector(token)) {
        current.pop();
    }
    if *significant >= 2 {
        insert_phrase_entity(ids, &current.join(" "));
    }
    current.clear();
    *significant = 0;
}

fn insert_phrase_entity(ids: &mut BTreeSet<EntityId>, phrase: &str) {
    let normalized = normalize_phrase_entity(phrase);
    if normalized
        .split_whitespace()
        .filter(|term| !is_name_connector(term))
        .count()
        >= 1
        && normalized.len() >= 3
    {
        ids.insert(format!("phrase:{normalized}"));
    }
}

fn normalize_phrase_entity(phrase: &str) -> String {
    phrase
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn phrase_token(token: &str) -> Option<String> {
    let trimmed = token
        .trim_matches(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '\'' | '-' | '.')));
    let trimmed = trimmed.trim_matches(|ch: char| matches!(ch, '\'' | '-' | '.'));
    (!trimmed.is_empty()).then_some(trimmed.to_owned())
}

fn is_capitalized_phrase_token(token: &str) -> bool {
    token
        .chars()
        .find(|ch| ch.is_ascii_alphabetic())
        .is_some_and(|ch| ch.is_ascii_uppercase())
}

fn is_name_connector(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "and" | "de" | "of" | "the"
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hive-memory-entity-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

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
    fn extracts_quoted_title_entities() {
        let ids = extract("I finished 'The Nightingale' before starting \"Dune\".");

        assert!(ids.contains(&"phrase:the nightingale".to_owned()));
        assert!(ids.contains(&"phrase:dune".to_owned()));
    }

    #[test]
    fn extracts_proper_name_phrase_entities() {
        let ids = extract("We visited the Museum of Modern Art with Kristin Hannah.");

        assert!(ids.contains(&"phrase:museum of modern art".to_owned()));
        assert!(ids.contains(&"phrase:kristin hannah".to_owned()));
    }

    #[test]
    fn respects_phrase_boundaries() {
        let ids = extract("the word agents.mdx is not the AGENTS.md file");

        assert!(ids.contains(&"file:agents.md".to_owned()));
        assert!(!ids.contains(&"file:agents.mdx".to_owned()));
    }

    #[test]
    fn loads_store_registry_aliases() {
        let root = temp_dir("store-registry");
        fs::write(
            root.join("entities.toml"),
            r#"
schema_version = 1

[[entity]]
id = "tool:deployctl"
aliases = ["DeployCtl", "release promotion gate"]
"#,
        )
        .expect("write registry");

        let registry = EntityRegistry::load_for_store(&root).expect("load registry");
        let ids = extract_with_registry("run the release promotion gate", &registry);

        assert_eq!(ids, vec!["tool:deployctl".to_owned()]);
    }

    #[test]
    fn rejects_registry_entities_without_aliases() {
        let root = temp_dir("bad-registry");
        fs::write(
            root.join("entities.toml"),
            r#"
schema_version = 1

[[entity]]
id = "tool:deployctl"
"#,
        )
        .expect("write registry");

        let err = EntityRegistry::load_for_store(&root).expect_err("registry should be invalid");

        assert!(err.to_string().contains("at least one alias"));
    }
}
