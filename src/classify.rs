//! Background LLM classification worker.
//!
//! The worker derives its queue from durable note/index provenance, takes a
//! per-store local lock to avoid overlapping work on one machine, and applies
//! every verdict through the same retag rewrite path as human corrections.

use crate::{config, index, llm, memory, note, write};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use time::OffsetDateTime;

const STALE_LOCK: Duration = Duration::from_secs(60 * 60);

/// Attempts one record gets per run before the worker moves past it. Without
/// this cap a single poisoned record (oversized body hitting the timeout, a
/// body that crashes the CLI) would sit at the head of the oldest-first queue
/// and re-abort every future run, permanently blocking everything behind it.
const MAX_ENTRY_ATTEMPTS: usize = 2;

#[derive(Debug, Clone, Copy, Default)]
struct ReportCounts {
    pending: usize,
    judged: usize,
    applied: usize,
    marked_only: usize,
    errors: usize,
}

/// Why a run did or did not do work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// Worker ran normally, even if there was no pending work.
    Ran,
    /// All candidate backends failed repeatedly.
    Aborted,
    /// Classifier is disabled by config or non-configurable store policy.
    SkippedDisabled,
    /// No allowed backend was available.
    SkippedNoBackend,
    /// Another local worker holds the per-store lock.
    SkippedLocked,
    /// Last run stamp is newer than the configured interval.
    SkippedFresh,
}

/// Per-run report for JSON output, stamps, and doctor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    /// Run outcome.
    pub outcome: Outcome,
    /// Backend that produced successful verdicts or last failed.
    pub backend: Option<String>,
    /// Pending records observed before the batch cap.
    pub pending: usize,
    /// Records that received a parseable verdict.
    pub judged: usize,
    /// Records whose kind changed.
    pub applied: usize,
    /// Records marked reviewed without changing kind.
    pub marked_only: usize,
    /// Failed backend invocations.
    pub errors: usize,
    /// Diagnostic text from the most recent backend failure, so doctor and
    /// JSON consumers can distinguish quota/auth exhaustion from crashes.
    /// `default` keeps stamps written before this field readable.
    #[serde(default)]
    pub last_error: Option<String>,
    /// RFC3339 timestamp for the run.
    pub at: String,
}

/// Decide what to persist for one verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyDecision {
    /// Persist the new kind plus LLM provenance.
    Apply(note::MemoryKind),
    /// Persist LLM provenance only.
    MarkOnly,
}

/// Input for one worker run.
#[derive(Debug)]
pub struct RunInput<'a> {
    /// Effective config.
    pub config: &'a config::Config,
    /// Local store alias.
    pub store_name: &'a str,
    /// Store root.
    pub store_root: &'a Path,
    /// Configured store sensitivity.
    pub store_sensitivity: config::Sensitivity,
    /// Current index entries for this store.
    pub entries: &'a [index::IndexEntry],
    /// Candidate backends in failover order.
    pub backends: Vec<llm::Backend>,
    /// Ignore freshness stamp when true.
    pub force: bool,
    /// Report what would happen without writing notes or stamps.
    pub dry_run: bool,
    /// Optional batch limit override.
    pub limit: Option<u32>,
    /// Atomic write options for note/event/stamp rewrites.
    pub options: write::AtomicWriteOptions,
}

/// Records still owed an LLM review at `verdict_version`.
///
/// Audience-restricted (`agent-private`) records are excluded entirely: their
/// bodies are visible only to the listed agents, while the classifier pipes
/// bodies to whichever backend CLI wins detection. Skipping them keeps the
/// classifier consistent with the store's own visibility model; such records
/// keep write-time/manual kinds and read-time relevance handling.
pub fn pending_entries(
    entries: &[index::IndexEntry],
    verdict_version: u32,
) -> Vec<&index::IndexEntry> {
    let mut pending: Vec<&index::IndexEntry> = entries
        .iter()
        .filter(|entry| entry.audience.is_empty())
        .filter(|entry| match &entry.classified {
            None => true,
            Some(classified) => match classified.source {
                note::ClassifierSource::Manual => false,
                note::ClassifierSource::Llm => classified.verdict_version < verdict_version,
            },
        })
        .collect();
    pending.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    pending
}

/// Map a structured verdict onto a persistence decision.
pub fn apply_policy(
    verdict: llm::Verdict,
    current_kind: Option<note::MemoryKind>,
    apply_confidence: llm::VerdictConfidence,
) -> ApplyDecision {
    match verdict.kind {
        llm::VerdictKind::Unclear => ApplyDecision::MarkOnly,
        llm::VerdictKind::Kind(kind) => {
            if verdict.confidence < apply_confidence || Some(kind) == current_kind {
                ApplyDecision::MarkOnly
            } else {
                ApplyDecision::Apply(kind)
            }
        }
    }
}

/// Lock path under `<state_dir>/classifier/<store_name>/`.
pub fn lock_path(state_dir: &Path, store_name: &str) -> PathBuf {
    state_dir
        .join("classifier")
        .join(store_name)
        .join("classifier.lock")
}

/// Last-run stamp path under `<state_dir>/classifier/<store_name>/`.
pub fn stamp_path(state_dir: &Path, store_name: &str) -> PathBuf {
    state_dir
        .join("classifier")
        .join(store_name)
        .join("last-run.json")
}

/// Return whether `hm hook stop` should spawn a detached worker.
pub fn should_spawn(
    mode: &str,
    min_interval: Duration,
    state_dir: &Path,
    store_name: &str,
    now: OffsetDateTime,
) -> bool {
    if mode == "off" || lock_path(state_dir, store_name).exists() {
        return false;
    }
    !stamp_is_fresh(&stamp_path(state_dir, store_name), min_interval, now)
}

/// Resolve candidate backends under the classifier privacy policy.
///
/// In plain `mode = "auto"`, only CLIs with matching `[agents]` entries are
/// considered because those agents already receive memory context. Explicit
/// `backend = ...` or `mode = "on"` is an opt-in to the selected adapter.
pub fn configured_backends(config: &config::Config) -> Vec<llm::Backend> {
    let backends = llm::detect_all(
        config.classifier.backend.as_deref(),
        &config.classifier.command,
        config.classifier.model.as_deref(),
        None,
    );
    if config.classifier.mode != "auto" || config.classifier.backend.is_some() {
        return backends;
    }
    backends
        .into_iter()
        .filter(|backend| config.agents.contains_key(&backend.label))
        .collect()
}

/// Run one bounded classification pass.
pub fn run(input: RunInput<'_>) -> RunReport {
    let now = OffsetDateTime::now_utc();
    if (!input.force && input.config.classifier.mode == "off")
        || input.store_sensitivity == config::Sensitivity::Secret
    {
        return report(Outcome::SkippedDisabled, None, ReportCounts::default(), now);
    }

    let stamp = stamp_path(&input.config.state_dir, input.store_name);
    if !input.force
        && stamp_is_fresh(
            &stamp,
            input.config.classifier_min_interval(),
            OffsetDateTime::now_utc(),
        )
    {
        return report(Outcome::SkippedFresh, None, ReportCounts::default(), now);
    }

    let lock = match LockGuard::acquire(&input.config.state_dir, input.store_name) {
        Ok(Some(lock)) => lock,
        Ok(None) => return report(Outcome::SkippedLocked, None, ReportCounts::default(), now),
        Err(_) => return report(Outcome::SkippedLocked, None, ReportCounts::default(), now),
    };

    if input.backends.is_empty() {
        let output = report(
            Outcome::SkippedNoBackend,
            None,
            ReportCounts::default(),
            now,
        );
        write_stamp_if_needed(&stamp, &output, input.dry_run, &input.options);
        drop(lock);
        return output;
    }

    let apply_confidence =
        llm::VerdictConfidence::from_label(&input.config.classifier.apply_confidence)
            .expect("validated classifier confidence");
    let timeout = Duration::from_secs(input.config.classifier.timeout_seconds);
    let limit = input
        .limit
        .unwrap_or(input.config.classifier.batch_limit)
        .try_into()
        .unwrap_or(usize::MAX);
    let pending = pending_entries(input.entries, llm::VERDICT_VERSION);
    let batch: Vec<&index::IndexEntry> = pending.iter().copied().take(limit).collect();

    let mut backend_index = 0usize;
    let mut consecutive_errors = 0usize;
    let mut entry_attempts = 0usize;
    let mut judged = 0usize;
    let mut applied = 0usize;
    let mut marked_only = 0usize;
    let mut errors = 0usize;
    let mut last_error: Option<String> = None;
    let mut last_backend = input.backends.first().map(|backend| backend.label.clone());
    let mut outcome = Outcome::Ran;

    let mut index = 0usize;
    while index < batch.len() {
        // A slow run with retries can legitimately outlive STALE_LOCK
        // (batch_limit * timeout alone can exceed it), so refresh the lock
        // mtime each iteration to keep other workers from stealing it.
        lock.touch();
        let backend = &input.backends[backend_index];
        last_backend = Some(backend.label.clone());
        let entry = batch[index];
        let prompt = llm::classification_prompt(
            &entry.body,
            &entry.scope,
            entry.project_id.as_deref(),
            entry.kind,
        );

        let verdict = match llm::invoke(backend, &prompt, timeout) {
            Ok(verdict) => {
                consecutive_errors = 0;
                verdict
            }
            Err(err) => {
                errors += 1;
                consecutive_errors += 1;
                entry_attempts += 1;
                last_error = Some(format!("{}: {err}", backend.label));
                if entry_attempts >= MAX_ENTRY_ATTEMPTS {
                    // Move past a likely poisoned record; it stays pending for
                    // future runs but no longer blocks the rest of the queue.
                    // Deliberately do NOT reset consecutive_errors here: a dead
                    // backend must still rotate instead of burning the batch.
                    index += 1;
                    entry_attempts = 0;
                }
                if consecutive_errors >= 3 {
                    backend_index += 1;
                    consecutive_errors = 0;
                    if backend_index >= input.backends.len() {
                        outcome = Outcome::Aborted;
                        break;
                    }
                }
                continue;
            }
        };

        judged += 1;
        let mut decision = apply_policy(verdict, entry.kind, apply_confidence);
        if let ApplyDecision::Apply(kind) = decision
            && memory::validate_kind_context(Some(kind), &entry.scope, entry.project_id.as_deref())
                .is_err()
        {
            decision = ApplyDecision::MarkOnly;
        }
        let classified = note::ClassifiedBy {
            source: note::ClassifierSource::Llm,
            backend: Some(backend.label.clone()),
            at: rfc3339(OffsetDateTime::now_utc()),
            verdict_version: llm::VERDICT_VERSION,
            confidence: Some(verdict.confidence.label().to_owned()),
        };

        match decision {
            ApplyDecision::Apply(kind) => {
                applied += 1;
                if !input.dry_run {
                    apply_update(&input, entry, Some(kind), classified);
                }
            }
            ApplyDecision::MarkOnly => {
                marked_only += 1;
                if !input.dry_run {
                    apply_update(&input, entry, entry.kind, classified);
                }
            }
        }
        index += 1;
        entry_attempts = 0;
    }

    let mut output = report(
        outcome,
        last_backend,
        ReportCounts {
            pending: pending.len(),
            judged,
            applied,
            marked_only,
            errors,
        },
        now,
    );
    output.last_error = last_error;
    write_stamp_if_needed(&stamp, &output, input.dry_run, &input.options);
    drop(lock);
    output
}

fn apply_update(
    input: &RunInput<'_>,
    entry: &index::IndexEntry,
    kind: Option<note::MemoryKind>,
    classified: note::ClassifiedBy,
) {
    let Ok(contents) = fs::read_to_string(input.store_root.join(&entry.note_path)) else {
        return;
    };
    let Ok(parsed) = note::parse_note(&contents) else {
        return;
    };
    if parsed.front_matter.kind != entry.kind || parsed.front_matter.classified != entry.classified
    {
        return;
    }
    let _ = memory::retag_record(memory::RetagRecordInput {
        root: input.store_root,
        note_path: &entry.note_path,
        kind,
        classified: memory::ClassifiedUpdate::Set(classified),
        options: input.options.clone(),
    });
}

fn stamp_is_fresh(path: &Path, min_interval: Duration, now: OffsetDateTime) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(report) = serde_json::from_str::<RunReport>(&contents) else {
        return false;
    };
    let Ok(at) = OffsetDateTime::parse(&report.at, &time::format_description::well_known::Rfc3339)
    else {
        return false;
    };
    let Ok(age) = Duration::try_from(now - at) else {
        return false;
    };
    age < min_interval
}

fn write_stamp_if_needed(
    path: &Path,
    report: &RunReport,
    dry_run: bool,
    options: &write::AtomicWriteOptions,
) {
    if dry_run {
        return;
    }
    if !matches!(
        report.outcome,
        Outcome::Ran | Outcome::Aborted | Outcome::SkippedNoBackend
    ) {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(report) {
        let _ = write::write_atomic(path, format!("{json}\n").as_bytes(), options);
    }
}

fn report(
    outcome: Outcome,
    backend: Option<String>,
    counts: ReportCounts,
    at: OffsetDateTime,
) -> RunReport {
    RunReport {
        outcome,
        backend,
        pending: counts.pending,
        judged: counts.judged,
        applied: counts.applied,
        marked_only: counts.marked_only,
        errors: counts.errors,
        last_error: None,
        at: rfc3339(at),
    }
}

fn rfc3339(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting is infallible for UTC timestamps")
}

struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    fn acquire(state_dir: &Path, store_name: &str) -> std::io::Result<Option<Self>> {
        let path = lock_path(state_dir, store_name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                Ok(Some(Self { path }))
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_is_stale(&path) {
                    let _ = fs::remove_file(&path);
                    return Self::acquire(state_dir, store_name);
                }
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    /// Refresh the lock mtime so `lock_is_stale` keeps seeing a live worker.
    ///
    /// Staleness is mtime-based and the file is otherwise only written at
    /// acquire time, so a long run must heartbeat or a second worker would
    /// steal the lock mid-run and judge the same records concurrently.
    fn touch(&self) {
        let _ = fs::write(&self.path, format!("{}\n", std::process::id()));
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_is_stale(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age > STALE_LOCK)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note::MemoryKind;

    fn entry(
        kind: Option<MemoryKind>,
        classified: Option<note::ClassifiedBy>,
    ) -> index::IndexEntry {
        index::IndexEntry {
            id: format!("id-{}", classified.is_some()),
            store_id: "store-id".to_owned(),
            entry_kind: note::EntryKind::Remember,
            scope: "global".to_owned(),
            project_id: None,
            audience: Vec::new(),
            tags: Vec::new(),
            subject: None,
            confidence: note::Confidence::High,
            kind,
            classified,
            agent_id: "codex".to_owned(),
            host_id: "taylor".to_owned(),
            created_at: "2026-06-12T00:00:00Z".to_owned(),
            body: "body".to_owned(),
            note_path: "inbox/notes/2026/06/12/id.md".to_owned(),
            event_path: None,
        }
    }

    fn llm_classified(version: u32) -> note::ClassifiedBy {
        note::ClassifiedBy {
            source: note::ClassifierSource::Llm,
            backend: Some("fake".to_owned()),
            at: "2026-06-12T00:00:00Z".to_owned(),
            verdict_version: version,
            confidence: Some("high".to_owned()),
        }
    }

    fn manual() -> note::ClassifiedBy {
        note::ClassifiedBy {
            source: note::ClassifierSource::Manual,
            backend: None,
            at: "2026-06-12T00:00:00Z".to_owned(),
            verdict_version: 0,
            confidence: None,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "hive-memory-classify-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn pending_selects_unreviewed_and_stale_llm_versions() {
        let entries = vec![
            entry(None, None),
            entry(None, Some(llm_classified(1))),
            entry(None, Some(llm_classified(0))),
            entry(Some(MemoryKind::Preference), Some(manual())),
        ];

        let pending = pending_entries(&entries, 1);

        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn pending_excludes_audience_restricted_records() {
        let mut agent_private = entry(None, None);
        agent_private.id = "agent-private".to_owned();
        agent_private.scope = "agent-private".to_owned();
        agent_private.audience = vec!["codex".to_owned()];
        let entries = vec![agent_private, entry(None, None)];

        let pending = pending_entries(&entries, 1);

        // The unrestricted record is still pending; the audience-restricted
        // one must never be piped to an arbitrary backend CLI.
        assert_eq!(pending.len(), 1);
        assert!(pending[0].audience.is_empty());
    }

    #[test]
    fn apply_policy_respects_confidence_and_unclear() {
        assert_eq!(
            apply_policy(
                llm::Verdict {
                    kind: llm::VerdictKind::Kind(MemoryKind::Incident),
                    confidence: llm::VerdictConfidence::High,
                },
                None,
                llm::VerdictConfidence::High,
            ),
            ApplyDecision::Apply(MemoryKind::Incident)
        );
        assert_eq!(
            apply_policy(
                llm::Verdict {
                    kind: llm::VerdictKind::Kind(MemoryKind::Incident),
                    confidence: llm::VerdictConfidence::Medium,
                },
                None,
                llm::VerdictConfidence::High,
            ),
            ApplyDecision::MarkOnly
        );
        assert_eq!(
            apply_policy(
                llm::Verdict {
                    kind: llm::VerdictKind::Unclear,
                    confidence: llm::VerdictConfidence::High,
                },
                None,
                llm::VerdictConfidence::High,
            ),
            ApplyDecision::MarkOnly
        );
        assert_eq!(
            apply_policy(
                llm::Verdict {
                    kind: llm::VerdictKind::Kind(MemoryKind::Incident),
                    confidence: llm::VerdictConfidence::High,
                },
                Some(MemoryKind::Incident),
                llm::VerdictConfidence::High,
            ),
            ApplyDecision::MarkOnly
        );
    }

    #[test]
    fn should_spawn_uses_only_local_checks() {
        let state = temp_dir("spawn");
        let now = OffsetDateTime::now_utc();

        assert!(!should_spawn(
            "off",
            Duration::from_secs(60),
            &state,
            "personal",
            now
        ));
        assert!(should_spawn(
            "auto",
            Duration::from_secs(60),
            &state,
            "personal",
            now
        ));

        let stamp = stamp_path(&state, "personal");
        let report = report(Outcome::Ran, None, ReportCounts::default(), now);
        write_stamp_if_needed(
            &stamp,
            &report,
            false,
            &write::AtomicWriteOptions {
                fsync: write::FsyncPolicy::Never,
                ..write::AtomicWriteOptions::default()
            },
        );
        assert!(!should_spawn(
            "auto",
            Duration::from_secs(60),
            &state,
            "personal",
            now
        ));

        fs::remove_file(&stamp).expect("remove stamp");
        let lock = lock_path(&state, "personal");
        fs::create_dir_all(lock.parent().expect("parent")).expect("create parent");
        fs::write(&lock, "pid").expect("write lock");
        assert!(!should_spawn(
            "auto",
            Duration::from_secs(60),
            &state,
            "personal",
            now
        ));
    }

    fn poison_config(dir: &Path) -> config::Config {
        let fixture = format!(
            "{}/tests/fixtures/fake-llm-poison",
            env!("CARGO_MANIFEST_DIR")
        );
        config::LoadedConfig::from_str_with_env(
            &format!(
                r#"
                default_store = "personal"
                state_dir = "{state}"
                cache_dir = "{cache}"

                [stores.personal]
                root = "{root}"

                [classifier]
                mode = "on"
                backend = "command"
                command = ["{fixture}"]
                timeout_seconds = 5
                "#,
                state = dir.join("state").display(),
                cache = dir.join("cache").display(),
                root = dir.join("store").display(),
            ),
            |_| None,
        )
        .expect("config loads")
        .config
    }

    fn run_input<'a>(
        config: &'a config::Config,
        store_root: &'a Path,
        entries: &'a [index::IndexEntry],
    ) -> RunInput<'a> {
        RunInput {
            config,
            store_name: "personal",
            store_root,
            store_sensitivity: config::Sensitivity::Private,
            entries,
            backends: configured_backends(config),
            force: true,
            dry_run: false,
            limit: None,
            options: write::AtomicWriteOptions {
                fsync: write::FsyncPolicy::Never,
                ..write::AtomicWriteOptions::default()
            },
        }
    }

    #[test]
    fn run_skips_poisoned_record_and_continues() {
        let dir = temp_dir("poison-skip");
        let config = poison_config(&dir);
        let mut poisoned = entry(None, None);
        poisoned.id = "poisoned".to_owned();
        poisoned.created_at = "2026-06-10T00:00:00Z".to_owned();
        poisoned.body = "POISON record the backend cannot judge".to_owned();
        let mut healthy = entry(None, None);
        healthy.id = "healthy".to_owned();
        healthy.created_at = "2026-06-11T00:00:00Z".to_owned();
        let entries = vec![poisoned, healthy];

        let report = run(run_input(&config, &dir.join("store"), &entries));

        // The poisoned head of the queue burns MAX_ENTRY_ATTEMPTS errors,
        // then the worker moves on and still judges the record behind it.
        assert_eq!(report.outcome, Outcome::Ran);
        assert_eq!(report.errors, MAX_ENTRY_ATTEMPTS);
        assert_eq!(report.judged, 1);
        assert_eq!(report.applied, 1);
        assert_eq!(report.pending, 2);
        let last_error = report.last_error.expect("last error");
        assert!(
            last_error.contains("poisoned record refused"),
            "missing stderr excerpt: {last_error}"
        );
    }

    #[test]
    fn run_aborts_when_every_record_fails_on_all_backends() {
        let dir = temp_dir("poison-abort");
        let config = poison_config(&dir);
        let mut first = entry(None, None);
        first.id = "poisoned-1".to_owned();
        first.created_at = "2026-06-10T00:00:00Z".to_owned();
        first.body = "POISON one".to_owned();
        let mut second = entry(None, None);
        second.id = "poisoned-2".to_owned();
        second.created_at = "2026-06-11T00:00:00Z".to_owned();
        second.body = "POISON two".to_owned();
        let entries = vec![first, second];

        let report = run(run_input(&config, &dir.join("store"), &entries));

        // Entry skips must not reset the consecutive-error counter, or a dead
        // backend would burn the whole batch instead of rotating/aborting.
        assert_eq!(report.outcome, Outcome::Aborted);
        assert_eq!(report.errors, 3);
        assert_eq!(report.judged, 0);
        assert!(report.last_error.is_some());
    }

    #[test]
    fn lock_touch_refreshes_staleness() {
        let state = temp_dir("lock-touch");
        let lock = LockGuard::acquire(&state, "personal")
            .expect("acquire io")
            .expect("acquire lock");
        let path = lock_path(&state, "personal");

        backdate(&path, STALE_LOCK + Duration::from_secs(60));
        assert!(lock_is_stale(&path));

        lock.touch();
        assert!(!lock_is_stale(&path));
    }

    /// Set a file's mtime into the past without sleeping in the test.
    fn backdate(path: &Path, age: Duration) {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path without NUL");
        let then = SystemTime::now() - age;
        let secs = then
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("after epoch")
            .as_secs();
        let times = [
            libc::timeval {
                tv_sec: secs.try_into().expect("mtime seconds fit timeval"),
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: secs.try_into().expect("mtime seconds fit timeval"),
                tv_usec: 0,
            },
        ];
        let result = unsafe { libc::utimes(c_path.as_ptr(), times.as_ptr()) };
        assert_eq!(result, 0, "utimes failed");
    }
}
