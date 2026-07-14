//! Curated-memory file discovery.
//!
//! Curated Markdown is intentionally outside the raw inbox index: humans may
//! edit these files directly, and they should stay readable without rebuilding
//! cache state. This module owns the shared filesystem walk so context and
//! search agree on which curated files are eligible and avoid following
//! symlinks outside the store.

use crate::project;
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
    collect_global(store_root, &mut files)?;
    if let Some(project_id) = project_id {
        for id in project::related_project_ids(store_root, project_id).map_err(alias_error)? {
            collect_project(store_root, &id, &mut files)?;
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
    let mut files = Vec::new();
    collect_global(store_root, &mut files)?;
    let projects_root = store_root.join("memories/projects");
    let entries = match fs::read_dir(&projects_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
            return Ok(files);
        }
        Err(err) => return Err(read_error(projects_root, err)),
    };
    for entry in entries {
        let entry = entry.map_err(|err| read_error(projects_root.clone(), err))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| read_error(path.clone(), err))?;
        if !file_type.is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // Broad recall should survive one corrupt or mid-sync unrelated
        // project. Collect into a temporary vector so a failed project is
        // skipped atomically instead of leaking a partial traversal.
        let mut project_files = Vec::new();
        if collect_project(store_root, &id, &mut project_files).is_ok() {
            files.extend(project_files);
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

fn collect_global(store_root: &Path, files: &mut Vec<CuratedFile>) -> Result<(), CuratedError> {
    collect_tree(store_root, Path::new("rules"), "global", None, 0, files)?;
    collect_tree(store_root, Path::new("people"), "global", None, 0, files)?;
    collect_tree(
        store_root,
        Path::new("memories/global"),
        "global",
        None,
        0,
        files,
    )
}

fn collect_project(
    store_root: &Path,
    project_id: &str,
    files: &mut Vec<CuratedFile>,
) -> Result<(), CuratedError> {
    // This join is a filesystem sink for ids from both CLI input and synced
    // alias metadata. Keep path safety local even though project resolution
    // already validates ids at its own boundary.
    if !project::is_safe_project_id(project_id) {
        return Ok(());
    }
    collect_tree(
        store_root,
        &Path::new("memories/projects").join(project_id),
        "project",
        Some(project_id),
        0,
        files,
    )
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
) -> Result<(), CuratedError> {
    let root = store_root.join(relative_root);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(read_error(root, err));
        }
    };

    for entry in entries {
        let entry = entry.map_err(|err| read_error(root.clone(), err))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| read_error(path.clone(), err))?;
        if file_type.is_dir() {
            if depth >= MAX_CURATED_DEPTH {
                continue;
            }
            let relative = path.strip_prefix(store_root).unwrap_or(&path);
            collect_tree(store_root, relative, scope, project_id, depth + 1, files)?;
        } else if file_type.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("md")
        {
            let metadata = entry
                .metadata()
                .map_err(|err| read_error(path.clone(), err))?;
            if metadata.len() > MAX_CURATED_FILE_BYTES {
                continue;
            }
            let body = fs::read_to_string(&path).map_err(|err| read_error(path.clone(), err))?;
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

    Ok(())
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
