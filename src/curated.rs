//! Curated-memory file discovery.
//!
//! Curated Markdown is intentionally outside the raw inbox index: humans may
//! edit these files directly, and they should stay readable without rebuilding
//! cache state. This module owns the shared filesystem walk so context and
//! search agree on which curated files are eligible and avoid following
//! symlinks outside the store.

use crate::{project, write};
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};

const MAX_CURATED_FILE_BYTES: u64 = 1_048_576;
const MAX_CURATED_DEPTH: usize = 16;

/// One curated Markdown file discovered inside a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuratedFile {
    /// Stable synthetic memory id used in rendered context and search output.
    pub id: String,
    /// Store-relative source path.
    pub relative_path: String,
    /// Scope derived from the curated directory.
    pub scope: String,
    /// Owning project identity for project-scoped curated memory.
    pub project_id: Option<String>,
    /// Markdown body read from the curated file.
    pub body: String,
}

/// Curated-file discovery failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CuratedError {
    /// Filesystem operation failed while walking or reading curated files.
    ReadFile {
        /// Path involved in the failure.
        path: PathBuf,
        /// Original error rendered for diagnostics.
        message: String,
    },
    /// Project alias metadata could not be read or parsed.
    ProjectAlias {
        /// Alias file path that failed.
        path: PathBuf,
        /// Original error rendered for diagnostics.
        message: String,
    },
}

impl Display for CuratedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadFile { path, message } => {
                write!(
                    f,
                    "failed to read curated memory {}: {message}",
                    path.display()
                )
            }
            Self::ProjectAlias { path, message } => {
                write!(
                    f,
                    "failed to read project aliases {}: {message}",
                    path.display()
                )
            }
        }
    }
}

impl Error for CuratedError {}

impl CuratedError {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::ReadFile { path, .. } | Self::ProjectAlias { path, .. } => path,
        }
    }
}

/// Best-effort broad collection plus every omission observed during the walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CuratedCollection {
    pub files: Vec<CuratedFile>,
    pub warnings: Vec<CuratedError>,
}

/// Return curated Markdown files eligible for the active project.
///
/// Global curated memory comes from `rules/`, `people/`, and
/// `memories/global/`. Project curated memory is included only when the caller
/// supplies a project id; long-lived agents can move between projects, so CWD
/// guessing belongs in the caller's project-resolution policy, not in this file
/// walker. Directory entries are inspected without following symlinks so a
/// synced store cannot accidentally inject arbitrary outside files into agent
/// context or search results.
pub fn collect(
    store_root: &Path,
    project_id: Option<&str>,
) -> Result<Vec<CuratedFile>, CuratedError> {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    collect_global(store_root, &mut files, &mut warnings);
    if let Some(project_id) = project_id {
        for id in project::related_project_ids(store_root, project_id).map_err(alias_error)? {
            collect_project(store_root, &id, &mut files, &mut warnings);
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

/// Return every curated Markdown file in a store.
///
/// Explicit search is intentionally cross-project. Automatic context continues
/// to call [`collect`] so session-start injection stays bounded to the active
/// project. Project directory names are validated before joining them into the
/// store path, and recursive discovery never follows symlinks.
pub fn collect_all(store_root: &Path) -> Result<Vec<CuratedFile>, CuratedError> {
    Ok(collect_all_report(store_root).files)
}

/// Return every readable curated file and report individual omissions.
pub(crate) fn collect_all_report(store_root: &Path) -> CuratedCollection {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    collect_global(store_root, &mut files, &mut warnings);
    let projects_root = store_root.join("memories/projects");
    let entries = match fs::read_dir(&projects_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
            return CuratedCollection { files, warnings };
        }
        Err(err) => {
            warnings.push(read_error(projects_root, err));
            files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
            return CuratedCollection { files, warnings };
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                warnings.push(read_error(projects_root.clone(), err));
                continue;
            }
        };
        let path = entry.path();
        if write::is_atomic_temp_path(&path) {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                warnings.push(read_error(path, err));
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        collect_project(store_root, &id, &mut files, &mut warnings);
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    CuratedCollection { files, warnings }
}

fn collect_global(
    store_root: &Path,
    files: &mut Vec<CuratedFile>,
    warnings: &mut Vec<CuratedError>,
) {
    collect_tree(
        store_root,
        Path::new("rules"),
        "global",
        None,
        0,
        files,
        warnings,
    );
    collect_tree(
        store_root,
        Path::new("people"),
        "global",
        None,
        0,
        files,
        warnings,
    );
    collect_tree(
        store_root,
        Path::new("memories/global"),
        "global",
        None,
        0,
        files,
        warnings,
    );
}

fn collect_project(
    store_root: &Path,
    project_id: &str,
    files: &mut Vec<CuratedFile>,
    warnings: &mut Vec<CuratedError>,
) {
    // This join is a filesystem sink for ids from both CLI input and synced
    // alias metadata. Keep path safety local even though project resolution
    // already validates ids at its own boundary.
    if !project::is_safe_project_id(project_id) {
        return;
    }
    collect_tree(
        store_root,
        &Path::new("memories/projects").join(project_id),
        "project",
        Some(project_id),
        0,
        files,
        warnings,
    );
}

fn alias_error(err: project::ProjectError) -> CuratedError {
    match err {
        project::ProjectError::Alias { path, message } => {
            CuratedError::ProjectAlias { path, message }
        }
        other => CuratedError::ProjectAlias {
            path: PathBuf::new(),
            message: other.to_string(),
        },
    }
}

fn collect_tree(
    store_root: &Path,
    relative_root: &Path,
    scope: &str,
    project_id: Option<&str>,
    depth: usize,
    files: &mut Vec<CuratedFile>,
    warnings: &mut Vec<CuratedError>,
) {
    let root = store_root.join(relative_root);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            warnings.push(read_error(root, err));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                warnings.push(read_error(root.clone(), err));
                continue;
            }
        };
        let path = entry.path();
        if write::is_atomic_temp_path(&path) {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                warnings.push(read_error(path, err));
                continue;
            }
        };
        if file_type.is_dir() {
            if depth >= MAX_CURATED_DEPTH {
                warnings.push(CuratedError::ReadFile {
                    path,
                    message: format!("curated directory exceeds maximum depth {MAX_CURATED_DEPTH}"),
                });
                continue;
            }
            let relative = path.strip_prefix(store_root).unwrap_or(&path);
            collect_tree(
                store_root,
                relative,
                scope,
                project_id,
                depth + 1,
                files,
                warnings,
            );
        } else if file_type.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("md")
        {
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    warnings.push(read_error(path, err));
                    continue;
                }
            };
            if metadata.len() > MAX_CURATED_FILE_BYTES {
                warnings.push(CuratedError::ReadFile {
                    path,
                    message: format!(
                        "curated file is {} bytes, over the {MAX_CURATED_FILE_BYTES}-byte limit",
                        metadata.len()
                    ),
                });
                continue;
            }
            let body = match fs::read_to_string(&path) {
                Ok(body) => body,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    warnings.push(read_error(path, err));
                    continue;
                }
            };
            let relative_path = path_string(path.strip_prefix(store_root).unwrap_or(&path));
            files.push(CuratedFile {
                id: format!("curated:{relative_path}"),
                relative_path,
                scope: scope.to_owned(),
                project_id: project_id.map(str::to_owned),
                body,
            });
        }
    }
}

fn path_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn read_error(path: PathBuf, err: std::io::Error) -> CuratedError {
    CuratedError::ReadFile {
        path,
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_all_retains_good_memory_beside_unreadable_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let project = dir.path().join("memories/projects/example");
        fs::create_dir_all(&project).expect("project parent");
        fs::write(project.join("good.md"), "Good project memory.\n").expect("good memory");
        fs::write(project.join("broken.md"), [0xff, 0xfe]).expect("invalid utf8 curated file");

        let report = collect_all_report(dir.path());
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].body, "Good project memory.\n");
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].to_string().contains("broken.md"));
    }

    #[test]
    fn collect_all_retains_good_memory_beside_oversized_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let rules = dir.path().join("rules");
        fs::create_dir_all(&rules).expect("rules parent");
        fs::write(rules.join("good.md"), "Good global memory.\n").expect("good memory");
        let oversized = vec![b'x'; usize::try_from(MAX_CURATED_FILE_BYTES + 1).expect("size")];
        fs::write(rules.join("oversized.md"), oversized).expect("oversized curated file");

        let report = collect_all_report(dir.path());
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].body, "Good global memory.\n");
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].to_string().contains("oversized.md"));
    }
}
