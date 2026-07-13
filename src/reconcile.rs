//! mem0-style reconciliation of a candidate fact against existing memory.
//!
//! Capture (`hm capture`) stages extracted facts as raw inbox notes. Promotion
//! is the gated step that turns a candidate into durable memory while keeping the
//! store tidy: rather than blindly appending, the model compares the candidate
//! against the most similar existing memories and chooses one operation —
//! ADD a new fact, UPDATE one that the candidate refines, mark one obsolete
//! (DELETE), or NOOP when the candidate is already known. This is the operation
//! set mem0 popularized; here every mutating op is applied through the existing
//! supersession machinery (write the candidate, optionally superseding the
//! target) so nothing is ever hard-deleted.
//!
//! This module owns only the *decision* (prompt + parse). Applying the decision
//! and retrieving the similar memories belongs to the command layer, which has
//! the store, write path, and policy context.

use std::time::Duration;

use crate::llm::{self, Backend, LlmError};

/// One existing memory offered to the model as a reconciliation candidate.
#[derive(Debug, Clone)]
pub struct ExistingMemory {
    /// Record id, echoed back by the model to target UPDATE/DELETE.
    pub id: String,
    /// Record body shown to the model.
    pub text: String,
}

/// The reconciliation decision for a candidate fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// No equivalent memory exists; store the candidate as a new fact.
    Add,
    /// The candidate refines `target`; store it superseding that record.
    Update {
        /// Id of the existing record the candidate replaces.
        target: String,
    },
    /// The candidate makes `target` obsolete; store it superseding that record
    /// (the superseded record is retained for audit, never hard-deleted).
    Delete {
        /// Id of the now-obsolete record.
        target: String,
    },
    /// The candidate is already represented; do nothing.
    Noop,
}

/// Build the decision prompt: the candidate plus numbered existing memories, with
/// instructions to return a single JSON object naming the operation and (for
/// UPDATE/DELETE) the target id.
pub fn decision_prompt(candidate: &str, existing: &[ExistingMemory]) -> String {
    let mut listing = String::new();
    if existing.is_empty() {
        listing.push_str("(none)\n");
    } else {
        for memory in existing {
            listing.push_str(&format!("- id={}: {}\n", memory.id, memory.text));
        }
    }
    format!(
        "You maintain a durable memory store. Decide how to reconcile a new \
         candidate fact against the existing memories.\n\
         Reply with ONLY a JSON object: {{\"op\": \"ADD|UPDATE|DELETE|NOOP\", \
         \"id\": \"<existing id for UPDATE/DELETE, else omit>\"}}.\n\
         - ADD: no existing memory means the same thing.\n\
         - UPDATE: an existing memory should be refined/replaced by the candidate.\n\
         - DELETE: the candidate makes an existing memory obsolete or wrong.\n\
         - NOOP: an existing memory already conveys the candidate.\n\n\
         Candidate fact: {candidate}\n\n\
         Existing memories:\n{listing}\n\
         JSON decision:"
    )
}

/// Parse the model's JSON decision. `valid_ids` gates UPDATE/DELETE targets: an
/// operation naming an unknown id is downgraded to ADD (for UPDATE) or NOOP (for
/// DELETE) rather than acting on a non-existent record. Output that does not
/// parse defaults to ADD — the conservative choice that never mutates an
/// existing record.
pub fn parse_operation(stdout: &str, valid_ids: &[String]) -> Operation {
    let Some(start) = stdout.find('{') else {
        return Operation::Add;
    };
    let mut stream =
        serde_json::Deserializer::from_str(&stdout[start..]).into_iter::<serde_json::Value>();
    let Some(Ok(value)) = stream.next() else {
        return Operation::Add;
    };
    let op = value
        .get("op")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase();
    let target = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let target_valid = target
        .as_deref()
        .is_some_and(|id| valid_ids.iter().any(|valid| valid == id));

    match op.as_str() {
        "UPDATE" if target_valid => Operation::Update {
            target: target.expect("validated present"),
        },
        // An UPDATE with no/unknown target still wants the fact recorded.
        "UPDATE" => Operation::Add,
        "DELETE" if target_valid => Operation::Delete {
            target: target.expect("validated present"),
        },
        // A DELETE with no/unknown target has nothing to act on.
        "DELETE" => Operation::Noop,
        "NOOP" => Operation::Noop,
        _ => Operation::Add,
    }
}

/// Ask the backend to reconcile `candidate` against `existing` memories.
pub fn reconcile(
    backend: &Backend,
    candidate: &str,
    existing: &[ExistingMemory],
    timeout: Duration,
) -> Result<Operation, LlmError> {
    let raw = llm::invoke_raw(backend, &decision_prompt(candidate, existing), timeout)?;
    let valid_ids: Vec<String> = existing.iter().map(|memory| memory.id.clone()).collect();
    Ok(parse_operation(&raw, &valid_ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn existing() -> Vec<ExistingMemory> {
        vec![
            ExistingMemory {
                id: "a".to_owned(),
                text: "user prefers fd over find".to_owned(),
            },
            ExistingMemory {
                id: "b".to_owned(),
                text: "user uses neovim".to_owned(),
            },
        ]
    }

    fn ids() -> Vec<String> {
        vec!["a".to_owned(), "b".to_owned()]
    }

    #[test]
    fn decision_prompt_lists_candidate_and_existing() {
        let prompt = decision_prompt("new fact", &existing());
        assert!(prompt.contains("new fact"));
        assert!(prompt.contains("id=a: user prefers fd over find"));
        assert!(prompt.contains("NOOP"));
    }

    #[test]
    fn decision_prompt_handles_no_existing_memories() {
        let prompt = decision_prompt("first fact", &[]);
        assert!(prompt.contains("(none)"));
    }

    #[test]
    fn parse_update_and_delete_require_valid_target() {
        assert_eq!(
            parse_operation(r#"{"op":"UPDATE","id":"a"}"#, &ids()),
            Operation::Update {
                target: "a".to_owned()
            }
        );
        assert_eq!(
            parse_operation(r#"{"op":"DELETE","id":"b"}"#, &ids()),
            Operation::Delete {
                target: "b".to_owned()
            }
        );
        // Unknown target: UPDATE degrades to ADD, DELETE to NOOP.
        assert_eq!(
            parse_operation(r#"{"op":"UPDATE","id":"zzz"}"#, &ids()),
            Operation::Add
        );
        assert_eq!(
            parse_operation(r#"{"op":"DELETE","id":"zzz"}"#, &ids()),
            Operation::Noop
        );
    }

    #[test]
    fn parse_noop_and_add_and_garbage() {
        assert_eq!(parse_operation(r#"{"op":"NOOP"}"#, &ids()), Operation::Noop);
        assert_eq!(parse_operation(r#"{"op":"ADD"}"#, &ids()), Operation::Add);
        // Chatty prose around the JSON is tolerated.
        assert_eq!(
            parse_operation("Here is my decision:\n{\"op\":\"ADD\"}\nThanks", &ids()),
            Operation::Add
        );
        // Output that does not parse defaults to the non-mutating ADD.
        assert_eq!(parse_operation("no json", &ids()), Operation::Add);
    }

    #[test]
    fn reconcile_runs_the_fake_backend_end_to_end() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/fake-llm-reconcile");
        std::fs::write(
            &fixture,
            "#!/usr/bin/env bash\ncat >/dev/null\nprintf '%s' '{\"op\":\"UPDATE\",\"id\":\"a\"}'\n",
        )
        .expect("write fixture");
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&fixture).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fixture, perms).expect("chmod");

        let backend = Backend::command(vec![
            "bash".to_owned(),
            fixture.to_string_lossy().into_owned(),
        ]);
        let op = reconcile(
            &backend,
            "user now strongly prefers fd",
            &existing(),
            Duration::from_secs(30),
        )
        .expect("reconcile");
        assert_eq!(
            op,
            Operation::Update {
                target: "a".to_owned()
            }
        );

        let _ = std::fs::remove_file(&fixture);
    }
}
