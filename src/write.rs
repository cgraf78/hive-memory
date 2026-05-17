//! Collision-safe atomic file publishing.
//!
//! Agent writes should create unique files and publish them with a final rename
//! instead of appending to shared hot files. This module owns that write path so
//! notes, JSON sidecars, and future importers share the same durability and
//! collision behavior.

use crate::id::WriteIdContext;
use std::error::Error;
use std::fmt::{self, Display};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

enum PublishMode {
    Replace,
    CreateNew,
}

/// Durability policy for an atomic write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncPolicy {
    /// Do not explicitly sync file or directory contents.
    Never,
    /// Try to sync, but return warnings instead of failing where possible.
    BestEffort,
    /// Fail the write if data or directory sync fails.
    Required,
}

/// Options for unique atomic writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicWriteOptions {
    /// Durability level for temp-file and parent-directory syncs.
    pub fsync: FsyncPolicy,
    /// Maximum generated-id attempts before reporting a collision failure.
    pub max_attempts: usize,
    /// Skip syncing the parent directory even when the fsync policy asks for it.
    pub skip_parent_fsync: bool,
}

/// Result of a successful unique write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicWriteResult {
    /// Extensionless id used as the final filename stem.
    pub id: String,
    /// Final path installed by the atomic rename.
    pub path: PathBuf,
    /// Non-fatal durability warnings from best-effort syncing.
    pub warnings: Vec<AtomicWriteWarning>,
}

/// Non-fatal durability warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicWriteWarning {
    /// Human-readable warning suitable for CLI diagnostics.
    pub message: String,
}

/// Atomic write failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtomicWriteError {
    /// The caller requested a unique file without a usable extension.
    EmptyExtension,
    /// A generated temp path already existed before this process wrote it.
    TempExists {
        /// Existing temp path that blocked the attempt.
        path: PathBuf,
    },
    /// All generated final/temp paths collided.
    CollisionLimit {
        /// Number of attempts that were made.
        attempts: usize,
    },
    /// Underlying filesystem error with operation context.
    Io {
        /// Operation that failed.
        action: &'static str,
        /// Path involved in the failure.
        path: PathBuf,
        /// Original error rendered for CLI diagnostics.
        message: String,
    },
}

impl Display for AtomicWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyExtension => write!(f, "file extension is required"),
            Self::TempExists { path } => {
                write!(f, "temporary file already exists: {}", path.display())
            }
            Self::CollisionLimit { attempts } => {
                write!(f, "could not find a unique path after {attempts} attempts")
            }
            Self::Io {
                action,
                path,
                message,
            } => write!(f, "failed to {action} {}: {message}", path.display()),
        }
    }
}

impl Error for AtomicWriteError {}

impl Default for AtomicWriteOptions {
    fn default() -> Self {
        Self {
            fsync: FsyncPolicy::BestEffort,
            max_attempts: 5,
            skip_parent_fsync: false,
        }
    }
}

/// Write bytes to a unique `<id>.<extension>` file under `parent`.
///
/// The generated id is returned so callers can reuse it for paired files, such
/// as a Markdown note and JSON event sidecar. If a generated final path already
/// exists, a new id is generated and the write is retried.
pub fn write_unique(
    parent: &Path,
    extension: &str,
    contents: &[u8],
    context: &WriteIdContext,
    options: &AtomicWriteOptions,
) -> Result<AtomicWriteResult, AtomicWriteError> {
    write_unique_with_id_generator(parent, extension, contents, options, || {
        crate::id::new_write_id(context)
    })
}

/// Atomically write bytes to a caller-selected final path, replacing it.
///
/// This is for singleton files like manifests. It shares the same
/// temp/write/fsync/rename behavior as unique inbox writes, but does not retry
/// because the final path is part of the caller's contract.
pub fn write_atomic(
    final_path: &Path,
    contents: &[u8],
    options: &AtomicWriteOptions,
) -> Result<Vec<AtomicWriteWarning>, AtomicWriteError> {
    write_atomic_with_mode(final_path, contents, options, PublishMode::Replace)
}

/// Atomically create bytes at a caller-selected final path.
///
/// Unlike [`write_atomic`], this fails if `final_path` already exists, including
/// a concurrent create between temp-file write and publish. Use this for
/// append-only inbox files where overwriting another writer would be data loss.
pub fn write_atomic_create_new(
    final_path: &Path,
    contents: &[u8],
    options: &AtomicWriteOptions,
) -> Result<Vec<AtomicWriteWarning>, AtomicWriteError> {
    write_atomic_with_mode(final_path, contents, options, PublishMode::CreateNew)
}

fn write_atomic_with_mode(
    final_path: &Path,
    contents: &[u8],
    options: &AtomicWriteOptions,
    mode: PublishMode,
) -> Result<Vec<AtomicWriteWarning>, AtomicWriteError> {
    let Some(parent) = final_path.parent() else {
        return Err(AtomicWriteError::Io {
            action: "resolve parent directory",
            path: final_path.to_path_buf(),
            message: "path has no parent".to_owned(),
        });
    };
    fs::create_dir_all(parent).map_err(|err| io_error("create parent directory", parent, err))?;
    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temp_path = parent.join(format!(
        ".tmp.{file_name}.{}-{}",
        std::process::id(),
        Uuid::now_v7()
    ));

    match write_temp_then_publish(&temp_path, final_path, contents, options, mode) {
        Ok(warnings) => Ok(warnings),
        Err(err) => {
            remove_temp(&temp_path);
            Err(err)
        }
    }
}

/// Test seam for unique writes with deterministic id generation.
///
/// Production code should use [`write_unique`]. Tests use this to force final
/// path collisions and verify retry behavior without relying on randomness.
pub fn write_unique_with_id_generator<F>(
    parent: &Path,
    extension: &str,
    contents: &[u8],
    options: &AtomicWriteOptions,
    mut next_id: F,
) -> Result<AtomicWriteResult, AtomicWriteError>
where
    F: FnMut() -> String,
{
    validate_extension(extension)?;
    fs::create_dir_all(parent).map_err(|err| io_error("create parent directory", parent, err))?;

    let attempts = options.max_attempts.max(1);
    for _ in 0..attempts {
        let id = next_id();
        let final_path = parent.join(format!("{id}.{extension}"));
        if final_path.exists() {
            continue;
        }

        let temp_path = parent.join(format!(".tmp.{id}.{}", std::process::id()));
        match write_temp_then_publish(
            &temp_path,
            &final_path,
            contents,
            options,
            PublishMode::CreateNew,
        ) {
            Ok(warnings) => {
                return Ok(AtomicWriteResult {
                    id,
                    path: final_path,
                    warnings,
                });
            }
            Err(AtomicWriteError::Io {
                action: "install final file",
                ..
            }) => {
                remove_temp(&temp_path);
                continue;
            }
            Err(AtomicWriteError::TempExists { .. }) => {
                // The temp file may belong to another live process or a crash
                // remnant. Do not remove it here; generate a new id instead.
                continue;
            }
            Err(err) => {
                remove_temp(&temp_path);
                return Err(err);
            }
        }
    }

    Err(AtomicWriteError::CollisionLimit { attempts })
}

fn write_temp_then_publish(
    temp_path: &Path,
    final_path: &Path,
    contents: &[u8],
    options: &AtomicWriteOptions,
    mode: PublishMode,
) -> Result<Vec<AtomicWriteWarning>, AtomicWriteError> {
    let mut warnings = Vec::new();

    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp_path)
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::AlreadyExists {
                    AtomicWriteError::TempExists {
                        path: temp_path.to_path_buf(),
                    }
                } else {
                    io_error("create temporary file", temp_path, err)
                }
            })?;
        file.write_all(contents)
            .map_err(|err| io_error("write temporary file", temp_path, err))?;
        sync_file(&file, temp_path, "sync temporary file", options)?;
    }

    publish_temp(temp_path, final_path, mode, options)?;
    sync_parent(final_path, options, &mut warnings)?;
    Ok(warnings)
}

fn publish_temp(
    temp_path: &Path,
    final_path: &Path,
    mode: PublishMode,
    options: &AtomicWriteOptions,
) -> Result<(), AtomicWriteError> {
    publish_temp_with_linker(temp_path, final_path, mode, options, |source, target| {
        fs::hard_link(source, target)
    })
}

fn publish_temp_with_linker<F>(
    temp_path: &Path,
    final_path: &Path,
    mode: PublishMode,
    options: &AtomicWriteOptions,
    link: F,
) -> Result<(), AtomicWriteError>
where
    F: FnOnce(&Path, &Path) -> io::Result<()>,
{
    match mode {
        PublishMode::Replace => {
            fs::rename(temp_path, final_path)
                .map_err(|err| io_error("install final file", final_path, err))?;
        }
        PublishMode::CreateNew => {
            // `rename` can overwrite an existing file on Unix. Creating a hard
            // link to the already-fsynced temp file gives unique inbox writes
            // the create-if-absent publish semantics they need.
            match link(temp_path, final_path) {
                Ok(()) => remove_temp(temp_path),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(AtomicWriteError::Io {
                        action: "install final file",
                        path: final_path.to_path_buf(),
                        message: "final path already exists".to_owned(),
                    });
                }
                Err(_) => {
                    // Some sync-backed filesystems, including the mounted
                    // cloud roots this tool targets, reject hard links even
                    // within a single directory. Fall back to a create-new copy:
                    // it preserves the critical no-overwrite contract for
                    // concurrent writers, though it cannot be as crash-atomic
                    // as link-based publish on local Unix filesystems.
                    copy_temp_create_new(temp_path, final_path, options)?;
                    remove_temp(temp_path);
                }
            }
        }
    }
    Ok(())
}

fn copy_temp_create_new(
    temp_path: &Path,
    final_path: &Path,
    options: &AtomicWriteOptions,
) -> Result<(), AtomicWriteError> {
    let mut temp =
        File::open(temp_path).map_err(|err| io_error("open temporary file", temp_path, err))?;
    let mut final_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(final_path)
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                AtomicWriteError::Io {
                    action: "install final file",
                    path: final_path.to_path_buf(),
                    message: "final path already exists".to_owned(),
                }
            } else {
                io_error("create final file", final_path, err)
            }
        })?;

    if let Err(err) = io::copy(&mut temp, &mut final_file) {
        remove_temp(final_path);
        return Err(io_error("write final file", final_path, err));
    }

    if let Err(err) = sync_file(&final_file, final_path, "sync final file", options) {
        remove_temp(final_path);
        return Err(err);
    }

    Ok(())
}

fn sync_file(
    file: &File,
    path: &Path,
    action: &'static str,
    options: &AtomicWriteOptions,
) -> Result<(), AtomicWriteError> {
    match options.fsync {
        FsyncPolicy::Never => Ok(()),
        FsyncPolicy::BestEffort => {
            let _ = file.sync_all();
            Ok(())
        }
        FsyncPolicy::Required => file.sync_all().map_err(|err| io_error(action, path, err)),
    }
}

fn sync_parent(
    final_path: &Path,
    options: &AtomicWriteOptions,
    warnings: &mut Vec<AtomicWriteWarning>,
) -> Result<(), AtomicWriteError> {
    if matches!(options.fsync, FsyncPolicy::Never) || options.skip_parent_fsync {
        return Ok(());
    }

    let Some(parent) = final_path.parent() else {
        return Ok(());
    };
    let sync_result = File::open(parent).and_then(|dir| dir.sync_all());

    match (options.fsync, sync_result) {
        (_, Ok(())) => Ok(()),
        (FsyncPolicy::BestEffort, Err(err)) => {
            warnings.push(AtomicWriteWarning {
                message: format!("parent directory fsync skipped: {err}"),
            });
            Ok(())
        }
        (FsyncPolicy::Required, Err(err)) => Err(io_error("sync parent directory", parent, err)),
        (FsyncPolicy::Never, _) => Ok(()),
    }
}

fn validate_extension(extension: &str) -> Result<(), AtomicWriteError> {
    if extension.is_empty() || extension.contains('/') || extension.contains('\\') {
        return Err(AtomicWriteError::EmptyExtension);
    }
    Ok(())
}

fn io_error(action: &'static str, path: &Path, err: std::io::Error) -> AtomicWriteError {
    AtomicWriteError::Io {
        action,
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

fn remove_temp(path: &Path) {
    let _ = fs::remove_file(path);
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
            "hive-memory-write-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn writes_temp_then_final_file() {
        let parent = temp_dir("happy");
        let result = write_unique_with_id_generator(
            &parent,
            "md",
            b"hello",
            &AtomicWriteOptions {
                fsync: FsyncPolicy::Never,
                ..AtomicWriteOptions::default()
            },
            || "id1".to_owned(),
        )
        .expect("write succeeds");

        assert_eq!(result.id, "id1");
        assert_eq!(
            fs::read_to_string(parent.join("id1.md")).expect("read"),
            "hello"
        );
        assert!(fs::read_dir(&parent).expect("read dir").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp.")
        }));
    }

    #[test]
    fn retries_when_final_path_collides() {
        let parent = temp_dir("collision");
        fs::write(parent.join("id1.md"), "existing").expect("precreate collision");
        let mut ids = ["id1", "id2"].into_iter();

        let result = write_unique_with_id_generator(
            &parent,
            "md",
            b"new",
            &AtomicWriteOptions {
                fsync: FsyncPolicy::Never,
                max_attempts: 5,
                skip_parent_fsync: false,
            },
            || ids.next().expect("id").to_owned(),
        )
        .expect("write succeeds");

        assert_eq!(result.id, "id2");
        assert_eq!(
            fs::read_to_string(parent.join("id1.md")).expect("read old"),
            "existing"
        );
        assert_eq!(
            fs::read_to_string(parent.join("id2.md")).expect("read new"),
            "new"
        );
    }

    #[test]
    fn retries_when_stale_temp_path_collides() {
        let parent = temp_dir("stale-temp");
        fs::write(
            parent.join(format!(".tmp.id1.{}", std::process::id())),
            "stale",
        )
        .expect("precreate stale temp");
        let mut ids = ["id1", "id2"].into_iter();

        let result = write_unique_with_id_generator(
            &parent,
            "md",
            b"new",
            &AtomicWriteOptions {
                fsync: FsyncPolicy::Never,
                max_attempts: 5,
                skip_parent_fsync: false,
            },
            || ids.next().expect("id").to_owned(),
        )
        .expect("write succeeds");

        assert_eq!(result.id, "id2");
        assert_eq!(
            fs::read_to_string(parent.join("id2.md")).expect("read new"),
            "new"
        );
        let stale_temp = parent.join(format!(".tmp.id1.{}", std::process::id()));
        assert!(stale_temp.is_file());
    }

    #[test]
    fn returns_collision_limit_after_retries() {
        let parent = temp_dir("collision-limit");
        fs::write(parent.join("id1.md"), "existing").expect("precreate collision");

        let err = write_unique_with_id_generator(
            &parent,
            "md",
            b"new",
            &AtomicWriteOptions {
                fsync: FsyncPolicy::Never,
                max_attempts: 5,
                skip_parent_fsync: false,
            },
            || "id1".to_owned(),
        )
        .expect_err("write fails");

        assert_eq!(err, AtomicWriteError::CollisionLimit { attempts: 5 });
    }

    #[test]
    fn required_fsync_succeeds_on_tempdir() {
        let parent = temp_dir("required-fsync");
        let result = write_unique_with_id_generator(
            &parent,
            "json",
            b"{}",
            &AtomicWriteOptions {
                fsync: FsyncPolicy::Required,
                max_attempts: 5,
                skip_parent_fsync: false,
            },
            || "id1".to_owned(),
        )
        .expect("write succeeds");

        assert!(result.warnings.is_empty());
        assert!(result.path.is_file());
    }

    #[test]
    fn rejects_empty_extension() {
        let parent = temp_dir("bad-extension");
        let err = write_unique_with_id_generator(
            &parent,
            "",
            b"hello",
            &AtomicWriteOptions::default(),
            || "id1".to_owned(),
        )
        .expect_err("write fails");

        assert_eq!(err, AtomicWriteError::EmptyExtension);
    }

    #[test]
    fn writes_fixed_path_atomically() {
        let parent = temp_dir("fixed");
        let path = parent.join("manifest.toml");

        write_atomic(
            &path,
            b"schema_version = 1\n",
            &AtomicWriteOptions {
                fsync: FsyncPolicy::Never,
                ..AtomicWriteOptions::default()
            },
        )
        .expect("write fixed path");

        assert_eq!(
            fs::read_to_string(path).expect("read fixed path"),
            "schema_version = 1\n"
        );
    }

    #[test]
    fn create_new_atomic_refuses_existing_final_path() {
        let parent = temp_dir("create-new");
        let path = parent.join("note.md");
        fs::write(&path, "existing").expect("precreate final");

        let err = write_atomic_create_new(
            &path,
            b"new",
            &AtomicWriteOptions {
                fsync: FsyncPolicy::Never,
                ..AtomicWriteOptions::default()
            },
        )
        .expect_err("write fails");

        assert!(matches!(
            err,
            AtomicWriteError::Io {
                action: "install final file",
                ..
            }
        ));
        assert_eq!(fs::read_to_string(path).expect("read final"), "existing");
    }

    #[test]
    fn create_new_copy_fallback_preserves_contents() {
        let parent = temp_dir("copy-fallback");
        let temp = parent.join(".tmp.note");
        let path = parent.join("note.md");
        fs::write(&temp, "new").expect("write temp");

        copy_temp_create_new(
            &temp,
            &path,
            &AtomicWriteOptions {
                fsync: FsyncPolicy::Never,
                ..AtomicWriteOptions::default()
            },
        )
        .expect("copy fallback succeeds");

        assert_eq!(fs::read_to_string(&path).expect("read final"), "new");
        assert!(temp.is_file());
    }

    #[test]
    fn create_new_publish_falls_back_when_hard_links_are_unavailable() {
        let parent = temp_dir("publish-copy-fallback");
        let temp = parent.join(".tmp.note");
        let path = parent.join("note.md");
        fs::write(&temp, "new").expect("write temp");

        publish_temp_with_linker(
            &temp,
            &path,
            PublishMode::CreateNew,
            &AtomicWriteOptions {
                fsync: FsyncPolicy::Never,
                ..AtomicWriteOptions::default()
            },
            |_source, _target| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "hard links disabled",
                ))
            },
        )
        .expect("publish falls back to copy");

        assert_eq!(fs::read_to_string(&path).expect("read final"), "new");
        assert!(!temp.exists());
    }

    #[test]
    fn create_new_copy_fallback_refuses_existing_final_path() {
        let parent = temp_dir("copy-fallback-collision");
        let temp = parent.join(".tmp.note");
        let path = parent.join("note.md");
        fs::write(&temp, "new").expect("write temp");
        fs::write(&path, "existing").expect("write final");

        let err = copy_temp_create_new(
            &temp,
            &path,
            &AtomicWriteOptions {
                fsync: FsyncPolicy::Never,
                ..AtomicWriteOptions::default()
            },
        )
        .expect_err("copy fallback fails");

        assert!(matches!(
            err,
            AtomicWriteError::Io {
                action: "install final file",
                ..
            }
        ));
        assert_eq!(fs::read_to_string(&path).expect("read final"), "existing");
        assert!(temp.is_file());
    }
}
