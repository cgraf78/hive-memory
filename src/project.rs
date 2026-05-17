//! Project identity resolution.
//!
//! Agent sessions are often launched from subdirectories, editor buffers, or
//! tool-specific working directories. Project identity therefore cannot be
//! process CWD by default. This module turns an explicit path hint into a stable
//! project id, falling back to CWD only when callers provide no better signal.

use crate::{id, write};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Input for resolving one project identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveProjectInput {
    /// Path, file, or directory hint supplied by CLI/env/hook code.
    pub hint: PathBuf,
    /// Explicit project id from CLI.
    pub explicit_project_id: Option<String>,
    /// Project id from `HIVE_MEMORY_PROJECT_ID`.
    pub env_project_id: Option<String>,
}

/// Resolved project identity and root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectResolution {
    /// Stable project id used in memory metadata.
    pub project_id: String,
    /// Root directory used for project-relative decisions.
    pub project_root: PathBuf,
    /// Original path hint after absolutizing/canonicalizing enough for display.
    pub project_hint: PathBuf,
    /// Where the project id came from.
    pub source: ProjectIdSource,
}

/// Source of a resolved project id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectIdSource {
    /// Caller supplied the project id directly.
    Explicit,
    /// Environment supplied `HIVE_MEMORY_PROJECT_ID`.
    Env,
    /// A `.hive-memory/project.toml` marker supplied the id.
    Marker,
    /// Git remote identity was normalized into a project id.
    GitRemote,
    /// Filesystem path fallback was hashed into a stable local project id.
    Path,
}

impl Display for ProjectIdSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Explicit => "explicit",
            Self::Env => "env",
            Self::Marker => "marker",
            Self::GitRemote => "git-remote",
            Self::Path => "path",
        };
        f.write_str(value)
    }
}

/// Project resolution failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectError {
    /// Current directory could not be read when no hint was supplied.
    CurrentDir(String),
    /// Marker file existed but was unreadable or malformed.
    Marker {
        /// Marker file path.
        path: PathBuf,
        /// Original error rendered for diagnostics.
        message: String,
    },
    /// Local project binding could not be read, written, or parsed.
    Binding {
        /// Binding file path.
        path: PathBuf,
        /// Original error rendered for diagnostics.
        message: String,
    },
    /// Project alias file could not be read, written, or parsed.
    Alias {
        /// Alias file path.
        path: PathBuf,
        /// Original error rendered for diagnostics.
        message: String,
    },
}

impl Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(message) => {
                write!(f, "failed to resolve current directory: {message}")
            }
            Self::Marker { path, message } => {
                write!(
                    f,
                    "failed to read project marker {}: {message}",
                    path.display()
                )
            }
            Self::Binding { path, message } => {
                write!(
                    f,
                    "failed to access project binding {}: {message}",
                    path.display()
                )
            }
            Self::Alias { path, message } => {
                write!(
                    f,
                    "failed to access project aliases {}: {message}",
                    path.display()
                )
            }
        }
    }
}

impl Error for ProjectError {}

/// Local machine policy binding from a project id to a preferred store.
///
/// Bindings live under `data_dir`, not inside a memory store or project repo,
/// because they represent this machine's private store affinity decision. This
/// keeps work/personal routing out of shared repositories and out of the
/// canonical memory stores that other machines may sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectBinding {
    /// Project id this binding applies to.
    pub project_id: String,
    /// Configured store alias preferred for this project.
    pub store: String,
}

/// Canonical project alias file stored with curated project memory.
///
/// The file lives at `memories/projects/<canonical-id>/aliases.toml` inside a
/// memory store. Aliases are durable shared memory metadata, unlike local store
/// bindings under `data_dir`, because every machine needs the same old-id to
/// new-id mapping after a repo rename or remote URL migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectAliases {
    /// Alias schema version.
    pub schema_version: u32,
    /// Canonical/current project id represented by this directory.
    pub project_id: String,
    /// Prior project ids that should resolve to `project_id`.
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Resolve a stable project id from a path hint and optional explicit IDs.
///
/// Identity precedence follows the v1 spec: CLI id, env id, marker file,
/// normalized git origin URL, then local path. Root discovery still uses the
/// path hint so explicit IDs can be attached to the correct checkout without
/// trusting process CWD.
pub fn resolve_project(input: ResolveProjectInput) -> Result<ProjectResolution, ProjectError> {
    let project_hint = absolutize_hint(&input.hint)?;
    let start_dir = starting_dir(&project_hint);
    let marker = find_marker(&start_dir)?;
    let git_root = find_git_root(&start_dir);
    let project_root = marker
        .as_ref()
        .map(|marker| marker.root.clone())
        .or_else(|| git_root.clone())
        .unwrap_or_else(|| start_dir.clone());

    if let Some(project_id) = input.explicit_project_id.filter(|value| !value.is_empty()) {
        return Ok(ProjectResolution {
            project_id,
            project_root,
            project_hint,
            source: ProjectIdSource::Explicit,
        });
    }
    if let Some(project_id) = input.env_project_id.filter(|value| !value.is_empty()) {
        return Ok(ProjectResolution {
            project_id,
            project_root,
            project_hint,
            source: ProjectIdSource::Env,
        });
    }
    if let Some(marker) = marker {
        return Ok(ProjectResolution {
            project_id: marker.project_id,
            project_root,
            project_hint,
            source: ProjectIdSource::Marker,
        });
    }
    if let Some(root) = git_root
        && let Some(remote) = git_origin_url(&root)
    {
        let normalized = normalize_remote_url(&remote);
        return Ok(ProjectResolution {
            project_id: derived_id(&normalized),
            project_root: root,
            project_hint,
            source: ProjectIdSource::GitRemote,
        });
    }

    let path_key = project_root.to_string_lossy();
    Ok(ProjectResolution {
        project_id: path_id(&project_root, &path_key),
        project_root,
        project_hint,
        source: ProjectIdSource::Path,
    })
}

/// Normalize a git origin URL so protocol/auth changes preserve project id.
pub fn normalize_remote_url(input: &str) -> String {
    let mut value = input.trim().trim_end_matches('/').to_owned();
    for prefix in ["ssh://", "https://", "http://", "git://"] {
        if let Some(stripped) = value.strip_prefix(prefix) {
            value = stripped.to_owned();
            break;
        }
    }
    if let Some((_, rest)) = value.split_once('@') {
        value = rest.to_owned();
    }
    if let Some(stripped) = value.strip_suffix(".git") {
        value = stripped.to_owned();
    }

    let slash_index = value.find('/');
    let colon_index = value.find(':');
    let scp_like = match (colon_index, slash_index) {
        (Some(_), None) => true,
        (Some(colon), Some(slash)) => colon < slash,
        _ => false,
    };
    let (host, rest) = if scp_like {
        let (host, rest) = value.split_once(':').expect("colon index exists");
        (
            host.to_ascii_lowercase(),
            rest.trim_start_matches('/').to_owned(),
        )
    } else if let Some((host, rest)) = value.split_once('/') {
        (
            host.to_ascii_lowercase(),
            rest.trim_start_matches('/').to_owned(),
        )
    } else {
        (value.to_ascii_lowercase(), String::new())
    };

    if rest.is_empty() {
        host
    } else {
        format!("{host}/{}", rest.trim_end_matches('/'))
    }
}

/// Return the local binding file for a project id.
///
/// Project IDs can come from marker files, remotes, or paths. Sanitizing here
/// makes the on-disk contract safe no matter which source produced the id.
pub fn binding_path(data_dir: &Path, project_id: &str) -> PathBuf {
    data_dir
        .join("projects")
        .join(format!("{}.toml", id::sanitize_component(project_id)))
}

/// Load a local project binding, returning `None` when no binding exists.
///
/// Missing means the project has no local affinity and should continue through
/// normal agent/default store resolution; parse and I/O errors are surfaced
/// because a corrupt binding would otherwise silently route memory elsewhere.
pub fn load_binding(
    data_dir: &Path,
    project_id: &str,
) -> Result<Option<ProjectBinding>, ProjectError> {
    let path = binding_path(data_dir, project_id);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(ProjectError::Binding {
                path,
                message: err.to_string(),
            });
        }
    };
    let binding =
        toml::from_str::<ProjectBinding>(&contents).map_err(|err| ProjectError::Binding {
            path: path.clone(),
            message: err.to_string(),
        })?;
    Ok(Some(binding))
}

/// Write or replace a local project store binding.
///
/// Bindings are small, but still policy-bearing. Use the shared atomic writer so
/// hooks and long-lived agents never observe a partial TOML file.
pub fn save_binding(
    data_dir: &Path,
    binding: &ProjectBinding,
    options: &write::AtomicWriteOptions,
) -> Result<PathBuf, ProjectError> {
    let path = binding_path(data_dir, &binding.project_id);
    let contents = toml::to_string_pretty(binding).map_err(|err| ProjectError::Binding {
        path: path.clone(),
        message: err.to_string(),
    })?;
    write::write_atomic(&path, contents.as_bytes(), options).map_err(|err| {
        ProjectError::Binding {
            path: path.clone(),
            message: err.to_string(),
        }
    })?;
    Ok(path)
}

/// Remove a local project binding. Missing bindings are already unbound.
pub fn remove_binding(data_dir: &Path, project_id: &str) -> Result<Option<PathBuf>, ProjectError> {
    let path = binding_path(data_dir, project_id);
    match fs::remove_file(&path) {
        Ok(()) => Ok(Some(path)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ProjectError::Binding {
            path,
            message: err.to_string(),
        }),
    }
}

fn absolutize_hint(hint: &Path) -> Result<PathBuf, ProjectError> {
    let path = if hint.as_os_str().is_empty() {
        std::env::current_dir().map_err(|err| ProjectError::CurrentDir(err.to_string()))?
    } else if hint.is_absolute() {
        hint.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| ProjectError::CurrentDir(err.to_string()))?
            .join(hint)
    };

    Ok(fs::canonicalize(&path).unwrap_or(path))
}

fn starting_dir(path: &Path) -> PathBuf {
    if path.is_file() {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    }
}

/// Return the store-relative alias file path for one canonical project id.
pub fn aliases_relative_path(project_id: &str) -> PathBuf {
    Path::new("memories/projects")
        .join(id::sanitize_component(project_id))
        .join("aliases.toml")
}

/// Return the absolute alias file path for one canonical project id.
pub fn aliases_path(store_root: &Path, project_id: &str) -> PathBuf {
    store_root.join(aliases_relative_path(project_id))
}

/// Load a canonical project's alias file.
///
/// Missing means the canonical id has no declared historical ids. Malformed
/// alias files are surfaced because silently ignoring them would make old
/// project memory disappear from context/search after a move.
pub fn load_aliases(
    store_root: &Path,
    project_id: &str,
) -> Result<Option<ProjectAliases>, ProjectError> {
    let path = aliases_path(store_root, project_id);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(ProjectError::Alias {
                path,
                message: err.to_string(),
            });
        }
    };
    let aliases =
        toml::from_str::<ProjectAliases>(&contents).map_err(|err| ProjectError::Alias {
            path: path.clone(),
            message: err.to_string(),
        })?;
    Ok(Some(aliases))
}

/// Add one prior project id to a canonical project's alias file.
///
/// The operation is idempotent and keeps aliases sorted so repeated `hm
/// projects alias` runs produce stable diffs. It does not move curated files;
/// users can keep memory under the old id, the new id, or both while
/// search/context follow the alias relationship.
pub fn add_alias(
    store_root: &Path,
    old_project_id: &str,
    new_project_id: &str,
    options: &write::AtomicWriteOptions,
) -> Result<PathBuf, ProjectError> {
    let path = aliases_path(store_root, new_project_id);
    let mut aliases = load_aliases(store_root, new_project_id)?.unwrap_or(ProjectAliases {
        schema_version: 1,
        project_id: new_project_id.to_owned(),
        aliases: Vec::new(),
    });
    aliases.project_id = new_project_id.to_owned();
    if !aliases.aliases.iter().any(|alias| alias == old_project_id) {
        aliases.aliases.push(old_project_id.to_owned());
    }
    aliases.aliases.sort();
    aliases.aliases.dedup();
    let contents = toml::to_string_pretty(&aliases).map_err(|err| ProjectError::Alias {
        path: path.clone(),
        message: err.to_string(),
    })?;
    write::write_atomic(&path, contents.as_bytes(), options).map_err(|err| {
        ProjectError::Alias {
            path: path.clone(),
            message: err.to_string(),
        }
    })?;
    Ok(path)
}

/// Return every project id claimed by curated project metadata in one store.
///
/// A project directory claims its directory name even before an alias file
/// exists. When `aliases.toml` exists, it also claims the canonical id inside
/// the file plus every historical id listed there. Doctor uses this as the
/// authoritative set for deciding whether project-scoped inbox notes are
/// attached to a durable project identity or were produced from stale hints.
pub fn claimed_project_ids(store_root: &Path) -> Result<BTreeSet<String>, ProjectError> {
    let mut ids = BTreeSet::new();
    for directory_id in project_directories(store_root)? {
        ids.insert(directory_id.clone());
        if let Some(aliases) = load_aliases(store_root, &directory_id)? {
            ids.insert(aliases.project_id);
            ids.extend(aliases.aliases);
        }
    }
    Ok(ids)
}

/// Return all project ids related to an active project id by aliases.
///
/// Alias files model identity continuity, not a one-way redirect. Context and
/// search therefore include both old and current ids when either side is
/// active, so long-lived project memory survives repository renames while
/// humans decide whether to move files or rewrite old note metadata.
pub fn related_project_ids(
    store_root: &Path,
    project_id: &str,
) -> Result<BTreeSet<String>, ProjectError> {
    let mut ids = BTreeSet::from([project_id.to_owned()]);
    for directory_id in project_directories(store_root)? {
        let Some(aliases) = load_aliases(store_root, &directory_id)? else {
            continue;
        };
        if aliases.project_id == project_id
            || directory_id == project_id
            || aliases.aliases.iter().any(|alias| alias == project_id)
        {
            ids.insert(directory_id);
            ids.insert(aliases.project_id);
            ids.extend(aliases.aliases);
        }
    }
    Ok(ids)
}

fn project_directories(store_root: &Path) -> Result<Vec<String>, ProjectError> {
    let root = store_root.join("memories/projects");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(ProjectError::Alias {
                path: root,
                message: err.to_string(),
            });
        }
    };

    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| ProjectError::Alias {
            path: root.clone(),
            message: err.to_string(),
        })?;
        if !entry
            .file_type()
            .map_err(|err| ProjectError::Alias {
                path: entry.path(),
                message: err.to_string(),
            })?
            .is_dir()
        {
            continue;
        }
        if let Some(id) = entry.file_name().to_str() {
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}

struct Marker {
    root: PathBuf,
    project_id: String,
}

fn find_marker(start: &Path) -> Result<Option<Marker>, ProjectError> {
    for dir in start.ancestors() {
        let path = dir.join(".hive-memory-project");
        if path.is_file() {
            let contents = fs::read_to_string(&path).map_err(|err| ProjectError::Marker {
                path: path.clone(),
                message: err.to_string(),
            })?;
            let value = contents
                .parse::<toml::Table>()
                .map_err(|err| ProjectError::Marker {
                    path: path.clone(),
                    message: err.to_string(),
                })?;
            let project_id = value
                .get("id")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| ProjectError::Marker {
                    path: path.clone(),
                    message: "missing id string".to_owned(),
                })?;
            return Ok(Some(Marker {
                root: dir.to_path_buf(),
                project_id: project_id.to_owned(),
            }));
        }
    }
    Ok(None)
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["-C", start.to_str()?, "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = root.trim();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

fn git_origin_url(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", root.to_str()?, "remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let remote = String::from_utf8(output.stdout).ok()?;
    let remote = remote.trim();
    (!remote.is_empty()).then(|| remote.to_owned())
}

fn derived_id(key: &str) -> String {
    let slug = slug(key);
    format!("{slug}-{}", short_hash(key))
}

fn path_id(root: &Path, key: &str) -> String {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .map(id::sanitize_component)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_owned())
        .to_ascii_lowercase();
    format!("{name}-{}", short_hash(key))
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut previous_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            output.push(ch);
            previous_dash = false;
        } else if !previous_dash {
            output.push('-');
            previous_dash = true;
        }
    }
    output.trim_matches('-').to_owned()
}

fn short_hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..12].to_owned()
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
            "hive-memory-project-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn normalizes_common_git_url_forms() {
        let expected = "github.com/cgraf78/hive-memory";

        assert_eq!(
            normalize_remote_url("git@github.com:cgraf78/hive-memory.git"),
            expected
        );
        assert_eq!(
            normalize_remote_url("ssh://git@github.com/cgraf78/hive-memory"),
            expected
        );
        assert_eq!(
            normalize_remote_url("https://github.com/cgraf78/hive-memory.git"),
            expected
        );
    }

    #[test]
    fn local_binding_round_trips_and_removes() {
        let dir = temp_dir("binding");
        let binding = ProjectBinding {
            project_id: "github-com-cgraf78-hive-memory-abc123".to_owned(),
            store: "work".to_owned(),
        };
        let options = write::AtomicWriteOptions {
            fsync: write::FsyncPolicy::Never,
            ..write::AtomicWriteOptions::default()
        };

        let path = save_binding(&dir, &binding, &options).expect("save binding");
        let loaded = load_binding(&dir, &binding.project_id).expect("load binding");
        let removed = remove_binding(&dir, &binding.project_id).expect("remove binding");
        let missing = load_binding(&dir, &binding.project_id).expect("load missing binding");

        assert!(path.ends_with("projects/github-com-cgraf78-hive-memory-abc123.toml"));
        assert_eq!(loaded, Some(binding));
        assert_eq!(removed, Some(path));
        assert_eq!(missing, None);
    }

    #[test]
    fn project_aliases_are_idempotent_and_sorted() {
        let dir = temp_dir("aliases");
        let options = write::AtomicWriteOptions {
            fsync: write::FsyncPolicy::Never,
            ..write::AtomicWriteOptions::default()
        };

        let first = add_alias(&dir, "old-z", "new-project", &options).expect("add alias");
        let second = add_alias(&dir, "old-a", "new-project", &options).expect("add alias");
        let third = add_alias(&dir, "old-z", "new-project", &options).expect("add alias");
        let loaded = load_aliases(&dir, "new-project")
            .expect("load aliases")
            .expect("aliases exist");

        assert_eq!(first, aliases_path(&dir, "new-project"));
        assert_eq!(second, first);
        assert_eq!(third, first);
        assert_eq!(loaded.project_id, "new-project");
        assert_eq!(loaded.aliases, vec!["old-a", "old-z"]);
    }

    #[test]
    fn claimed_project_ids_include_directory_canonical_and_alias_ids() {
        let dir = temp_dir("claimed-aliases");
        let options = write::AtomicWriteOptions {
            fsync: write::FsyncPolicy::Never,
            ..write::AtomicWriteOptions::default()
        };
        add_alias(&dir, "old-project", "current-project", &options).expect("add alias");

        let ids = claimed_project_ids(&dir).expect("claimed ids");

        assert!(ids.contains("current-project"));
        assert!(ids.contains("old-project"));
    }

    #[test]
    fn related_project_ids_link_current_directory_and_old_ids() {
        let dir = temp_dir("related-aliases");
        let options = write::AtomicWriteOptions {
            fsync: write::FsyncPolicy::Never,
            ..write::AtomicWriteOptions::default()
        };
        add_alias(&dir, "old-project", "current-project", &options).expect("add alias");

        let from_current = related_project_ids(&dir, "current-project").expect("related ids");
        let from_old = related_project_ids(&dir, "old-project").expect("related ids");

        assert_eq!(from_current, from_old);
        assert!(from_current.contains("current-project"));
        assert!(from_current.contains("old-project"));
    }

    #[test]
    fn marker_file_overrides_path_identity() {
        let root = temp_dir("marker");
        let nested = root.join("a/b");
        fs::create_dir_all(&nested).expect("nested");
        fs::write(
            root.join(".hive-memory-project"),
            "id = \"project-explicit\"\n",
        )
        .expect("marker");

        let resolved = resolve_project(ResolveProjectInput {
            hint: nested,
            explicit_project_id: None,
            env_project_id: None,
        })
        .expect("resolve project");

        assert_eq!(resolved.project_id, "project-explicit");
        assert_eq!(resolved.project_root, root);
        assert_eq!(resolved.source, ProjectIdSource::Marker);
    }

    #[test]
    fn explicit_id_wins_but_keeps_resolved_root() {
        let root = temp_dir("explicit");
        fs::write(root.join(".hive-memory-project"), "id = \"marker-id\"\n").expect("marker");

        let resolved = resolve_project(ResolveProjectInput {
            hint: root.join("missing-file.rs"),
            explicit_project_id: Some("cli-id".to_owned()),
            env_project_id: Some("env-id".to_owned()),
        })
        .expect("resolve project");

        assert_eq!(resolved.project_id, "cli-id");
        assert_eq!(resolved.project_root, root);
        assert_eq!(resolved.source, ProjectIdSource::Explicit);
    }
}
