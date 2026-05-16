//! Adapter render file safety.
//!
//! Rendered adapter files are disposable views over canonical memory, but they
//! are still user-visible files that agents may load at startup. This module
//! owns the generated-file marker and checksum rules so every adapter refuses
//! drift the same way before overwriting a file.

use crate::write;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};

const GENERATED_PREFIX: &str = "<!-- hive-memory:generated v=1 sha256=";
const GENERATED_SUFFIX: &str = " -->";

/// Input for rendering a generated adapter file.
#[derive(Debug, Clone)]
pub struct RenderFileInput<'a> {
    /// Output path configured for the adapter.
    pub output: &'a Path,
    /// Body bytes after the generated marker header.
    pub body: &'a str,
    /// Atomic writer behavior.
    pub options: write::AtomicWriteOptions,
    /// Whether a drifted generated file may be overwritten.
    pub force: bool,
    /// Whether a force overwrite must leave a side-by-side backup.
    pub backup: bool,
}

/// Result of writing or refreshing a generated render file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderFileReport {
    /// Final output path.
    pub output: PathBuf,
    /// SHA-256 of the body after the generated header.
    pub sha256: String,
    /// Whether the write path changed the file contents.
    pub written: bool,
    /// Backup path written before a forced overwrite.
    pub backup_path: Option<PathBuf>,
}

/// Render-file failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// Filesystem operation failed.
    Io {
        /// Operation that failed.
        action: &'static str,
        /// Path involved in the failure.
        path: PathBuf,
        /// Original error rendered for diagnostics.
        message: String,
    },
    /// Existing output is not managed by hive-memory.
    MissingHeader {
        /// Refused path.
        path: PathBuf,
    },
    /// Existing output was edited after the last generated write.
    DriftedChecksum {
        /// Refused path.
        path: PathBuf,
        /// Header checksum.
        expected: String,
        /// Actual checksum of the current body.
        actual: String,
    },
}

impl Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                message,
            } => write!(f, "failed to {action} {}: {message}", path.display()),
            Self::MissingHeader { path } => write!(
                f,
                "refusing to overwrite non-generated render file {}",
                path.display()
            ),
            Self::DriftedChecksum {
                path,
                expected,
                actual,
            } => write!(
                f,
                "refusing to overwrite edited render file {}; header sha256={expected}, actual sha256={actual}",
                path.display()
            ),
        }
    }
}

impl Error for RenderError {}

/// Write a generated adapter file with drift protection.
///
/// New files are created directly. Existing files must carry a valid
/// hive-memory generated header, and the header checksum must still match the
/// body unless the caller explicitly forces an overwrite with a backup. This
/// protects users from losing manual edits in files that look generated but were
/// changed by hand.
pub fn write_rendered_file(input: RenderFileInput<'_>) -> Result<RenderFileReport, RenderError> {
    let rendered = render_generated(input.body);
    let sha256 = body_sha256(input.body);
    let mut backup_path = None;

    if input.output.exists() {
        let existing = fs::read_to_string(input.output)
            .map_err(|err| io_error("read render output", input.output, err))?;
        validate_existing(input.output, &existing, input.force && input.backup)?;
        if existing == rendered {
            return Ok(RenderFileReport {
                output: input.output.to_path_buf(),
                sha256,
                written: false,
                backup_path: None,
            });
        }
        if input.force && input.backup {
            let path = backup_render_path(input.output);
            write::write_atomic(&path, existing.as_bytes(), &input.options).map_err(|err| {
                RenderError::Io {
                    action: "write render backup",
                    path: path.clone(),
                    message: err.to_string(),
                }
            })?;
            backup_path = Some(path);
        }
    }

    write::write_atomic(input.output, rendered.as_bytes(), &input.options).map_err(|err| {
        RenderError::Io {
            action: "write render output",
            path: input.output.to_path_buf(),
            message: err.to_string(),
        }
    })?;

    Ok(RenderFileReport {
        output: input.output.to_path_buf(),
        sha256,
        written: true,
        backup_path,
    })
}

/// Recompute the generated marker for an existing rendered file body.
///
/// This is the `--upgrade-marker` primitive. It intentionally does not compare
/// the old checksum to the body, because its purpose is to re-bless an
/// intentional renderer/header change while leaving the body bytes untouched.
pub fn upgrade_marker(
    output: &Path,
    options: write::AtomicWriteOptions,
) -> Result<RenderFileReport, RenderError> {
    let existing =
        fs::read_to_string(output).map_err(|err| io_error("read render output", output, err))?;
    let (_old_sha, body) =
        split_generated(&existing).ok_or_else(|| RenderError::MissingHeader {
            path: output.into(),
        })?;
    let rendered = render_generated(body);
    let sha256 = body_sha256(body);
    let written = rendered != existing;
    if written {
        write::write_atomic(output, rendered.as_bytes(), &options).map_err(|err| {
            RenderError::Io {
                action: "write render output",
                path: output.to_path_buf(),
                message: err.to_string(),
            }
        })?;
    }

    Ok(RenderFileReport {
        output: output.to_path_buf(),
        sha256,
        written,
        backup_path: None,
    })
}

/// Render the complete generated file contents.
pub fn render_generated(body: &str) -> String {
    format!(
        "{}{}{}\n{}",
        GENERATED_PREFIX,
        body_sha256(body),
        GENERATED_SUFFIX,
        body
    )
}

/// Return the SHA-256 covered by a generated render marker.
pub fn body_sha256(body: &str) -> String {
    format!("{:x}", Sha256::digest(body.as_bytes()))
}

fn validate_existing(path: &Path, contents: &str, force: bool) -> Result<(), RenderError> {
    let Some((expected, body)) = split_generated(contents) else {
        return Err(RenderError::MissingHeader { path: path.into() });
    };
    let actual = body_sha256(body);
    if expected != actual && !force {
        return Err(RenderError::DriftedChecksum {
            path: path.into(),
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(())
}

fn split_generated(contents: &str) -> Option<(&str, &str)> {
    // Only the first line is structural metadata. The body is allowed to contain
    // arbitrary Markdown, including marker-like text, without affecting drift
    // detection or giving generated content a way to rewrite its own checksum.
    let (header, body) = contents.split_once('\n')?;
    let checksum = header
        .strip_prefix(GENERATED_PREFIX)?
        .strip_suffix(GENERATED_SUFFIX)?;
    Some((checksum, body))
}

fn backup_render_path(output: &Path) -> PathBuf {
    output.with_extension(format!(
        "{}hive-memory.bak",
        output
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| format!("{extension}."))
            .unwrap_or_default()
    ))
}

fn io_error(action: &'static str, path: &Path, err: std::io::Error) -> RenderError {
    RenderError::Io {
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
            "hive-memory-render-{name}-{}-{nanos}",
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
    fn generated_header_hashes_body_after_header() {
        let body = "Hive Memory\n";
        let rendered = render_generated(body);
        let (checksum, rendered_body) = split_generated(&rendered).expect("generated split");

        assert_eq!(rendered_body, body);
        assert_eq!(checksum, body_sha256(body));
    }

    #[test]
    fn writes_new_render_file_and_skips_unchanged_rewrite() {
        let dir = temp_dir("new");
        let output = dir.join("codex.md");

        let first = write_rendered_file(RenderFileInput {
            output: &output,
            body: "body\n",
            options: options(),
            force: false,
            backup: false,
        })
        .expect("write render");
        let second = write_rendered_file(RenderFileInput {
            output: &output,
            body: "body\n",
            options: options(),
            force: false,
            backup: false,
        })
        .expect("write render");

        assert!(first.written);
        assert!(!second.written);
        let has_generated_header = fs::read_to_string(output)
            .expect("read")
            .starts_with(GENERATED_PREFIX);
        assert!(has_generated_header);
    }

    #[test]
    fn refuses_non_generated_existing_file() {
        let dir = temp_dir("missing-header");
        let output = dir.join("codex.md");
        fs::write(&output, "manual\n").expect("manual file");

        let error = write_rendered_file(RenderFileInput {
            output: &output,
            body: "body\n",
            options: options(),
            force: false,
            backup: false,
        })
        .expect_err("missing header");

        assert!(matches!(error, RenderError::MissingHeader { .. }));
    }

    #[test]
    fn refuses_drifted_generated_file_without_force() {
        let dir = temp_dir("drift");
        let output = dir.join("codex.md");
        fs::write(&output, render_generated("body\n")).expect("generated file");
        fs::write(
            &output,
            fs::read_to_string(&output).expect("read") + "manual\n",
        )
        .expect("drift file");

        let error = write_rendered_file(RenderFileInput {
            output: &output,
            body: "new body\n",
            options: options(),
            force: false,
            backup: false,
        })
        .expect_err("drift");

        assert!(matches!(error, RenderError::DriftedChecksum { .. }));
    }

    #[test]
    fn force_overwrite_writes_backup_for_drifted_file() {
        let dir = temp_dir("force");
        let output = dir.join("codex.md");
        fs::write(&output, render_generated("body\n")).expect("generated file");
        fs::write(
            &output,
            fs::read_to_string(&output).expect("read") + "manual\n",
        )
        .expect("drift file");

        let report = write_rendered_file(RenderFileInput {
            output: &output,
            body: "new body\n",
            options: options(),
            force: true,
            backup: true,
        })
        .expect("force write");

        let backup = report.backup_path.expect("backup path");
        assert!(backup.is_file());
        let backup_has_manual_edit = fs::read_to_string(backup)
            .expect("backup")
            .contains("manual");
        let output_has_new_body = fs::read_to_string(output)
            .expect("output")
            .contains("new body");
        assert!(backup_has_manual_edit);
        assert!(output_has_new_body);
    }

    #[test]
    fn force_without_backup_still_refuses_drifted_file() {
        let dir = temp_dir("force-no-backup");
        let output = dir.join("codex.md");
        fs::write(&output, render_generated("body\n")).expect("generated file");
        fs::write(
            &output,
            fs::read_to_string(&output).expect("read") + "manual\n",
        )
        .expect("drift file");

        let error = write_rendered_file(RenderFileInput {
            output: &output,
            body: "new body\n",
            options: options(),
            force: true,
            backup: false,
        })
        .expect_err("drift");

        assert!(matches!(error, RenderError::DriftedChecksum { .. }));
    }

    #[test]
    fn upgrade_marker_rehashes_existing_body_without_drift_check() {
        let dir = temp_dir("upgrade");
        let output = dir.join("codex.md");
        fs::write(
            &output,
            format!("{GENERATED_PREFIX}bad{GENERATED_SUFFIX}\nbody\n"),
        )
        .expect("bad marker");

        let report = upgrade_marker(&output, options()).expect("upgrade marker");
        let contents = fs::read_to_string(output).expect("read upgraded");

        assert!(report.written);
        assert!(contents.contains(&body_sha256("body\n")));
        assert!(contents.ends_with("body\n"));
    }
}
