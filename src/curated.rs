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

/// One curated Markdown file discovered inside a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuratedFile {
    /// Stable synthetic memory id used in rendered context and search output.
    pub id: String,
    /// Store-relative source path.
    pub relative_path: String,
    /// Scope derived from the curated directory.
    pub scope: String,
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
    collect_tree(store_root, Path::new("rules"), "global", &mut files)?;
    collect_tree(store_root, Path::new("people"), "global", &mut files)?;
    collect_tree(
        store_root,
        Path::new("memories/global"),
        "global",
        &mut files,
    )?;
    if let Some(project_id) = project_id {
        for id in project::related_project_ids(store_root, project_id).map_err(alias_error)? {
            // Defense in depth: `related_project_ids` already sanitizes ids at
            // the identity boundary, but this join is the actual filesystem
            // sink for an attacker-controlled, synced `aliases.toml`. Re-check
            // here so any future caller path that reaches this join cannot turn
            // a poisoned id (`..`, absolute, separators) into a store escape
            // that injects arbitrary `.md` at the highest `curated` trust level.
            if !project::is_safe_project_id(&id) {
                continue;
            }
            collect_tree(
                store_root,
                &Path::new("memories/projects").join(id),
                "project",
                &mut files,
            )?;
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
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
            let relative = path.strip_prefix(store_root).unwrap_or(&path);
            collect_tree(store_root, relative, scope, files)?;
        } else if file_type.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("md")
        {
            let body = fs::read_to_string(&path).map_err(|err| read_error(path.clone(), err))?;
            let relative_path = path_string(path.strip_prefix(store_root).unwrap_or(&path));
            files.push(CuratedFile {
                id: format!("curated:{relative_path}"),
                relative_path,
                scope: scope.to_owned(),
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
