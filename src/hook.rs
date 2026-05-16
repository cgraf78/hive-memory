//! Agent lifecycle hook helpers.
//!
//! Hook adapters should stay thin: they know the host event shape, then call
//! `hm hook <event>` and apply returned actions. This module owns the local
//! session state and deterministic prompt heuristic so dotfiles scripts do not
//! reimplement memory policy in shell.

use crate::{id, write};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// Session-local lifecycle state.
///
/// This file is coordination state, not canonical memory. It lets prompt,
/// tool, and stop hooks share durable-memory debt across separate hook
/// invocations without writing memory automatically.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HookState {
    /// Whether a prompt has indicated durable memory intent not yet satisfied.
    pub memory_pending: bool,
    /// Short human-readable reason for the pending reminder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_reason: Option<String>,
    /// RFC3339 timestamp of the last state update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// Hook state load/save failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookStateError {
    /// Filesystem operation failed.
    Io {
        /// Operation that failed.
        action: &'static str,
        /// Path involved in the failure.
        path: PathBuf,
        /// Original error rendered for CLI diagnostics.
        message: String,
    },
    /// State JSON was corrupt or could not be serialized.
    Json(String),
}

impl Display for HookStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                message,
            } => write!(
                f,
                "failed to {action} hook state {}: {message}",
                path.display()
            ),
            Self::Json(message) => write!(f, "invalid hook state JSON: {message}"),
        }
    }
}

impl Error for HookStateError {}

/// Return the state file for one agent session.
///
/// Session IDs come from host integrations and may contain characters that are
/// awkward as path components, so the path uses the same filename sanitizer as
/// memory IDs. Keeping each session in its own directory leaves room for write
/// receipts and refresh cursors beside the hook state file.
pub fn state_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join("runs")
        .join(id::sanitize_component(session_id))
        .join("hook-state.json")
}

/// Load hook state, returning an empty state when the session has no file yet.
pub fn load_state(state_dir: &Path, session_id: &str) -> Result<HookState, HookStateError> {
    let path = state_path(state_dir, session_id);
    match fs::read_to_string(&path) {
        Ok(contents) => {
            serde_json::from_str(&contents).map_err(|err| HookStateError::Json(err.to_string()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HookState::default()),
        Err(err) => Err(io_error("read", &path, err)),
    }
}

/// Save hook state with the shared atomic writer.
///
/// Hook processes can run close together at prompt/tool boundaries. Atomic
/// replace keeps readers from seeing partial JSON and reuses the same durability
/// policy as generated files and indexes.
pub fn save_state(
    state_dir: &Path,
    session_id: &str,
    state: &HookState,
    options: &write::AtomicWriteOptions,
) -> Result<(), HookStateError> {
    let path = state_path(state_dir, session_id);
    let json =
        serde_json::to_vec_pretty(state).map_err(|err| HookStateError::Json(err.to_string()))?;
    write::write_atomic(&path, &json, options).map_err(|err| HookStateError::Io {
        action: "write",
        path,
        message: err.to_string(),
    })?;
    Ok(())
}

/// Mark durable-memory intent as pending for a session.
pub fn mark_memory_pending(
    state_dir: &Path,
    session_id: &str,
    reason: impl Into<String>,
    options: &write::AtomicWriteOptions,
) -> Result<HookState, HookStateError> {
    let mut state = load_state(state_dir, session_id)?;
    state.memory_pending = true;
    state.pending_reason = Some(reason.into());
    state.updated_at = Some(rfc3339(OffsetDateTime::now_utc()));
    save_state(state_dir, session_id, &state, options)?;
    Ok(state)
}

/// Return whether a prompt obviously asks the agent to remember something.
///
/// V1 deliberately uses a small deterministic heuristic instead of a model call:
/// hooks must be cheap, predictable, and safe to run in every agent session.
/// The heuristic only catches explicit memory intent; agent policy still handles
/// judgment-based writes for subtler cases.
pub fn prompt_has_memory_intent(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        "remember this",
        "remember that",
        "please remember",
        "don't forget",
        "do not forget",
        "keep in mind",
        "for future reference",
        "from now on",
        "add to memory",
        "save this",
        "make a note",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn rfc3339(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting should not fail")
}

fn io_error(action: &'static str, path: &Path, err: std::io::Error) -> HookStateError {
    HookStateError::Io {
        action,
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::FsyncPolicy;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hive-memory-hook-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn options() -> write::AtomicWriteOptions {
        write::AtomicWriteOptions {
            fsync: FsyncPolicy::Never,
            ..write::AtomicWriteOptions::default()
        }
    }

    #[test]
    fn state_path_sanitizes_session_id() {
        let path = state_path(Path::new("/tmp/hm"), "codex/session:1");

        assert_eq!(
            path,
            PathBuf::from("/tmp/hm/runs/codex-session-1/hook-state.json")
        );
    }

    #[test]
    fn memory_pending_round_trips() {
        let dir = temp_dir("pending");

        let saved = mark_memory_pending(&dir, "session-1", "explicit memory intent", &options())
            .expect("mark pending");
        let loaded = load_state(&dir, "session-1").expect("load state");

        assert!(saved.memory_pending);
        assert_eq!(
            loaded.pending_reason.as_deref(),
            Some("explicit memory intent")
        );
        assert!(loaded.updated_at.is_some());
    }

    #[test]
    fn state_load_tolerates_missing_future_fields() {
        let dir = temp_dir("state-defaults");
        let path = state_path(&dir, "session-1");
        fs::create_dir_all(path.parent().expect("state parent")).expect("state parent");
        fs::write(&path, r#"{"memory_pending":true}"#).expect("write old state");

        let loaded = load_state(&dir, "session-1").expect("load state");

        assert!(loaded.memory_pending);
        assert_eq!(loaded.pending_reason, None);
    }

    #[test]
    fn prompt_intent_is_deliberately_explicit() {
        assert!(prompt_has_memory_intent(
            "Please remember this preference for later."
        ));
        assert!(prompt_has_memory_intent(
            "For future reference, this repo uses cargo-dist."
        ));
        assert!(!prompt_has_memory_intent(
            "Can you inspect the failing test output?"
        ));
    }
}
