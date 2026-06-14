//! LLM-assisted capture: extract durable candidate facts from conversation text.
//!
//! This is the "automatic" leg of memory: instead of the user hand-writing every
//! `hm remember`, an explicit `hm capture` run asks a model to distill a
//! conversation into atomic, durable facts. Per the SPEC trust boundary, capture
//! is **staging only** — callers write the results to the low-trust inbox (raw
//! notes, excluded from context by default), never to canonical memory. Promotion
//! of staged candidates into durable memory (with mem0-style ADD/UPDATE/DELETE
//! reconciliation against existing memories) is a separate, gated step left to a
//! follow-up; nothing here can pollute curated or remembered memory.
//!
//! Like `hm classify`, capture is an explicit, opt-in command that invokes a
//! model. It is never called from the latency-sensitive hook/search/context hot
//! paths.

use std::time::Duration;

use crate::llm::{self, Backend, LlmError};
use crate::secret;

/// Reject candidates longer than this many characters: a long string is almost
/// always a summary paragraph rather than the atomic fact capture is meant to
/// stage, and oversized notes hurt retrieval quality downstream.
pub const MAX_FACT_CHARS: usize = 280;

/// Build the extraction prompt.
///
/// The instructions bias hard toward *durable* facts and explicitly exclude the
/// categories that make auto-captured memory noisy or unsafe: speculation,
/// transient task/PR/status state, time-bound details, and secrets. The model is
/// told to emit a strict JSON array of short strings so the output is machine
/// parseable while remaining human-readable.
pub fn extraction_prompt(conversation: &str) -> String {
    format!(
        "You extract durable, reusable memory from a conversation.\n\
         Output ONLY a JSON array of short strings. Each string is ONE atomic, \
         self-contained fact in plain English about the user or their projects: \
         stable preferences, decisions, conventions, identities, or project \
         facts.\n\
         EXCLUDE anything that is: speculative or uncertain; transient task, PR, \
         or status state; time-bound and likely stale tomorrow; or a secret or \
         credential.\n\
         If there is nothing durable to capture, output exactly [].\n\n\
         Conversation:\n{conversation}\n\n\
         JSON array:"
    )
}

/// Parse a JSON array of fact strings from (possibly chatty) model stdout.
///
/// Agent CLIs print prose around their answer, so this scans for the first `[`
/// and reads exactly one JSON value from there, ignoring any trailing output.
/// Non-string array elements are dropped rather than failing the whole parse.
pub fn parse_candidates(stdout: &str) -> Vec<String> {
    let Some(start) = stdout.find('[') else {
        return Vec::new();
    };
    let mut stream =
        serde_json::Deserializer::from_str(&stdout[start..]).into_iter::<serde_json::Value>();
    let Some(Ok(serde_json::Value::Array(items))) = stream.next() else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| match item {
            serde_json::Value::String(text) => Some(text.trim().to_owned()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect()
}

/// Filter candidates to those safe and sensible to stage.
///
/// Drops empties, over-long blobs, exact duplicates (order-preserving), and —
/// critically — any candidate that trips secret detection, so an extracted
/// credential is never written to disk or echoed. Secret-bearing candidates are
/// dropped silently to honor the secret-write refusal policy.
pub fn filter_candidates(candidates: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    candidates
        .into_iter()
        .map(|candidate| candidate.trim().to_owned())
        .filter(|candidate| {
            !candidate.is_empty()
                && candidate.chars().count() <= MAX_FACT_CHARS
                && secret::detect(candidate).is_empty()
        })
        .filter(|candidate| seen.insert(candidate.clone()))
        .collect()
}

/// Extract durable candidate facts from `conversation` using `backend`.
///
/// Runs the model, parses the JSON array, and applies [`filter_candidates`]. The
/// result is the set of candidate facts to stage; it is never written to
/// canonical memory by this function.
pub fn extract(
    backend: &Backend,
    conversation: &str,
    timeout: Duration,
) -> Result<Vec<String>, LlmError> {
    let raw = llm::invoke_raw(backend, &extraction_prompt(conversation), timeout)?;
    Ok(filter_candidates(parse_candidates(&raw)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_prompt_carries_conversation_and_exclusions() {
        let prompt = extraction_prompt("user: I prefer fd over find");
        assert!(prompt.contains("I prefer fd over find"));
        assert!(prompt.contains("JSON array"));
        assert!(prompt.contains("secret"));
    }

    #[test]
    fn parse_candidates_reads_array_amid_prose() {
        let stdout = "Here are the facts:\n[\"prefers dark roast\", \"uses rust\"]\nDone.";
        assert_eq!(
            parse_candidates(stdout),
            vec!["prefers dark roast".to_owned(), "uses rust".to_owned()]
        );
    }

    #[test]
    fn parse_candidates_skips_non_strings_and_handles_empty() {
        assert_eq!(
            parse_candidates("[\"keep\", 3, null, \"  \", \"also\"]"),
            vec!["keep".to_owned(), "also".to_owned()]
        );
        assert!(parse_candidates("[]").is_empty());
        assert!(parse_candidates("no array here").is_empty());
    }

    #[test]
    fn filter_drops_empty_overlong_and_duplicate() {
        let long = "x".repeat(MAX_FACT_CHARS + 1);
        let kept = filter_candidates(vec![
            "  fact one  ".to_owned(),
            String::new(),
            long,
            "fact one".to_owned(), // duplicate of the trimmed first
            "fact two".to_owned(),
        ]);
        assert_eq!(kept, vec!["fact one".to_owned(), "fact two".to_owned()]);
    }

    #[test]
    fn filter_drops_candidates_bearing_secrets() {
        // An AWS access key id must never be staged. AKIA + 16 uppercase alnum.
        let kept = filter_candidates(vec![
            "user prefers tabs".to_owned(),
            "deploy key is AKIAIOSFODNN7EXAMPLE".to_owned(),
        ]);
        assert_eq!(kept, vec!["user prefers tabs".to_owned()]);
    }

    #[test]
    fn extract_runs_the_fake_backend_end_to_end() {
        // The fake-llm fixture echoes env-driven output; point it at a JSON array
        // so the full extract path (invoke -> parse -> filter) is exercised.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/fake-llm-capture");
        std::fs::write(
            &fixture,
            "#!/usr/bin/env bash\ncat >/dev/null\nprintf '%s' '[\"prefers fd over find\", \"\"]'\n",
        )
        .expect("write fixture");
        let mut perms = std::fs::metadata(&fixture).expect("meta").permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(&fixture, perms).expect("chmod");

        let backend = Backend::command(vec![fixture.to_string_lossy().into_owned()]);
        let facts = extract(&backend, "user: I like fd", Duration::from_secs(30))
            .expect("extract succeeds");
        assert_eq!(facts, vec!["prefers fd over find".to_owned()]);

        let _ = std::fs::remove_file(&fixture);
    }
}
