//! Local full-text retrieval backend (Tantivy BM25).
//!
//! This is the candidate-generation engine behind `hm search` and `hm context`.
//! It exists because the deterministic lexical scan in [`crate::search`] cannot
//! recover paraphrased or vocabulary-shifted matches, which is the dominant
//! retrieval gap on long-lived, multi-session stores (see
//! `plans/bm25-experiment-results.md`: BM25 lifts LongMemEval recall@5 from 0.781
//! to 0.904 overall, and 0.571 to 0.714 on multi-session questions).
//!
//! Design boundaries (`plans/full-text-retrieval.md`):
//! - Canonical memory stays plain files. This index is a rebuildable cache: a
//!   caller may delete it at any time and rebuild from canonical records without
//!   data loss.
//! - The index returns ranked record ids plus scores only. Store/scope/project/
//!   audience policy stays in `hm`; callers map ids back to their own
//!   [`crate::index::IndexEntry`] set and apply policy post-filtering. The index
//!   is a speed/recall optimization, never a safety boundary.
//! - Query text is treated as natural language: query-syntax metacharacters are
//!   stripped so raw agent prompts and code-heavy text cannot trigger the parser.
//!
//! This first phase ships the backend (document model, schema, rebuild, query,
//! and a freshness manifest) but does not yet route `hm search`/`hm context`
//! through it; that wiring is a separate change so each step is measured on its
//! own.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, Value};
use tantivy::{Index, IndexWriter, TantivyDocument};

/// Schema version for the search-document contract and Tantivy field layout.
///
/// Any incompatible change to the fields below must bump this so a stale on-disk
/// index is rebuilt rather than read with the wrong shape.
pub const SEARCH_SCHEMA_VERSION: u32 = 1;

/// Filename of the freshness/compatibility manifest written beside a dir index.
const MANIFEST_FILE: &str = "manifest.json";

/// Tantivy writer heap budget. The minimum Tantivy accepts is ~3 MB; this gives
/// comfortable headroom for batched full rebuilds without being wasteful.
const WRITER_HEAP_BYTES: usize = 50_000_000;

/// One record to index. Built from a canonical note/curated file by the caller;
/// only the fields that participate in text retrieval are carried here.
///
/// `id` is the stable record id used to map a hit back to the caller's policy
/// metadata. `subject` and `tags` are boosted over `body` at query time.
#[derive(Debug, Clone)]
pub struct SearchDocument {
    /// Stable record id, returned verbatim in [`RankedHit::id`].
    pub id: String,
    /// Optional short subject/title; boosted above body text.
    pub subject: Option<String>,
    /// Tags; boosted above body text.
    pub tags: Vec<String>,
    /// Full record body.
    pub body: String,
}

/// One ranked retrieval hit: a record id and its BM25 relevance score.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedHit {
    /// The matched record id.
    pub id: String,
    /// BM25 score; higher is more relevant. Only meaningful relative to peers.
    pub score: f32,
}

/// Freshness/compatibility manifest persisted next to a directory-backed index.
///
/// It is intentionally tiny and cheap to read so an opener can decide to rebuild
/// on a schema mismatch without scanning the index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchManifest {
    /// Manifest envelope version.
    pub schema_version: u32,
    /// Search-document/Tantivy-schema version this index was built with.
    pub search_schema_version: u32,
    /// Number of documents in the last successful rebuild.
    pub document_count: usize,
    /// Opaque content fingerprint of the corpus the index was built from. The
    /// caller supplies it (typically derived from the source records); when it is
    /// unchanged the index is fresh and a rebuild can be skipped.
    #[serde(default)]
    pub fingerprint: Option<String>,
}

/// A retrieval failure. Tantivy errors are flattened to a message because the
/// caller only needs to report or fall back, not branch on engine internals.
#[derive(Debug)]
pub enum RetrievalError {
    /// The Tantivy engine or its directory backend failed.
    Engine(String),
    /// Reading or writing the freshness manifest failed.
    Manifest {
        /// Manifest path involved.
        path: PathBuf,
        /// Underlying cause.
        message: String,
    },
}

impl std::fmt::Display for RetrievalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine(message) => write!(formatter, "retrieval engine error: {message}"),
            Self::Manifest { path, message } => {
                write!(
                    formatter,
                    "retrieval manifest error at {}: {message}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for RetrievalError {}

/// Field handles for the index schema, resolved once at construction.
#[derive(Debug, Clone, Copy)]
struct Fields {
    id: Field,
    subject: Field,
    tags: Field,
    body: Field,
}

/// A local BM25 full-text index over [`SearchDocument`]s.
///
/// Construct with [`SearchIndex::in_memory`] (tests, ephemeral use) or
/// [`SearchIndex::open_or_create_in_dir`] (durable rebuildable cache), populate
/// with [`SearchIndex::rebuild`], then [`SearchIndex::query`].
pub struct SearchIndex {
    index: Index,
    fields: Fields,
    /// Set when the index is directory-backed; enables manifest persistence.
    dir: Option<PathBuf>,
}

impl SearchIndex {
    /// Build the Tantivy schema and return it with resolved field handles.
    ///
    /// `id` is `STRING | STORED`: untokenized for exact round-trip, stored so a
    /// hit can be mapped back to caller metadata. The text fields are tokenized
    /// (Tantivy's default analyzer: Unicode segmentation + lowercasing), which
    /// matches the BM25 experiment's tokenizer closely enough to reproduce its
    /// recall. A coding-aware tokenizer (`AGENTS.md`, `Cargo.toml`, `g<C-x>`) is
    /// a later refinement tracked in `plans/full-text-retrieval.md`.
    fn build_schema() -> (Schema, Fields) {
        let mut builder = Schema::builder();
        let id = builder.add_text_field("id", STRING | STORED);
        let subject = builder.add_text_field("subject", TEXT);
        let tags = builder.add_text_field("tags", TEXT);
        let body = builder.add_text_field("body", TEXT);
        let schema = builder.build();
        (
            schema,
            Fields {
                id,
                subject,
                tags,
                body,
            },
        )
    }

    /// Create an in-memory index. Useful for tests and one-shot ranking; nothing
    /// is persisted and no manifest is written.
    pub fn in_memory() -> Result<Self, RetrievalError> {
        let (schema, fields) = Self::build_schema();
        let index = Index::create_in_ram(schema);
        Ok(Self {
            index,
            fields,
            dir: None,
        })
    }

    /// Open a directory-backed index, creating it if absent.
    ///
    /// If an existing index's manifest reports an incompatible
    /// [`SEARCH_SCHEMA_VERSION`], the directory is wiped and recreated so a stale
    /// cache never silently serves results under the wrong schema. Because the
    /// index is a rebuildable cache, discarding it is always safe.
    pub fn open_or_create_in_dir(dir: &Path) -> Result<Self, RetrievalError> {
        std::fs::create_dir_all(dir).map_err(|err| RetrievalError::Engine(err.to_string()))?;

        if Self::manifest_is_incompatible(dir) {
            Self::clear_dir(dir)?;
        }

        let (schema, fields) = Self::build_schema();
        let directory =
            MmapDirectory::open(dir).map_err(|err| RetrievalError::Engine(err.to_string()))?;
        let index = Index::open_or_create(directory, schema)
            .map_err(|err| RetrievalError::Engine(err.to_string()))?;
        Ok(Self {
            index,
            fields,
            dir: Some(dir.to_path_buf()),
        })
    }

    /// Replace the entire index contents with `documents` in one atomic commit.
    ///
    /// This is a full rebuild: it deletes all existing documents, adds the new
    /// set, and commits. Incremental, receipt-driven updates are a later phase;
    /// the full-rebuild contract is the simple, correct starting point. On a
    /// directory-backed index a manifest is written after the commit succeeds.
    /// Returns the number of documents indexed. `fingerprint` is recorded in the
    /// manifest so a later open can skip the rebuild when the corpus is unchanged
    /// (see [`SearchIndex::is_fresh`]).
    pub fn rebuild_tagged(
        &self,
        documents: &[SearchDocument],
        fingerprint: Option<&str>,
    ) -> Result<usize, RetrievalError> {
        let mut writer: IndexWriter = self
            .index
            .writer(WRITER_HEAP_BYTES)
            .map_err(|err| RetrievalError::Engine(err.to_string()))?;
        writer
            .delete_all_documents()
            .map_err(|err| RetrievalError::Engine(err.to_string()))?;

        for document in documents {
            let mut tantivy_document = TantivyDocument::default();
            tantivy_document.add_text(self.fields.id, &document.id);
            if let Some(subject) = &document.subject {
                tantivy_document.add_text(self.fields.subject, subject);
            }
            if !document.tags.is_empty() {
                tantivy_document.add_text(self.fields.tags, document.tags.join(" "));
            }
            tantivy_document.add_text(self.fields.body, &document.body);
            writer
                .add_document(tantivy_document)
                .map_err(|err| RetrievalError::Engine(err.to_string()))?;
        }

        writer
            .commit()
            .map_err(|err| RetrievalError::Engine(err.to_string()))?;

        if let Some(dir) = &self.dir {
            Self::write_manifest(
                dir,
                &SearchManifest {
                    schema_version: 1,
                    search_schema_version: SEARCH_SCHEMA_VERSION,
                    document_count: documents.len(),
                    fingerprint: fingerprint.map(str::to_owned),
                },
            )?;
        }
        Ok(documents.len())
    }

    /// Full rebuild without a content fingerprint. Convenience for callers that
    /// manage freshness themselves or use an ephemeral in-memory index.
    pub fn rebuild(&self, documents: &[SearchDocument]) -> Result<usize, RetrievalError> {
        self.rebuild_tagged(documents, None)
    }

    /// True when a directory-backed index's manifest fingerprint matches
    /// `fingerprint`, meaning the cache already reflects the current corpus and a
    /// rebuild can be skipped. Always false for an in-memory index or an index
    /// built without a fingerprint.
    pub fn is_fresh(&self, fingerprint: &str) -> bool {
        self.manifest()
            .and_then(|manifest| manifest.fingerprint)
            .is_some_and(|stored| stored == fingerprint)
    }

    /// Retrieve up to `limit` records ranked by BM25 relevance to `query`.
    ///
    /// Query text is sanitized to natural-language tokens (see
    /// [`sanitize_query`]) so raw prompts never reach the query parser as
    /// operators. Terms combine disjunctively (any term may match), with BM25
    /// ranking the overlap — the behavior that produced the experiment's recall
    /// gain. Subject and tags are boosted above body. Returns an empty list when
    /// the query has no usable terms.
    pub fn query(&self, query: &str, limit: usize) -> Result<Vec<RankedHit>, RetrievalError> {
        let sanitized = sanitize_query(query);
        if sanitized.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let reader = self
            .index
            .reader()
            .map_err(|err| RetrievalError::Engine(err.to_string()))?;
        let searcher = reader.searcher();

        let mut parser = QueryParser::for_index(
            &self.index,
            vec![self.fields.subject, self.fields.tags, self.fields.body],
        );
        // Favor a record whose subject/tags name the topic over an incidental
        // body mention, without letting either dominate exact body relevance.
        parser.set_field_boost(self.fields.subject, 2.0);
        parser.set_field_boost(self.fields.tags, 1.5);

        // Lenient parsing best-effort-tokenizes natural language and never hard
        // fails on stray punctuation (e.g. an apostrophe in "shelter's"),
        // honoring the contract that retrieval must not error on a raw agent
        // prompt. Recoverable parse diagnostics are intentionally ignored.
        let (parsed, _diagnostics) = parser.parse_query_lenient(&sanitized);
        let top_docs = searcher
            .search(&parsed, &TopDocs::with_limit(limit).order_by_score())
            .map_err(|err| RetrievalError::Engine(err.to_string()))?;

        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, address) in top_docs {
            let document: TantivyDocument = searcher
                .doc(address)
                .map_err(|err| RetrievalError::Engine(err.to_string()))?;
            if let Some(id) = document
                .get_first(self.fields.id)
                .and_then(|value| value.as_str())
            {
                hits.push(RankedHit {
                    id: id.to_owned(),
                    score,
                });
            }
        }
        Ok(hits)
    }

    /// Read the persisted manifest for a directory-backed index, if any.
    pub fn manifest(&self) -> Option<SearchManifest> {
        let dir = self.dir.as_ref()?;
        Self::read_manifest(dir)
    }

    fn manifest_path(dir: &Path) -> PathBuf {
        dir.join(MANIFEST_FILE)
    }

    fn read_manifest(dir: &Path) -> Option<SearchManifest> {
        let text = std::fs::read_to_string(Self::manifest_path(dir)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn write_manifest(dir: &Path, manifest: &SearchManifest) -> Result<(), RetrievalError> {
        let path = Self::manifest_path(dir);
        let text =
            serde_json::to_string_pretty(manifest).map_err(|err| RetrievalError::Manifest {
                path: path.clone(),
                message: err.to_string(),
            })?;
        // Write atomically (temp file in the same dir, then rename) so a crash or
        // failed write never leaves a populated index dir with a missing/partial
        // manifest — which `manifest_is_incompatible` would otherwise treat as
        // unknown and wipe, discarding a perfectly good committed index.
        let temp = dir.join(format!("{MANIFEST_FILE}.tmp"));
        let manifest_error = |err: std::io::Error| RetrievalError::Manifest {
            path: path.clone(),
            message: err.to_string(),
        };
        std::fs::write(&temp, text).map_err(manifest_error)?;
        std::fs::rename(&temp, &path).map_err(manifest_error)
    }

    /// True when a directory already holds an index whose manifest is missing or
    /// reports a different schema version than this build can read.
    fn manifest_is_incompatible(dir: &Path) -> bool {
        // An empty/new directory has nothing to be incompatible with.
        if !Self::manifest_path(dir).exists() && dir_is_empty(dir) {
            return false;
        }
        match Self::read_manifest(dir) {
            Some(manifest) => manifest.search_schema_version != SEARCH_SCHEMA_VERSION,
            // A populated directory with no readable manifest is treated as
            // incompatible: rebuild rather than trust unknown cache contents.
            None => !dir_is_empty(dir),
        }
    }

    fn clear_dir(dir: &Path) -> Result<(), RetrievalError> {
        // Remove and recreate the whole directory in one shot rather than
        // deleting entries piecemeal: a mid-loop failure would otherwise leave a
        // half-cleared index dir. The index is a rebuildable cache, so dropping
        // the entire tree is always safe.
        std::fs::remove_dir_all(dir).map_err(|err| RetrievalError::Engine(err.to_string()))?;
        std::fs::create_dir_all(dir).map_err(|err| RetrievalError::Engine(err.to_string()))?;
        Ok(())
    }
}

fn dir_is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true)
}

/// Strip Tantivy query-syntax metacharacters and collapse whitespace so raw
/// agent prompts and code-heavy text are treated as plain natural-language
/// terms. Without this, characters like `:` `/` `"` `(` would be parsed as field
/// selectors, phrases, or grouping and could fail the query or skew results.
pub fn sanitize_query(text: &str) -> String {
    const SPECIAL: &str = "+-&|!(){}[]^\"~*?:\\/";
    text.chars()
        .map(|character| {
            if SPECIAL.contains(character) {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, body: &str) -> SearchDocument {
        SearchDocument {
            id: id.to_owned(),
            subject: None,
            tags: Vec::new(),
            body: body.to_owned(),
        }
    }

    #[test]
    fn sanitize_strips_query_operators_and_collapses_whitespace() {
        assert_eq!(
            sanitize_query("how does hm:context  handle (cloud)/sync?"),
            "how does hm context handle cloud sync"
        );
        assert_eq!(sanitize_query("  +  *  "), "");
    }

    #[test]
    fn ranks_more_relevant_document_first() {
        let index = SearchIndex::in_memory().expect("create index");
        index
            .rebuild(&[
                doc("a", "coffee is a common morning drink enjoyed worldwide"),
                doc("b", "coffee espresso is a strong concentrated brew"),
            ])
            .expect("rebuild");

        let hits = index.query("espresso coffee", 5).expect("query");
        assert_eq!(hits.first().map(|hit| hit.id.as_str()), Some("b"));
    }

    #[test]
    fn omits_documents_with_no_term_overlap() {
        let index = SearchIndex::in_memory().expect("create index");
        index
            .rebuild(&[
                doc("a", "the user prefers dark roast coffee"),
                doc("b", "the user is learning to play the cello"),
            ])
            .expect("rebuild");

        let hits = index.query("coffee roast", 5).expect("query");
        let ids: Vec<&str> = hits.iter().map(|hit| hit.id.as_str()).collect();
        assert_eq!(ids, vec!["a"]);
    }

    #[test]
    fn subject_and_tag_matches_are_recalled() {
        let index = SearchIndex::in_memory().expect("create index");
        index
            .rebuild(&[SearchDocument {
                id: "a".to_owned(),
                subject: Some("travel preference".to_owned()),
                tags: vec!["airline".to_owned()],
                body: "body text without the query terms".to_owned(),
            }])
            .expect("rebuild");

        assert_eq!(
            index
                .query("travel", 5)
                .expect("subject query")
                .first()
                .map(|hit| hit.id.as_str()),
            Some("a")
        );
        assert_eq!(
            index
                .query("airline", 5)
                .expect("tag query")
                .first()
                .map(|hit| hit.id.as_str()),
            Some("a")
        );
    }

    #[test]
    fn empty_query_returns_no_hits() {
        let index = SearchIndex::in_memory().expect("create index");
        index.rebuild(&[doc("a", "anything")]).expect("rebuild");
        assert!(index.query("  *? ", 5).expect("query").is_empty());
        assert!(index.query("anything", 0).expect("query").is_empty());
    }

    #[test]
    fn rebuild_replaces_previous_contents() {
        let index = SearchIndex::in_memory().expect("create index");
        index.rebuild(&[doc("old", "coffee notes")]).expect("first");
        index
            .rebuild(&[doc("new", "coffee notes")])
            .expect("second");

        let hits = index.query("coffee", 5).expect("query");
        let ids: Vec<&str> = hits.iter().map(|hit| hit.id.as_str()).collect();
        assert_eq!(ids, vec!["new"], "old contents must not survive a rebuild");
    }

    #[test]
    fn is_fresh_tracks_the_corpus_fingerprint() {
        let dir = std::env::temp_dir().join(format!(
            "hm-retrieval-fresh-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let index = SearchIndex::open_or_create_in_dir(&dir).expect("create");
        index
            .rebuild_tagged(&[doc("a", "x")], Some("fp1"))
            .expect("rebuild");
        assert!(index.is_fresh("fp1"));
        assert!(!index.is_fresh("fp2"), "a changed fingerprint is not fresh");

        // The fingerprint persists, so a reopen can skip rebuilding.
        let reopened = SearchIndex::open_or_create_in_dir(&dir).expect("reopen");
        assert!(reopened.is_fresh("fp1"));

        // An in-memory or untagged index is never considered fresh.
        assert!(!SearchIndex::in_memory().expect("ram").is_fresh("fp1"));

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn dir_index_persists_and_writes_manifest() {
        let dir = std::env::temp_dir().join(format!(
            "hm-retrieval-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        {
            let index = SearchIndex::open_or_create_in_dir(&dir).expect("create dir index");
            index
                .rebuild(&[doc("a", "persisted coffee record")])
                .expect("rebuild");
            let manifest = index.manifest().expect("manifest written");
            assert_eq!(manifest.search_schema_version, SEARCH_SCHEMA_VERSION);
            assert_eq!(manifest.document_count, 1);
        }

        // Reopening the same directory must see the committed document without a
        // rebuild, proving the cache is durable.
        let reopened = SearchIndex::open_or_create_in_dir(&dir).expect("reopen dir index");
        let hits = reopened.query("coffee", 5).expect("query");
        assert_eq!(hits.first().map(|hit| hit.id.as_str()), Some("a"));

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
