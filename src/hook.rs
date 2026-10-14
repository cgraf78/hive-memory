//! Agent lifecycle hook helpers.
//!
//! Hook adapters should stay thin: they know the host event shape, then call
//! `hm hook <event>` and apply returned actions. This module owns the local
//! session state and deterministic prompt heuristic so dotfiles scripts do not
//! reimplement memory policy in shell.

use crate::{id, write};
use fs4::{FileExt, TryLockError};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::{self, Display};
use std::fs::{self, File, OpenOptions};
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
    /// Number of write receipts consumed by refresh/tool-complete.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub refreshed_receipts: usize,
    /// Last context selection emitted to the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_key: Option<String>,
    /// Memory ids emitted in startup/project-selection context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub startup_memory_ids: Vec<String>,
    /// Last prompt-specific recall selection emitted to the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_recall_key: Option<String>,
    /// Memory ids emitted by prompt-specific recall.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_recall_memory_ids: Vec<String>,
    /// RFC3339 timestamp of the last prompt recall update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_recall_updated_at: Option<String>,
    /// Memories emitted within the host's limit in the current context epoch.
    ///
    /// Only delivery-tracking hooks (`--delivery`) read or write this set. It
    /// holds memories whose context persists in the transcript, so a later
    /// hook may skip them until a context reset starts a new epoch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delivered: Vec<DeliveredMemory>,
    /// Context selection whose memories are all visible in this epoch.
    ///
    /// Tracked counterpart of `context_key`. It is separate so an untracked
    /// hook sharing the session id (a nested agent, an older adapter, tracking
    /// switched off) can never make a tracked hook skip a re-send.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_context_key: Option<String>,
    /// Tracked counterpart of `prompt_recall_key`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_recall_key: Option<String>,
    /// Memory ids turn-lifetime prompt recall emitted in this epoch.
    ///
    /// Tracked counterpart of `prompt_recall_memory_ids` for hosts that drop
    /// prompt context after the turn. These memories are not visible any
    /// more, so they never join `delivered`; recall only uses them to move on
    /// to the next hits, as untracked recall does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_recall_ids: Vec<String>,
    /// Counter advanced by every context reset.
    ///
    /// Tracked bookkeeping carries the epoch it was selected in, and an update
    /// from an earlier epoch is dropped: it was filtered against a delivered
    /// set the host no longer shows.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub context_epoch: u64,
    /// RFC3339 timestamp of the last context reset, for diagnostics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_epoch_started_at: Option<String>,
}

impl HookState {
    /// Return whether this exact memory text is already visible in the epoch.
    pub fn has_delivered(&self, id: &str, digest: &str) -> bool {
        self.delivered
            .iter()
            .any(|memory| memory.id == id && memory.digest == digest)
    }
}

/// How long context injected by one hook stays visible to the model.
///
/// This is a host fact supplied by the hook adapter. Claude Code and Codex
/// keep hook context in the transcript until compaction or a clear, while Muse
/// replaces prompt-submit context on every turn and keeps only session-start
/// context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryLifetime {
    /// Context stays in the transcript until the host loses it (compaction,
    /// clear), which the adapter reports as a context reset.
    Persistent,
    /// Context is visible for the current turn only; its memories never join
    /// the delivered set.
    Turn,
}

impl DeliveryLifetime {
    /// Return the stable lowercase label used on the CLI and in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Persistent => "persistent",
            Self::Turn => "turn",
        }
    }
}

/// One memory text delivered to the model, keyed by id and body digest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeliveredMemory {
    /// Memory id.
    pub id: String,
    /// Digest of the rendered body, so changed text under one id is new.
    pub digest: String,
}

/// Session bookkeeping a hook applies after writing its response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryUpdate {
    /// Context epoch the emitted context was selected in.
    #[serde(default)]
    pub epoch: u64,
    /// Memories that become part of the epoch's delivered set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<DeliveredMemory>,
    /// Context selection now fully visible (sets `delivered_context_key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_key: Option<String>,
    /// Prompt recall selection now visible (sets `delivered_recall_key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_recall_key: Option<String>,
    /// Ids turn-lifetime recall emitted (added to `turn_recall_ids`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_recall_ids: Vec<String>,
    /// Untracked `context_key` for an emitted selection, complete or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untracked_context_key: Option<String>,
    /// Ids that selection emitted (for `startup_memory_ids`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untracked_context_ids: Vec<String>,
    /// Untracked `prompt_recall_key` for emitted prompt recall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untracked_recall_key: Option<String>,
    /// Ids that recall emitted (for `prompt_recall_memory_ids`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untracked_recall_ids: Vec<String>,
}

/// Ephemeral receipt for a successful session memory write.
///
/// Receipts are not canonical memory. They are the cheap coordination signal
/// that lets `hm hook tool-complete` know whether a successful tool event
/// actually wrote memory and therefore should clear the prompt reminder debt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteReceipt {
    /// RFC3339 timestamp when the receipt was appended.
    pub created_at: String,
    /// Resolved store alias written by the command.
    pub store: String,
    /// Memory scope used for the write.
    pub scope: String,
    /// Project identity attached to the write, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Memory record id shared by the note and optional event.
    pub note_id: String,
    /// Whether this command created a new canonical note.
    pub created: bool,
}

/// Held local lock for one hook refresh run.
///
/// This is deliberately a local-process/file lock, not a distributed cloud-sync
/// lock. It only prevents overlapping refresh work on the same host; another
/// machine may still refresh its own synced copy independently.
#[derive(Debug)]
pub struct RefreshLock {
    file: File,
    /// Lock file path, exposed for diagnostics and tests.
    pub path: PathBuf,
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Held local lock serializing one session's hook-state read-modify-write.
///
/// `save_state` replaces the whole state file atomically, but the surrounding
/// load -> mutate -> save sequence in each `mark_*` is not atomic. Distinct hook
/// events for the same session (e.g. `prompt-submit` -> `mark_prompt_recall` and
/// `tool-complete` -> `mark_receipts_refreshed`) fire close together at
/// prompt/tool boundaries, each loads the same prior state, mutates a different
/// field, and atomically rewrites the file, so the second writer's whole-file
/// replace clobbers the first writer's field. This lock serializes that critical
/// section so concurrent field updates compose instead of racing.
///
/// Like `RefreshLock` this is a local-process/file lock only; cross-machine
/// synced copies still coordinate through their own host-local locks.
#[derive(Debug)]
pub struct StateLock {
    file: File,
    /// Lock file path, exposed for diagnostics and tests.
    pub path: PathBuf,
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Held local lock serializing one session's receipt stream rewrite.
///
/// Receipt publication blocks briefly rather than falling through on contention:
/// a missed receipt can leave durable-memory intent pending indefinitely. The OS
/// owns the lock through this open descriptor, so process exit releases ownership
/// even though the empty lock file remains on disk.
#[derive(Debug)]
struct ReceiptLock {
    file: File,
}

impl Drop for ReceiptLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
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
    /// Another live hook held the session state lock past the foreground budget.
    LockTimeout {
        /// Lock path involved in the timeout.
        path: PathBuf,
        /// Maximum wait before preserving hook liveness.
        wait_ms: u64,
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
            Self::LockTimeout { path, wait_ms } => write!(
                f,
                "timed out after {wait_ms}ms waiting for hook state lock {}",
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

/// Return the write-receipt JSONL file for one agent session.
pub fn write_receipts_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join("runs")
        .join(id::sanitize_component(session_id))
        .join("writes.jsonl")
}

/// Return the detached-refresh attempt marker for one agent session.
///
/// Keeping the marker beside session state prevents independent sessions from
/// overwriting each other's receipt cursors, and inactive-run pruning removes
/// it with the rest of the advisory session data.
pub fn receipt_refresh_attempt_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join("runs")
        .join(id::sanitize_component(session_id))
        .join("receipt-refresh-attempt")
}

/// Remove inactive per-session hook state older than `retention`.
///
/// State writes atomically replace files inside the run directory, updating its
/// mtime. A resumed session therefore becomes current before background refresh
/// can prune it. This removes only advisory context/receipt cursors, never
/// canonical memory or queued outbox data.
pub fn prune_inactive_runs(
    state_dir: &Path,
    retention: std::time::Duration,
) -> Result<usize, HookStateError> {
    let runs = state_dir.join("runs");
    let entries = match fs::read_dir(&runs) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(io_error("read hook runs", &runs, err)),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry.map_err(|err| io_error("read hook run entry", &runs, err))?;
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir()
            || metadata
                .modified()
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_none_or(|age| age <= retention)
        {
            continue;
        }
        fs::remove_dir_all(&path)
            .map_err(|err| io_error("remove inactive hook run", &path, err))?;
        removed += 1;
    }
    Ok(removed)
}

/// Return the hook refresh lock path for one agent/session pair.
///
/// Agent id participates in the path because long-lived agent hosts may run
/// separate agents inside the same terminal/session namespace. The lock still
/// lives under shared state so every hook invocation for that agent/session
/// converges on one local coordination point.
pub fn refresh_lock_path(state_dir: &Path, agent_id: &str, session_id: &str) -> PathBuf {
    state_dir
        .join("locks")
        .join("refresh")
        .join(id::sanitize_component(agent_id))
        .join(format!("{}.lock", id::sanitize_component(session_id)))
}

/// Try to acquire the non-blocking hook refresh lock.
///
/// Returning `Ok(None)` means another refresh is already running locally and
/// callers should report a coalesced success without consuming write receipts.
/// Any other I/O error is surfaced because it means the hook cannot trust its
/// local coordination state.
pub fn try_refresh_lock(
    state_dir: &Path,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<RefreshLock>, HookStateError> {
    let path = refresh_lock_path(state_dir, agent_id, session_id);
    let Some(parent) = path.parent() else {
        return Err(HookStateError::Io {
            action: "create refresh lock parent",
            path,
            message: "lock path has no parent".to_owned(),
        });
    };
    fs::create_dir_all(parent)
        .map_err(|err| io_error("create refresh lock parent", parent, err))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|err| io_error("open refresh lock", &path, err))?;
    match FileExt::try_lock(&file) {
        Ok(()) => Ok(Some(RefreshLock { file, path })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(io_error("lock refresh", &path, err)),
    }
}

/// Return the hook-state lock path for one session.
///
/// The lock is keyed by `session_id` alone because that is exactly the scope of
/// the state file (`state_path` ignores agent id): every writer of one session's
/// `hook-state.json` must converge on the same lock to serialize correctly, even
/// if separate agents share the session namespace. It lives under a dedicated
/// `locks/hook-state` subtree so it never contends with the refresh lock
/// (`locks/refresh`), which keeps the two locks deadlock-free by construction.
pub fn state_lock_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join("locks")
        .join("hook-state")
        .join(format!("{}.lock", id::sanitize_component(session_id)))
}

/// Return the write-receipt lock path for one session.
pub fn write_receipts_lock_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join("locks")
        .join("write-receipts")
        .join(format!("{}.lock", id::sanitize_component(session_id)))
}

/// Acquire the blocking per-session receipt lock.
///
/// Receipt writes are infrequent and the critical section only validates and
/// atomically republishes one small JSONL file. Blocking here preserves every
/// writer; unlike hook-state updates, proceeding unlocked is not a safe fallback.
fn lock_write_receipts(state_dir: &Path, session_id: &str) -> Result<ReceiptLock, HookStateError> {
    let path = write_receipts_lock_path(state_dir, session_id);
    let Some(parent) = path.parent() else {
        return Err(HookStateError::Io {
            action: "create write-receipt lock parent",
            path,
            message: "lock path has no parent".to_owned(),
        });
    };
    fs::create_dir_all(parent)
        .map_err(|err| io_error("create write-receipt lock parent", parent, err))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|err| io_error("open write-receipt lock", &path, err))?;
    FileExt::lock(&file).map_err(|err| io_error("lock write receipts", &path, err))?;
    Ok(ReceiptLock { file })
}

/// Try to acquire the non-blocking per-session hook-state lock.
///
/// Returning `Ok(None)` means another hook is mid read-modify-write for this
/// session. Callers choose either a nonblocking skip for advisory work or a
/// condition wait for lossless state updates. Only genuine I/O errors (which
/// mean local coordination state cannot be trusted) are surfaced.
pub fn try_state_lock(
    state_dir: &Path,
    session_id: &str,
) -> Result<Option<StateLock>, HookStateError> {
    let path = state_lock_path(state_dir, session_id);
    let Some(parent) = path.parent() else {
        return Err(HookStateError::Io {
            action: "create state lock parent",
            path,
            message: "lock path has no parent".to_owned(),
        });
    };
    fs::create_dir_all(parent).map_err(|err| io_error("create state lock parent", parent, err))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|err| io_error("open state lock", &path, err))?;
    match FileExt::try_lock(&file) {
        Ok(()) => Ok(Some(StateLock { file, path })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(io_error("lock state", &path, err)),
    }
}

/// Run a load -> mutate -> save cycle under the per-session state lock.
///
/// This is the single serialized critical section every `mark_*` routes through.
/// Contenders condition-wait on the local OS lock: proceeding unlocked can lose
/// unrelated fields from another hook's whole-file atomic replacement. A
/// 500-millisecond deadline keeps foreground hooks inside their outer adapter
/// budget; timing out is an explicit error so callers never mistake a dropped
/// mutation for success. The critical section contains only local state I/O,
/// and process death releases the advisory lock.
fn with_state_lock<F>(
    state_dir: &Path,
    session_id: &str,
    mutate: F,
    options: &write::AtomicWriteOptions,
) -> Result<HookState, HookStateError>
where
    F: FnOnce(&mut HookState),
{
    const STATE_LOCK_WAIT_MS: u64 = 500;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(STATE_LOCK_WAIT_MS);
    let _lock = loop {
        if let Some(lock) = try_state_lock(state_dir, session_id)? {
            break lock;
        }
        if std::time::Instant::now() >= deadline {
            return Err(HookStateError::LockTimeout {
                path: state_lock_path(state_dir, session_id),
                wait_ms: STATE_LOCK_WAIT_MS,
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    let mut state = load_state(state_dir, session_id)?;
    mutate(&mut state);
    save_state(state_dir, session_id, &state, options)?;
    Ok(state)
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
    let reason = reason.into();
    with_state_lock(
        state_dir,
        session_id,
        |state| {
            state.memory_pending = true;
            state.pending_reason = Some(reason);
            state.updated_at = Some(rfc3339(OffsetDateTime::now_utc()));
        },
        options,
    )
}

/// Clear durable-memory intent after a successful write receipt is observed.
///
/// This is separate from the refresh cursor: latency-sensitive tool hooks can
/// acknowledge the write immediately while detached index maintenance advances
/// `refreshed_receipts` only after it succeeds.
pub fn mark_memory_satisfied(
    state_dir: &Path,
    session_id: &str,
    options: &write::AtomicWriteOptions,
) -> Result<HookState, HookStateError> {
    with_state_lock(
        state_dir,
        session_id,
        |state| {
            state.memory_pending = false;
            state.pending_reason = None;
            state.updated_at = Some(rfc3339(OffsetDateTime::now_utc()));
        },
        options,
    )
}

/// Record that refresh/tool-complete consumed receipts through `receipt_count`.
pub fn mark_receipts_refreshed(
    state_dir: &Path,
    session_id: &str,
    receipt_count: usize,
    clear_memory_pending: bool,
    options: &write::AtomicWriteOptions,
) -> Result<HookState, HookStateError> {
    with_state_lock(
        state_dir,
        session_id,
        |state| {
            // Detached refresh children can finish out of launch order. Never
            // let an older child move the durable cursor backward.
            state.refreshed_receipts = state.refreshed_receipts.max(receipt_count);
            if clear_memory_pending {
                state.memory_pending = false;
                state.pending_reason = None;
            }
            state.updated_at = Some(rfc3339(OffsetDateTime::now_utc()));
        },
        options,
    )
}

/// Record the context selection emitted to a session.
pub fn mark_context_key(
    state_dir: &Path,
    session_id: &str,
    context_key: impl Into<String>,
    options: &write::AtomicWriteOptions,
) -> Result<HookState, HookStateError> {
    let context_key = context_key.into();
    with_state_lock(
        state_dir,
        session_id,
        |state| {
            state.context_key = Some(context_key);
            state.updated_at = Some(rfc3339(OffsetDateTime::now_utc()));
        },
        options,
    )
}

/// Record startup/project-selection context emitted to a session.
pub fn mark_startup_context(
    state_dir: &Path,
    session_id: &str,
    context_key: impl Into<String>,
    memory_ids: Vec<String>,
    options: &write::AtomicWriteOptions,
) -> Result<HookState, HookStateError> {
    let context_key = context_key.into();
    with_state_lock(
        state_dir,
        session_id,
        |state| {
            state.context_key = Some(context_key);
            state.startup_memory_ids = memory_ids;
            state.updated_at = Some(rfc3339(OffsetDateTime::now_utc()));
        },
        options,
    )
}

/// Record prompt-specific recall emitted to a session.
pub fn mark_prompt_recall(
    state_dir: &Path,
    session_id: &str,
    recall_key: impl Into<String>,
    memory_ids: Vec<String>,
    options: &write::AtomicWriteOptions,
) -> Result<HookState, HookStateError> {
    let recall_key = recall_key.into();
    with_state_lock(
        state_dir,
        session_id,
        |state| {
            state.prompt_recall_key = Some(recall_key);
            state.prompt_recall_memory_ids.extend(memory_ids);
            state.prompt_recall_memory_ids.sort();
            state.prompt_recall_memory_ids.dedup();
            state.prompt_recall_updated_at = Some(rfc3339(OffsetDateTime::now_utc()));
            state.updated_at = Some(rfc3339(OffsetDateTime::now_utc()));
        },
        options,
    )
}

/// Start a new context epoch after the host lost earlier context.
///
/// Compaction, a clear, or a resume can remove injected context from the
/// model's view. Everything that suppresses a later injection is cleared: the
/// delivered set, the tracked and untracked context and recall selections, and
/// the emitted-id lists turn and untracked recall skip. The epoch counter advances
/// so in-flight bookkeeping from before the reset is dropped. Durable-memory
/// debt and write receipts are unrelated to what the model can see and are
/// preserved.
pub fn reset_context_epoch(
    state_dir: &Path,
    session_id: &str,
    options: &write::AtomicWriteOptions,
) -> Result<HookState, HookStateError> {
    with_state_lock(
        state_dir,
        session_id,
        |state| {
            let now = rfc3339(OffsetDateTime::now_utc());
            state.delivered.clear();
            state.delivered_context_key = None;
            state.delivered_recall_key = None;
            state.turn_recall_ids.clear();
            state.context_epoch = state.context_epoch.wrapping_add(1);
            state.context_key = None;
            state.startup_memory_ids.clear();
            state.prompt_recall_key = None;
            state.prompt_recall_memory_ids.clear();
            state.prompt_recall_updated_at = None;
            state.context_epoch_started_at = Some(now.clone());
            state.updated_at = Some(now);
        },
        options,
    )
}

/// Record the memories and selections a tracked hook just emitted.
///
/// The caller applies this only after writing its response, so a failed write
/// never marks anything delivered.
pub fn apply_delivery(
    state_dir: &Path,
    session_id: &str,
    update: DeliveryUpdate,
    options: &write::AtomicWriteOptions,
) -> Result<HookState, HookStateError> {
    with_state_lock(
        state_dir,
        session_id,
        |state| apply_update(state, update),
        options,
    )
}

fn apply_update(state: &mut HookState, update: DeliveryUpdate) {
    if update.epoch != state.context_epoch {
        // Selected before a reset: its memories are not visible in this epoch
        // and its selection keys describe lost context.
        return;
    }
    let now = rfc3339(OffsetDateTime::now_utc());
    if !update.memories.is_empty() {
        state.delivered.extend(update.memories);
        state.delivered.sort();
        state.delivered.dedup();
    }
    if let Some(context_key) = update.context_key {
        state.delivered_context_key = Some(context_key);
    }
    if let Some(recall_key) = update.prompt_recall_key {
        state.delivered_recall_key = Some(recall_key);
    }
    if !update.turn_recall_ids.is_empty() {
        state.turn_recall_ids.extend(update.turn_recall_ids);
        state.turn_recall_ids.sort();
        state.turn_recall_ids.dedup();
    }
    // Keep the untracked fields as untracked hooks would have left them, so
    // an adapter that falls back to an untracked call (too little room, for
    // example) does not re-send a selection the session already received.
    // Tracked hooks never read these fields.
    if let Some(context_key) = update.untracked_context_key {
        if state.context_key.as_deref() != Some(context_key.as_str()) {
            // A new selection replaces the old one's ids, as untracked hooks do.
            state.startup_memory_ids.clear();
        }
        state.context_key = Some(context_key);
        state
            .startup_memory_ids
            .extend(update.untracked_context_ids);
        // Earlier batches of a drained selection, and anything else still
        // visible in the epoch, count as sent too.
        state
            .startup_memory_ids
            .extend(state.delivered.iter().map(|memory| memory.id.clone()));
        state.startup_memory_ids.sort();
        state.startup_memory_ids.dedup();
    }
    if let Some(recall_key) = update.untracked_recall_key {
        state.prompt_recall_key = Some(recall_key);
        state
            .prompt_recall_memory_ids
            .extend(update.untracked_recall_ids);
        state.prompt_recall_memory_ids.sort();
        state.prompt_recall_memory_ids.dedup();
        state.prompt_recall_updated_at = Some(now.clone());
    }
    state.updated_at = Some(now);
}

/// Return memory ids already sent to the session.
pub fn known_session_memory_ids(state: &HookState) -> std::collections::BTreeSet<String> {
    state
        .startup_memory_ids
        .iter()
        .chain(state.prompt_recall_memory_ids.iter())
        .cloned()
        .collect()
}

/// Append a successful write receipt for the active session.
///
/// Serialize writers with a dedicated OS lock, then atomically replace the full
/// JSONL stream. Plain O_APPEND cannot make serde's JSON write plus the trailing
/// newline indivisible, and process interruption can leave a partial final line.
/// Atomic replacement keeps the prior complete stream visible until the new
/// complete stream is published.
pub fn append_write_receipt(
    state_dir: &Path,
    session_id: &str,
    receipt: &WriteReceipt,
) -> Result<(), HookStateError> {
    append_write_receipt_with_options(
        state_dir,
        session_id,
        receipt,
        &write::AtomicWriteOptions::default(),
    )
}

/// Append a successful write receipt with an explicit durability policy.
pub fn append_write_receipt_with_options(
    state_dir: &Path,
    session_id: &str,
    receipt: &WriteReceipt,
    options: &write::AtomicWriteOptions,
) -> Result<(), HookStateError> {
    let path = write_receipts_path(state_dir, session_id);
    let _lock = lock_write_receipts(state_dir, session_id)?;
    let existing = read_write_receipts_path(&path)?;
    parse_write_receipts(&existing)?;

    // Preserve prior bytes so an older writer cannot erase unknown fields from
    // a newer receipt schema. A valid final record without a newline receives
    // its separator before the new record is appended.
    let mut contents = existing.into_bytes();
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        contents.push(b'\n');
    }
    serde_json::to_writer(&mut contents, receipt)
        .map_err(|err| HookStateError::Json(err.to_string()))?;
    contents.push(b'\n');
    write::write_atomic(&path, &contents, options).map_err(|err| HookStateError::Io {
        action: "write",
        path,
        message: err.to_string(),
    })?;
    Ok(())
}

/// Read all valid write receipts for a session.
///
/// Invalid JSON is treated as state corruption instead of skipped. Receipts are
/// the durable proof that a write happened during the session, so silently
/// dropping a bad line could clear or refresh the wrong amount of memory debt.
pub fn load_write_receipts(
    state_dir: &Path,
    session_id: &str,
) -> Result<Vec<WriteReceipt>, HookStateError> {
    let path = write_receipts_path(state_dir, session_id);
    let contents = read_write_receipts_path(&path)?;
    parse_write_receipts(&contents)
}

fn read_write_receipts_path(path: &Path) -> Result<String, HookStateError> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(io_error("read", path, err)),
    }
}

fn parse_write_receipts(contents: &str) -> Result<Vec<WriteReceipt>, HookStateError> {
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(|err| HookStateError::Json(err.to_string())))
        .collect()
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

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
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
    use std::collections::BTreeSet;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use crate::test_support::temp_dir;

    fn options() -> write::AtomicWriteOptions {
        write::AtomicWriteOptions {
            fsync: FsyncPolicy::Never,
            ..write::AtomicWriteOptions::default()
        }
    }

    fn receipt(note_id: impl Into<String>) -> WriteReceipt {
        WriteReceipt {
            created_at: "2026-05-16T00:00:00Z".to_owned(),
            store: "personal".to_owned(),
            scope: "project".to_owned(),
            project_id: Some("project-id".to_owned()),
            note_id: note_id.into(),
            created: true,
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
    fn write_receipts_path_sanitizes_session_id() {
        let path = write_receipts_path(Path::new("/tmp/hm"), "codex/session:1");

        assert_eq!(
            path,
            PathBuf::from("/tmp/hm/runs/codex-session-1/writes.jsonl")
        );
    }

    #[test]
    fn receipt_refresh_attempt_path_is_session_scoped_and_sanitized() {
        let path = receipt_refresh_attempt_path(Path::new("/tmp/hm"), "codex/session:1");

        assert_eq!(
            path,
            PathBuf::from("/tmp/hm/runs/codex-session-1/receipt-refresh-attempt")
        );
    }

    #[test]
    fn prune_inactive_runs_removes_expired_advisory_state() {
        let dir = temp_dir("prune-inactive-runs");
        let run = dir.join("runs/old-session");
        fs::create_dir_all(&run).expect("create run");
        fs::write(run.join("hook-state.json"), b"{}").expect("write state");

        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(
            prune_inactive_runs(&dir, std::time::Duration::ZERO).expect("prune runs"),
            1
        );
        assert!(!run.exists());
    }

    #[test]
    fn write_receipts_lock_path_is_disjoint_and_sanitized() {
        let lock = write_receipts_lock_path(Path::new("/tmp/hm"), "codex/session:1");
        let state = state_lock_path(Path::new("/tmp/hm"), "codex/session:1");
        let refresh = refresh_lock_path(Path::new("/tmp/hm"), "codex", "codex/session:1");

        assert_eq!(
            lock,
            PathBuf::from("/tmp/hm/locks/write-receipts/codex-session-1.lock")
        );
        assert_ne!(lock, state);
        assert_ne!(lock, refresh);
    }

    #[test]
    fn refresh_lock_path_sanitizes_agent_and_session() {
        let path = refresh_lock_path(Path::new("/tmp/hm"), "codex/dev", "session:1");

        assert_eq!(
            path,
            PathBuf::from("/tmp/hm/locks/refresh/codex-dev/session-1.lock")
        );
    }

    #[test]
    fn refresh_lock_reports_existing_holder_without_blocking() {
        let dir = temp_dir("refresh-lock");
        let first = try_refresh_lock(&dir, "codex", "session-1")
            .expect("first lock")
            .expect("first acquired");
        let second = try_refresh_lock(&dir, "codex", "session-1").expect("second lock");

        assert!(first.path.is_file());
        assert!(second.is_none());
    }

    #[test]
    fn state_lock_path_sanitizes_session() {
        let path = state_lock_path(Path::new("/tmp/hm"), "codex/session:1");

        assert_eq!(
            path,
            PathBuf::from("/tmp/hm/locks/hook-state/codex-session-1.lock")
        );
    }

    #[test]
    fn state_lock_lives_outside_refresh_lock_subtree() {
        // The two locks must not share a path, otherwise a held refresh lock
        // would block hook-state updates and vice versa, risking deadlock.
        let state = state_lock_path(Path::new("/tmp/hm"), "session-1");
        let refresh = refresh_lock_path(Path::new("/tmp/hm"), "codex", "session-1");

        assert_ne!(state, refresh);
        assert!(state.starts_with("/tmp/hm/locks/hook-state"));
        assert!(refresh.starts_with("/tmp/hm/locks/refresh"));
    }

    #[test]
    fn state_lock_reports_existing_holder_without_blocking() {
        // Contention must degrade gracefully: a second try_lock while the first
        // is held returns Ok(None) rather than panicking or hard-erroring.
        let dir = temp_dir("state-lock");
        let first = try_state_lock(&dir, "session-1")
            .expect("first lock")
            .expect("first acquired");
        let second = try_state_lock(&dir, "session-1").expect("second lock");

        assert!(first.path.is_file());
        assert!(second.is_none());

        // Releasing the first holder lets a later acquirer succeed again.
        drop(first);
        let third = try_state_lock(&dir, "session-1").expect("third lock");
        assert!(third.is_some());
    }

    #[test]
    fn serialized_concurrent_marks_preserve_both_field_updates() {
        // Two distinct hook events mutate different fields of the same session's
        // state at nearly the same time. Before serialization the second
        // whole-file replace could clobber the first writer's field. Exercise
        // the production mark API directly: it owns the per-session lock across
        // each load -> mutate -> save cycle, so the updates must compose.
        let dir = temp_dir("concurrent-marks");
        for iteration in 0..50 {
            let session = format!("session-{iteration}");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

            let pending_dir = dir.clone();
            let pending_session = session.clone();
            let pending_barrier = std::sync::Arc::clone(&barrier);
            let pending = std::thread::spawn(move || {
                pending_barrier.wait();
                mark_memory_pending(&pending_dir, &pending_session, "intent", &options())
                    .expect("mark pending");
            });

            let startup_dir = dir.clone();
            let startup_session = session.clone();
            let startup_barrier = std::sync::Arc::clone(&barrier);
            let startup = std::thread::spawn(move || {
                startup_barrier.wait();
                mark_startup_context(
                    &startup_dir,
                    &startup_session,
                    "agent=codex|store=personal",
                    vec!["mem-a".to_owned(), "mem-b".to_owned()],
                    &options(),
                )
                .expect("mark startup");
            });

            pending.join().expect("pending thread");
            startup.join().expect("startup thread");
            let loaded = load_state(&dir, &session).expect("load state");
            assert!(loaded.memory_pending, "iteration {iteration}: pending lost");
            assert_eq!(loaded.pending_reason.as_deref(), Some("intent"));
            assert_eq!(
                loaded.context_key.as_deref(),
                Some("agent=codex|store=personal"),
                "iteration {iteration}: startup context lost"
            );
            assert_eq!(
                loaded.startup_memory_ids,
                vec!["mem-a".to_owned(), "mem-b".to_owned()]
            );
        }
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
        assert_eq!(loaded.refreshed_receipts, 0);
        assert_eq!(loaded.context_key, None);
    }

    #[test]
    fn write_receipts_append_and_load() {
        let dir = temp_dir("receipts");
        let receipt = receipt("note-id");

        append_write_receipt(&dir, "session-1", &receipt).expect("append receipt");
        let receipts = load_write_receipts(&dir, "session-1").expect("load receipts");

        assert_eq!(receipts, vec![receipt]);
    }

    #[test]
    fn write_receipt_append_preserves_unknown_fields() {
        let dir = temp_dir("receipt-future-fields");
        let path = write_receipts_path(&dir, "session-1");
        fs::create_dir_all(path.parent().expect("receipt parent")).expect("receipt parent");
        let existing = concat!(
            r#"{"created_at":"2026-07-21T00:00:00Z","store":"personal","scope":"global","note_id":"old","created":true,"future":{"version":2}}"#,
            "\n"
        );
        fs::write(&path, existing).expect("write future receipt");

        append_write_receipt_with_options(&dir, "session-1", &receipt("new"), &options())
            .expect("append receipt");

        let contents = fs::read_to_string(&path).expect("read receipt stream");
        assert!(contents.starts_with(existing));
        assert!(contents.contains(r#""future":{"version":2}"#));
        assert_eq!(
            load_write_receipts(&dir, "session-1")
                .expect("load receipts")
                .len(),
            2
        );
    }

    #[test]
    fn write_receipt_append_separates_valid_unterminated_record() {
        let dir = temp_dir("receipt-no-newline");
        let path = write_receipts_path(&dir, "session-1");
        fs::create_dir_all(path.parent().expect("receipt parent")).expect("receipt parent");
        let existing = serde_json::to_string(&receipt("old")).expect("serialize receipt");
        fs::write(&path, &existing).expect("write receipt without newline");

        append_write_receipt_with_options(&dir, "session-1", &receipt("new"), &options())
            .expect("append receipt");

        let contents = fs::read_to_string(&path).expect("read receipt stream");
        assert!(contents.starts_with(&format!("{existing}\n")));
        assert_eq!(contents.lines().count(), 2);
    }

    #[test]
    fn corrupt_receipt_stream_fails_closed_without_rewriting() {
        let dir = temp_dir("corrupt-receipts");
        let path = write_receipts_path(&dir, "session-1");
        fs::create_dir_all(path.parent().expect("receipt parent")).expect("receipt parent");
        let corrupt = b"{not-json}\n";
        fs::write(&path, corrupt).expect("write corrupt stream");

        let error =
            append_write_receipt_with_options(&dir, "session-1", &receipt("new"), &options())
                .expect_err("corrupt stream must fail closed");

        assert!(matches!(error, HookStateError::Json(_)));
        assert_eq!(fs::read(path).expect("read corrupt stream"), corrupt);
    }

    fn receipt_child(mode: &str, dir: &Path, session: &str, start: &Path, marker: &str) -> Child {
        Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "hook::tests::receipt_process_child",
                "--nocapture",
            ])
            .env("HM_RECEIPT_CHILD_MODE", mode)
            .env("HM_RECEIPT_STATE_DIR", dir)
            .env("HM_RECEIPT_SESSION", session)
            .env("HM_RECEIPT_START", start)
            .env("HM_RECEIPT_MARKER", marker)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn receipt child")
    }

    #[test]
    fn receipt_process_child() {
        let Ok(mode) = std::env::var("HM_RECEIPT_CHILD_MODE") else {
            return;
        };
        let dir = PathBuf::from(std::env::var("HM_RECEIPT_STATE_DIR").expect("state dir"));
        let session = std::env::var("HM_RECEIPT_SESSION").expect("session");
        let start = PathBuf::from(std::env::var("HM_RECEIPT_START").expect("start marker"));
        let marker = std::env::var("HM_RECEIPT_MARKER").expect("marker");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !start.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for start marker"
            );
            std::thread::sleep(Duration::from_millis(2));
        }

        match mode.as_str() {
            "write" => {
                append_write_receipt_with_options(&dir, &session, &receipt(marker), &options())
                    .expect("append child receipt")
            }
            "hold" => {
                let _lock = lock_write_receipts(&dir, &session).expect("hold receipt lock");
                fs::write(marker, b"ready").expect("write ready marker");
                std::thread::sleep(Duration::from_secs(60));
            }
            other => panic!("unknown receipt child mode: {other}"),
        }
    }

    #[test]
    fn concurrent_process_writers_publish_complete_receipt_streams() {
        let dir = temp_dir("concurrent-receipts");
        let start = dir.join("start");
        let session = "session-1";
        let writer_count = 12;
        let mut children = (0..writer_count)
            .map(|index| receipt_child("write", &dir, session, &start, &format!("note-{index}")))
            .collect::<Vec<_>>();
        fs::write(&start, b"start").expect("publish start marker");

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut prior_count = 0;
        loop {
            let receipts = load_write_receipts(&dir, session).expect("reader sees complete JSONL");
            assert!(
                receipts.len() >= prior_count,
                "receipt count must not regress"
            );
            prior_count = receipts.len();

            let mut running = false;
            for child in &mut children {
                match child.try_wait().expect("poll receipt child") {
                    Some(status) => assert!(status.success(), "receipt child failed: {status}"),
                    None => running = true,
                }
            }
            if !running {
                break;
            }
            assert!(Instant::now() < deadline, "concurrent writers timed out");
            std::thread::sleep(Duration::from_millis(2));
        }

        let receipts = load_write_receipts(&dir, session).expect("load final receipts");
        let ids = receipts
            .iter()
            .map(|item| item.note_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(receipts.len(), writer_count);
        assert_eq!(ids.len(), writer_count);
    }

    #[test]
    fn process_exit_releases_receipt_lock() {
        let dir = temp_dir("receipt-lock-exit");
        let start = dir.join("start");
        let ready = dir.join("ready");
        fs::write(&start, b"start").expect("publish start marker");
        let mut holder = receipt_child(
            "hold",
            &dir,
            "session-1",
            &start,
            ready.to_str().expect("utf8 ready path"),
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "lock holder did not become ready"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        holder.kill().expect("kill lock holder");
        holder.wait().expect("reap lock holder");

        append_write_receipt_with_options(&dir, "session-1", &receipt("after-exit"), &options())
            .expect("OS released receipt lock");
        assert_eq!(
            load_write_receipts(&dir, "session-1")
                .expect("load receipt")
                .len(),
            1
        );
    }

    #[test]
    fn refreshed_receipt_count_clears_pending() {
        let dir = temp_dir("receipt-count");
        mark_memory_pending(&dir, "session-1", "intent", &options()).expect("mark pending");

        let state =
            mark_receipts_refreshed(&dir, "session-1", 2, true, &options()).expect("mark refresh");

        assert!(!state.memory_pending);
        assert_eq!(state.refreshed_receipts, 2);
    }

    #[test]
    fn refreshed_receipt_count_is_monotonic_across_out_of_order_workers() {
        let dir = temp_dir("receipt-count-monotonic");
        mark_receipts_refreshed(&dir, "session-1", 2, false, &options())
            .expect("newer worker completes");

        let state = mark_receipts_refreshed(&dir, "session-1", 1, false, &options())
            .expect("older worker completes later");

        assert_eq!(state.refreshed_receipts, 2);
    }

    #[test]
    fn state_updates_bound_lock_wait_without_overwriting_unrelated_fields() {
        let dir = temp_dir("state-update-contention");
        mark_context_key(&dir, "session-1", "existing-context", &options())
            .expect("seed context state");
        let held = try_state_lock(&dir, "session-1")
            .expect("state lock")
            .expect("hold state lock");
        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                tx.send(mark_prompt_recall(
                    &dir,
                    "session-1",
                    "new-recall",
                    vec!["memory-1".to_owned()],
                    &options(),
                ))
                .expect("send update result");
            });
            let error = rx
                .recv_timeout(Duration::from_secs(3))
                .expect("contended state update stayed latency-bounded")
                .expect_err("contended state update reports an explicit timeout");
            assert!(matches!(
                error,
                HookStateError::LockTimeout { wait_ms: 500, .. }
            ));
            drop(held);
        });

        let state = load_state(&dir, "session-1").expect("state remains readable");
        assert_eq!(state.context_key.as_deref(), Some("existing-context"));
        assert!(state.prompt_recall_key.is_none());

        let state = mark_prompt_recall(
            &dir,
            "session-1",
            "new-recall",
            vec!["memory-1".to_owned()],
            &options(),
        )
        .expect("retry state update after unlock");
        assert_eq!(state.context_key.as_deref(), Some("existing-context"));
        assert_eq!(state.prompt_recall_key.as_deref(), Some("new-recall"));
        assert_eq!(state.prompt_recall_memory_ids, ["memory-1"]);
    }

    #[test]
    fn context_key_round_trips_without_clearing_pending() {
        let dir = temp_dir("context-key");
        mark_memory_pending(&dir, "session-1", "intent", &options()).expect("mark pending");

        let state = mark_context_key(&dir, "session-1", "agent=codex|store=personal", &options())
            .expect("mark context key");

        assert!(state.memory_pending);
        assert_eq!(
            state.context_key.as_deref(),
            Some("agent=codex|store=personal")
        );
    }

    fn delivered(id: &str, digest: &str) -> DeliveredMemory {
        DeliveredMemory {
            id: id.to_owned(),
            digest: digest.to_owned(),
        }
    }

    fn update(ids: &[&str], context_key: Option<&str>) -> DeliveryUpdate {
        DeliveryUpdate {
            epoch: 0,
            memories: ids.iter().map(|id| delivered(id, "d")).collect(),
            context_key: context_key.map(str::to_owned),
            ..DeliveryUpdate::default()
        }
    }

    #[test]
    fn apply_delivery_unions_memories_and_sets_keys() {
        let dir = temp_dir("delivery-apply");
        apply_delivery(&dir, "s", update(&["b", "a"], Some("ctx-1")), &options())
            .expect("first apply");
        let state = apply_delivery(
            &dir,
            "s",
            DeliveryUpdate {
                epoch: 0,
                memories: vec![delivered("a", "d"), delivered("c", "d")],
                context_key: None,
                prompt_recall_key: Some("recall-1".to_owned()),
                turn_recall_ids: vec!["t2".to_owned(), "t1".to_owned()],
                ..DeliveryUpdate::default()
            },
            &options(),
        )
        .expect("second apply");

        let ids = state
            .delivered
            .iter()
            .map(|memory| memory.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(state.delivered_context_key.as_deref(), Some("ctx-1"));
        assert_eq!(state.delivered_recall_key.as_deref(), Some("recall-1"));
        // Turn recall ids are recorded apart from the delivered set.
        assert_eq!(state.turn_recall_ids, ["t1", "t2"]);
        // The untracked fields change only through the explicit mirror.
        assert!(state.context_key.is_none());
        assert!(state.prompt_recall_key.is_none());
    }

    #[test]
    fn apply_delivery_mirrors_untracked_fields() {
        let dir = temp_dir("delivery-mirror");
        let mirror = |key: &str, ids: &[&str]| DeliveryUpdate {
            untracked_context_key: Some(key.to_owned()),
            untracked_context_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            ..update(&[], None)
        };
        apply_delivery(&dir, "s", mirror("ctx-a", &["a1"]), &options()).expect("first");
        // More of the same selection (a drained remainder) adds to its ids.
        let state = apply_delivery(&dir, "s", mirror("ctx-a", &["a2"]), &options()).expect("drain");
        assert_eq!(state.startup_memory_ids, ["a1", "a2"]);
        // A new selection replaces them, as untracked hooks do, apart from
        // memories still visible in the epoch (earlier drained batches).
        apply_delivery(&dir, "s", update(&["seen"], None), &options()).expect("delivered");
        let state =
            apply_delivery(&dir, "s", mirror("ctx-b", &["b1"]), &options()).expect("switch");
        assert_eq!(state.context_key.as_deref(), Some("ctx-b"));
        assert_eq!(state.startup_memory_ids, ["b1", "seen"]);
        let state = apply_delivery(
            &dir,
            "s",
            DeliveryUpdate {
                untracked_recall_key: Some("recall".to_owned()),
                untracked_recall_ids: vec!["r1".to_owned()],
                ..update(&[], None)
            },
            &options(),
        )
        .expect("recall");
        assert_eq!(state.prompt_recall_key.as_deref(), Some("recall"));
        assert_eq!(state.prompt_recall_memory_ids, ["r1"]);
        // Tracked selection keys are untouched by the mirror.
        assert!(state.delivered_context_key.is_none());
        assert!(state.delivered_recall_key.is_none());
    }

    #[test]
    fn bookkeeping_from_an_earlier_epoch_is_dropped() {
        let dir = temp_dir("delivery-stale-epoch");
        let state = reset_context_epoch(&dir, "s", &options()).expect("reset");
        assert_eq!(state.context_epoch, 1);

        // A hook that selected before the reset applies afterwards.
        let state = apply_delivery(&dir, "s", update(&["old"], Some("ctx")), &options())
            .expect("stale apply");
        assert!(state.delivered.is_empty());
        assert!(state.delivered_context_key.is_none());

        let fresh = DeliveryUpdate {
            epoch: 1,
            ..update(&["new"], Some("ctx-2"))
        };
        let state = apply_delivery(&dir, "s", fresh, &options()).expect("fresh apply");
        assert!(state.has_delivered("new", "d"));
        assert_eq!(state.delivered_context_key.as_deref(), Some("ctx-2"));
    }

    #[test]
    fn has_delivered_requires_matching_digest() {
        let state = HookState {
            delivered: vec![delivered("a", "old-text")],
            ..HookState::default()
        };
        assert!(state.has_delivered("a", "old-text"));
        assert!(!state.has_delivered("a", "new-text"));
        assert!(!state.has_delivered("b", "old-text"));
    }

    #[test]
    fn reset_context_epoch_clears_visibility_state_only() {
        let dir = temp_dir("delivery-reset");
        mark_memory_pending(&dir, "s", "intent", &options()).expect("pending");
        mark_startup_context(&dir, "s", "ctx", vec!["legacy".to_owned()], &options())
            .expect("startup");
        mark_prompt_recall(&dir, "s", "recall", vec!["r".to_owned()], &options()).expect("recall");
        apply_delivery(&dir, "s", update(&["a"], Some("tracked")), &options()).expect("apply");
        apply_delivery(
            &dir,
            "s",
            DeliveryUpdate {
                prompt_recall_key: Some("tracked-recall".to_owned()),
                turn_recall_ids: vec!["turn".to_owned()],
                ..update(&[], None)
            },
            &options(),
        )
        .expect("apply recall");

        let state = reset_context_epoch(&dir, "s", &options()).expect("reset");

        assert!(state.delivered.is_empty());
        assert!(state.delivered_context_key.is_none());
        assert!(state.delivered_recall_key.is_none());
        assert!(state.turn_recall_ids.is_empty());
        assert_eq!(state.context_epoch, 1);
        assert!(state.prompt_recall_updated_at.is_none());
        assert!(state.context_key.is_none());
        assert!(state.startup_memory_ids.is_empty());
        assert!(state.prompt_recall_key.is_none());
        assert!(state.prompt_recall_memory_ids.is_empty());
        assert!(state.context_epoch_started_at.is_some());
        // Durable-memory debt is about writes, not visibility.
        assert!(state.memory_pending);
    }

    #[test]
    fn delivery_state_round_trips_and_old_state_loads() {
        let dir = temp_dir("delivery-roundtrip");
        let path = state_path(&dir, "s");
        fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        fs::write(&path, r#"{"context_key":"ctx","startup_memory_ids":["a"]}"#).expect("old state");
        let old = load_state(&dir, "s").expect("old state loads");
        assert!(old.delivered.is_empty());
        assert_eq!(old.context_epoch, 0);

        apply_delivery(&dir, "s", update(&["a"], Some("ctx-2")), &options()).expect("apply");
        let state = load_state(&dir, "s").expect("reload");
        assert!(state.has_delivered("a", "d"));
        assert_eq!(state.delivered_context_key.as_deref(), Some("ctx-2"));
        // Untracked fields written by an older binary survive.
        assert_eq!(state.context_key.as_deref(), Some("ctx"));
        assert_eq!(state.startup_memory_ids, ["a"]);
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
