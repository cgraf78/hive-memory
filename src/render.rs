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

/// Input for installing one adapter include marker into an agent instruction file.
#[derive(Debug, Clone)]
pub struct InstallAdapterInput<'a> {
    /// Adapter id, such as `codex` or `claude`.
    pub adapter: &'a str,
    /// Generated adapter output to include from the instruction file.
    pub output: &'a Path,
    /// Adapter instruction file that the agent loads.
    pub install_target: &'a Path,
    /// Stable shared policy block body.
    pub policy_body: &'a str,
    /// Atomic writer behavior.
    pub options: write::AtomicWriteOptions,
}

/// Result of installing one adapter include marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallAdapterReport {
    /// Resolved file actually edited. Symlink targets are canonicalized.
    pub target: PathBuf,
    /// Whether the instruction file changed.
    pub written: bool,
    /// Rolling backup path written before the change.
    pub backup_path: Option<PathBuf>,
    /// Backup metadata path written before the change.
    pub metadata_path: Option<PathBuf>,
}

/// Input for uninstalling adapter markers from an agent instruction file.
#[derive(Debug, Clone)]
pub struct UninstallAdapterInput<'a> {
    /// Adapter id whose include marker should be removed.
    pub adapter: &'a str,
    /// Adapter instruction file that the agent loads.
    pub install_target: &'a Path,
    /// Remove the shared policy block as well.
    pub all: bool,
    /// Atomic writer behavior.
    pub options: write::AtomicWriteOptions,
}

/// Result of uninstalling adapter markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallAdapterReport {
    /// Resolved file actually edited. Symlink targets are canonicalized.
    pub target: PathBuf,
    /// Whether the instruction file changed.
    pub written: bool,
    /// Rolling backup path written before the change.
    pub backup_path: Option<PathBuf>,
    /// Backup metadata path written before the change.
    pub metadata_path: Option<PathBuf>,
}

/// Input for checking whether an adapter is visible to its agent.
#[derive(Debug, Clone)]
pub struct InspectAdapterInstallInput<'a> {
    /// Adapter id, such as `codex` or `claude`.
    pub adapter: &'a str,
    /// Generated adapter output that the marker should include.
    pub output: &'a Path,
    /// Adapter instruction file that the agent loads.
    pub install_target: &'a Path,
}

/// Non-mutating adapter visibility report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInstallInspection {
    /// Resolved file that would be edited by install.
    pub target: PathBuf,
    /// Whether the instruction file exists.
    pub target_exists: bool,
    /// Whether the adapter marker block is present.
    pub installed: bool,
    /// Include body found inside the adapter marker block.
    pub include: Option<String>,
    /// Whether the include body points at the configured output path.
    pub include_matches: bool,
}

/// Non-mutating generated output report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderFileInspection {
    /// Render output path.
    pub output: PathBuf,
    /// Whether the output file exists.
    pub exists: bool,
    /// Whether the file has a valid generated marker and matching checksum.
    pub valid: bool,
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
    /// Adapter install target is a symlink that does not resolve.
    BrokenSymlink {
        /// Symlink path configured as install target.
        path: PathBuf,
        /// Original error rendered for diagnostics.
        message: String,
    },
    /// Existing instruction file is not user-writable.
    NonWritableTarget {
        /// Refused path.
        path: PathBuf,
    },
    /// Existing instruction file contains partial or mismatched markers.
    ConflictingMarkers {
        /// Refused path.
        path: PathBuf,
        /// Marker name that could not be parsed safely.
        marker: String,
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
            Self::BrokenSymlink { path, message } => {
                write!(
                    f,
                    "install target {} is a broken symlink: {message}",
                    path.display()
                )
            }
            Self::NonWritableTarget { path } => {
                write!(f, "install target {} is not user-writable", path.display())
            }
            Self::ConflictingMarkers { path, marker } => write!(
                f,
                "install target {} contains conflicting hive-memory marker {marker}",
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

/// Install or refresh one adapter include marker in an agent instruction file.
///
/// Install targets may be regular files or symlinks. Symlinks are resolved
/// before editing so shared `CLAUDE.md`/`AGENTS.md` setups are idempotent:
/// installing both adapters touches the shared file once, while regular files
/// remain independent. The generated output is referenced by a native include
/// line, and the canonical memory body is never copied into the instruction
/// file itself.
pub fn install_adapter(
    input: InstallAdapterInput<'_>,
) -> Result<InstallAdapterReport, RenderError> {
    let target = resolve_install_target(input.install_target)?;
    ensure_writable(&target)?;

    let existing = read_optional(&target)?;
    let eol = line_ending(&existing);
    let normalized = normalize_lf(&existing);
    let policy_block = marker_block("policy", input.policy_body, "\n");
    let include = format!("@{}", input.output.display());
    let adapter_block = marker_block(input.adapter, &include, "\n");
    let with_policy = upsert_marker(&target, &normalized, "policy", &policy_block)?;
    let installed = upsert_marker(&target, &with_policy, input.adapter, &adapter_block)?;
    let installed = denormalize_eol(&installed, eol);

    if installed == existing {
        return Ok(InstallAdapterReport {
            target,
            written: false,
            backup_path: None,
            metadata_path: None,
        });
    }

    let backup_path = backup_install_path(&target);
    let metadata_path = backup_metadata_path(&target);
    write::write_atomic(&backup_path, existing.as_bytes(), &input.options).map_err(|err| {
        RenderError::Io {
            action: "write install backup",
            path: backup_path.clone(),
            message: err.to_string(),
        }
    })?;
    let metadata = render_install_metadata(&existing, &installed, &["policy", input.adapter]);
    write::write_atomic(&metadata_path, metadata.as_bytes(), &input.options).map_err(|err| {
        RenderError::Io {
            action: "write install backup metadata",
            path: metadata_path.clone(),
            message: err.to_string(),
        }
    })?;
    write::write_atomic(&target, installed.as_bytes(), &input.options).map_err(|err| {
        RenderError::Io {
            action: "write install target",
            path: target.clone(),
            message: err.to_string(),
        }
    })?;

    Ok(InstallAdapterReport {
        target,
        written: true,
        backup_path: Some(backup_path),
        metadata_path: Some(metadata_path),
    })
}

/// Remove adapter include markers from an agent instruction file.
///
/// By default this removes only the selected adapter block. The shared policy
/// block remains because another adapter may still rely on it, and because the
/// policy is stable instructional text rather than generated memory. Callers
/// must opt into `all` when they want to remove the policy block too.
pub fn uninstall_adapter(
    input: UninstallAdapterInput<'_>,
) -> Result<UninstallAdapterReport, RenderError> {
    let target = resolve_install_target(input.install_target)?;
    ensure_writable(&target)?;

    let existing = read_optional(&target)?;
    let eol = line_ending(&existing);
    let normalized = normalize_lf(&existing);
    let without_adapter = remove_marker(&target, &normalized, input.adapter)?;
    let uninstalled = if input.all {
        remove_marker(&target, &without_adapter, "policy")?
    } else {
        without_adapter
    };
    let uninstalled = denormalize_eol(&uninstalled, eol);

    if uninstalled == existing {
        return Ok(UninstallAdapterReport {
            target,
            written: false,
            backup_path: None,
            metadata_path: None,
        });
    }

    let backup_path = backup_install_path(&target);
    let metadata_path = backup_metadata_path(&target);
    write::write_atomic(&backup_path, existing.as_bytes(), &input.options).map_err(|err| {
        RenderError::Io {
            action: "write uninstall backup",
            path: backup_path.clone(),
            message: err.to_string(),
        }
    })?;
    let markers = if input.all {
        vec![input.adapter, "policy"]
    } else {
        vec![input.adapter]
    };
    let metadata = render_install_metadata(&existing, &uninstalled, &markers);
    write::write_atomic(&metadata_path, metadata.as_bytes(), &input.options).map_err(|err| {
        RenderError::Io {
            action: "write uninstall backup metadata",
            path: metadata_path.clone(),
            message: err.to_string(),
        }
    })?;
    write::write_atomic(&target, uninstalled.as_bytes(), &input.options).map_err(|err| {
        RenderError::Io {
            action: "write uninstall target",
            path: target.clone(),
            message: err.to_string(),
        }
    })?;

    Ok(UninstallAdapterReport {
        target,
        written: true,
        backup_path: Some(backup_path),
        metadata_path: Some(metadata_path),
    })
}

/// Inspect a generated adapter output without modifying it.
///
/// Doctor uses this instead of open-coding marker parsing so render, install,
/// and diagnostics all agree on the generated-file checksum contract.
pub fn inspect_rendered_file(output: &Path) -> Result<RenderFileInspection, RenderError> {
    match fs::read_to_string(output) {
        Ok(contents) => {
            validate_existing(output, &contents, false)?;
            Ok(RenderFileInspection {
                output: output.to_path_buf(),
                exists: true,
                valid: true,
            })
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(RenderFileInspection {
            output: output.to_path_buf(),
            exists: false,
            valid: false,
        }),
        Err(err) => Err(io_error("read render output", output, err)),
    }
}

/// Inspect whether an adapter include marker is installed and current.
///
/// This is the read-only counterpart to [`install_adapter`]. Dotfiles update can
/// run `hm doctor --quick` after install and get the same symlink resolution and
/// marker interpretation used by the mutating install path.
pub fn inspect_adapter_install(
    input: InspectAdapterInstallInput<'_>,
) -> Result<AdapterInstallInspection, RenderError> {
    let target = resolve_install_target(input.install_target)?;
    let contents = match fs::read_to_string(&target) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AdapterInstallInspection {
                target,
                target_exists: false,
                installed: false,
                include: None,
                include_matches: false,
            });
        }
        Err(err) => return Err(io_error("read install target", &target, err)),
    };
    let normalized = normalize_lf(&contents);
    let include = marker_body(&target, &normalized, input.adapter)?;
    let expected = format!("@{}", input.output.display());
    let include_matches = include.as_deref() == Some(expected.as_str());
    Ok(AdapterInstallInspection {
        target,
        target_exists: true,
        installed: include.is_some(),
        include,
        include_matches,
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

fn resolve_install_target(path: &Path) -> Result<PathBuf, RenderError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            // Agent instruction files are commonly symlinked between tools
            // (for example CLAUDE.md and AGENTS.md). Editing the target makes
            // install idempotent regardless of which adapter is installed first.
            fs::canonicalize(path).map_err(|err| RenderError::BrokenSymlink {
                path: path.to_path_buf(),
                message: err.to_string(),
            })
        }
        Ok(_metadata) => Ok(path.to_path_buf()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(err) => Err(io_error("read install target metadata", path, err)),
    }
}

fn ensure_writable(path: &Path) -> Result<(), RenderError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.permissions().readonly() => Err(RenderError::NonWritableTarget {
            path: path.to_path_buf(),
        }),
        Ok(_metadata) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(io_error("read install target metadata", path, err)),
    }
}

fn read_optional(path: &Path) -> Result<String, RenderError> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(io_error("read install target", path, err)),
    }
}

fn line_ending(contents: &str) -> &'static str {
    // Preserve an existing file's dominant line ending. Agent instruction files
    // are user-owned, and install should avoid cosmetic churn outside managed
    // marker blocks.
    if contents.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn normalize_lf(contents: &str) -> String {
    contents.replace("\r\n", "\n")
}

fn denormalize_eol(contents: &str, eol: &str) -> String {
    if eol == "\r\n" {
        contents.replace('\n', "\r\n")
    } else {
        contents.to_owned()
    }
}

fn marker_block(name: &str, body: &str, eol: &str) -> String {
    let body = body.trim_matches('\n');
    format!("# BEGIN hive-memory:{name}{eol}{body}{eol}# END hive-memory:{name}{eol}")
}

/// Insert or replace one managed marker block.
///
/// The parser requires exact begin/end marker lines and at most one block per
/// marker name. That strictness is intentional: a partial marker usually means
/// a human edit or merge conflict, and guessing would risk deleting ordinary
/// instructions from an agent-owned file.
fn upsert_marker(
    path: &Path,
    contents: &str,
    name: &str,
    block: &str,
) -> Result<String, RenderError> {
    let lines = contents.lines().collect::<Vec<_>>();
    let Some((begin_index, end_index)) = marker_span(path, &lines, name)? else {
        let mut output = contents.trim_end_matches('\n').to_owned();
        if !output.is_empty() {
            output.push_str("\n\n");
        }
        output.push_str(block);
        return Ok(output);
    };

    let mut output = String::new();
    for line in &lines[..begin_index] {
        output.push_str(line);
        output.push('\n');
    }
    output.push_str(block);
    for line in &lines[end_index + 1..] {
        output.push_str(line);
        output.push('\n');
    }
    Ok(output)
}

/// Remove one managed marker block when it is present.
///
/// Absence is success so uninstall stays idempotent. Malformed or duplicate
/// markers are errors for the same reason as install: the tool should stop and
/// let the user inspect ambiguous instruction-file edits.
fn remove_marker(path: &Path, contents: &str, name: &str) -> Result<String, RenderError> {
    let lines = contents.lines().collect::<Vec<_>>();
    let Some((begin_index, end_index)) = marker_span(path, &lines, name)? else {
        return Ok(contents.to_owned());
    };

    let mut output = String::new();
    for line in &lines[..begin_index] {
        output.push_str(line);
        output.push('\n');
    }
    for line in &lines[end_index + 1..] {
        output.push_str(line);
        output.push('\n');
    }
    Ok(collapse_blank_lines(output.trim_end_matches('\n')))
}

fn marker_body(path: &Path, contents: &str, name: &str) -> Result<Option<String>, RenderError> {
    let lines = contents.lines().collect::<Vec<_>>();
    let Some((begin_index, end_index)) = marker_span(path, &lines, name)? else {
        return Ok(None);
    };
    Ok(Some(lines[begin_index + 1..end_index].join("\n")))
}

fn marker_span(
    path: &Path,
    lines: &[&str],
    name: &str,
) -> Result<Option<(usize, usize)>, RenderError> {
    let begin = format!("# BEGIN hive-memory:{name}");
    let end = format!("# END hive-memory:{name}");
    let begin_positions = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (*line == begin).then_some(index))
        .collect::<Vec<_>>();
    let end_positions = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (*line == end).then_some(index))
        .collect::<Vec<_>>();

    if begin_positions.len() != end_positions.len() || begin_positions.len() > 1 {
        return Err(RenderError::ConflictingMarkers {
            path: path.to_path_buf(),
            marker: name.to_owned(),
        });
    }
    if begin_positions.is_empty() {
        return Ok(None);
    }

    let begin_index = begin_positions[0];
    let end_index = end_positions[0];
    if end_index <= begin_index {
        return Err(RenderError::ConflictingMarkers {
            path: path.to_path_buf(),
            marker: name.to_owned(),
        });
    }

    Ok(Some((begin_index, end_index)))
}

fn collapse_blank_lines(contents: &str) -> String {
    let mut output = String::new();
    let mut previous_blank = false;
    for line in contents.lines() {
        let blank = line.is_empty();
        if blank && previous_blank {
            continue;
        }
        output.push_str(line);
        output.push('\n');
        previous_blank = blank;
    }
    output
}

fn backup_install_path(target: &Path) -> PathBuf {
    target.with_extension(format!(
        "{}hive-memory.bak",
        target
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| format!("{extension}."))
            .unwrap_or_default()
    ))
}

fn backup_metadata_path(target: &Path) -> PathBuf {
    target.with_extension(format!(
        "{}hive-memory.bak.toml",
        target
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| format!("{extension}."))
            .unwrap_or_default()
    ))
}

fn render_install_metadata(original: &str, installed: &str, markers: &[&str]) -> String {
    let markers = markers
        .iter()
        .map(|marker| format!("\"{marker}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "backup_sha256 = \"{}\"\ninstalled_sha256 = \"{}\"\nmarkers = [{}]\n",
        body_sha256(original),
        body_sha256(installed),
        markers
    )
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

    #[test]
    fn install_adapter_adds_policy_and_include_markers() {
        let dir = temp_dir("install");
        let target = dir.join("AGENTS.md");
        let output = dir.join("codex.generated.md");
        fs::write(&target, "# Existing\n").expect("instruction file");

        let report = install_adapter(InstallAdapterInput {
            adapter: "codex",
            output: &output,
            install_target: &target,
            policy_body: "Use Hive Memory as contextual data.",
            options: options(),
        })
        .expect("install adapter");
        let installed = fs::read_to_string(&target).expect("read install target");

        assert!(report.written);
        assert!(report.backup_path.expect("backup").is_file());
        assert!(report.metadata_path.expect("metadata").is_file());
        assert!(installed.contains("# BEGIN hive-memory:policy"));
        assert!(installed.contains("Use Hive Memory as contextual data."));
        assert!(installed.contains("# BEGIN hive-memory:codex"));
        assert!(installed.contains(&format!("@{}", output.display())));
    }

    #[test]
    fn install_adapter_is_idempotent() {
        let dir = temp_dir("install-idempotent");
        let target = dir.join("AGENTS.md");
        let output = dir.join("codex.generated.md");

        install_adapter(InstallAdapterInput {
            adapter: "codex",
            output: &output,
            install_target: &target,
            policy_body: "Policy.",
            options: options(),
        })
        .expect("first install");
        let second = install_adapter(InstallAdapterInput {
            adapter: "codex",
            output: &output,
            install_target: &target,
            policy_body: "Policy.",
            options: options(),
        })
        .expect("second install");

        assert!(!second.written);
        assert!(second.backup_path.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn install_adapter_resolves_symlinked_instruction_file() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("install-symlink");
        let shared = dir.join("CLAUDE.md");
        let link = dir.join("AGENTS.md");
        let output = dir.join("codex.generated.md");
        fs::write(&shared, "# Shared\n").expect("shared file");
        symlink(&shared, &link).expect("symlink");

        let report = install_adapter(InstallAdapterInput {
            adapter: "codex",
            output: &output,
            install_target: &link,
            policy_body: "Policy.",
            options: options(),
        })
        .expect("install through symlink");

        assert_eq!(report.target, fs::canonicalize(&shared).expect("canonical"));
        assert!(fs::read_to_string(shared).expect("shared").contains("@"));
    }

    #[cfg(unix)]
    #[test]
    fn install_adapter_refuses_broken_symlink() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("install-broken-symlink");
        let link = dir.join("AGENTS.md");
        let output = dir.join("codex.generated.md");
        symlink(dir.join("missing.md"), &link).expect("broken symlink");

        let error = install_adapter(InstallAdapterInput {
            adapter: "codex",
            output: &output,
            install_target: &link,
            policy_body: "Policy.",
            options: options(),
        })
        .expect_err("broken symlink");

        assert!(matches!(error, RenderError::BrokenSymlink { .. }));
    }

    #[test]
    fn install_adapter_refuses_conflicting_marker() {
        let dir = temp_dir("install-conflict");
        let target = dir.join("AGENTS.md");
        let output = dir.join("codex.generated.md");
        fs::write(&target, "# BEGIN hive-memory:codex\nmissing end\n").expect("conflict");

        let error = install_adapter(InstallAdapterInput {
            adapter: "codex",
            output: &output,
            install_target: &target,
            policy_body: "Policy.",
            options: options(),
        })
        .expect_err("conflicting marker");

        assert!(matches!(error, RenderError::ConflictingMarkers { .. }));
    }

    #[test]
    fn uninstall_adapter_removes_include_and_keeps_policy_by_default() {
        let dir = temp_dir("uninstall");
        let target = dir.join("AGENTS.md");
        let output = dir.join("codex.generated.md");
        install_adapter(InstallAdapterInput {
            adapter: "codex",
            output: &output,
            install_target: &target,
            policy_body: "Policy.",
            options: options(),
        })
        .expect("install adapter");

        let report = uninstall_adapter(UninstallAdapterInput {
            adapter: "codex",
            install_target: &target,
            all: false,
            options: options(),
        })
        .expect("uninstall adapter");
        let contents = fs::read_to_string(&target).expect("read target");

        assert!(report.written);
        assert!(report.backup_path.expect("backup").is_file());
        assert!(contents.contains("# BEGIN hive-memory:policy"));
        assert!(!contents.contains("# BEGIN hive-memory:codex"));
    }

    #[test]
    fn uninstall_adapter_all_removes_policy_block() {
        let dir = temp_dir("uninstall-all");
        let target = dir.join("AGENTS.md");
        let output = dir.join("codex.generated.md");
        install_adapter(InstallAdapterInput {
            adapter: "codex",
            output: &output,
            install_target: &target,
            policy_body: "Policy.",
            options: options(),
        })
        .expect("install adapter");

        uninstall_adapter(UninstallAdapterInput {
            adapter: "codex",
            install_target: &target,
            all: true,
            options: options(),
        })
        .expect("uninstall adapter");
        let contents = fs::read_to_string(&target).expect("read target");

        assert!(!contents.contains("# BEGIN hive-memory:policy"));
        assert!(!contents.contains("# BEGIN hive-memory:codex"));
    }

    #[test]
    fn uninstall_adapter_missing_marker_is_noop() {
        let dir = temp_dir("uninstall-noop");
        let target = dir.join("AGENTS.md");
        fs::write(&target, "# Existing\n").expect("instruction file");

        let report = uninstall_adapter(UninstallAdapterInput {
            adapter: "codex",
            install_target: &target,
            all: false,
            options: options(),
        })
        .expect("uninstall adapter");

        assert!(!report.written);
        assert!(report.backup_path.is_none());
    }
}
