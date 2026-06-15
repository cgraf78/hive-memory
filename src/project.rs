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
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

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

    // Cache the expensive part of resolution (filesystem walk + VCS config
    // parse) per starting directory. Explicit/env IDs short-circuit it, so the
    // cache key is the start dir alone; the cached value records what the
    // filesystem revealed (root + remote-derived or path-derived identity) and
    // each caller's IDs are layered on top below. Repeated resolves in one
    // command (e.g. recall walking several hints under the same repo) then cost
    // a map lookup instead of re-walking the tree.
    let discovered = discover_cached(&start_dir)?;

    let marker_root = discovered.marker.as_ref().map(|marker| marker.root.clone());
    let project_root = marker_root
        .or_else(|| discovered.vcs_root.clone())
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
    if let Some(marker) = &discovered.marker {
        return Ok(ProjectResolution {
            project_id: marker.project_id.clone(),
            project_root,
            project_hint,
            source: ProjectIdSource::Marker,
        });
    }
    if let (Some(root), Some(remote)) = (&discovered.vcs_root, &discovered.remote_url) {
        let normalized = normalize_remote_url(remote);
        return Ok(ProjectResolution {
            project_id: derived_id(&normalized),
            project_root: root.clone(),
            project_hint,
            source: ProjectIdSource::GitRemote,
        });
    }

    let path_key = stable_path_key(&project_root);
    Ok(ProjectResolution {
        project_id: path_id(&project_root, &path_key),
        project_root,
        project_hint,
        source: ProjectIdSource::Path,
    })
}

/// Everything resolution learns from the filesystem for one starting dir.
///
/// Separated from `ProjectResolution` because it is identity-source agnostic and
/// memoizable: it does not depend on a caller's explicit/env IDs, only on the
/// directory tree. `marker` is stored so the (cheap) marker walk is shared too.
#[derive(Clone)]
struct Discovered {
    marker: Option<Marker>,
    vcs_root: Option<PathBuf>,
    /// Raw remote URL parsed from the VCS config, if any. Normalization is
    /// deliberately deferred to callers so the cached value stays a faithful
    /// copy of what is on disk.
    remote_url: Option<String>,
}

/// Process-wide memo of filesystem discovery keyed by absolutized start dir.
///
/// A `Mutex<HashMap>` is enough: contention is nil (resolution is short and
/// infrequent within a single CLI invocation) and the win is eliminating
/// repeated tree walks, not lock-free concurrency.
static DISCOVERY_CACHE: Mutex<Option<HashMap<PathBuf, Discovered>>> = Mutex::new(None);

fn discover_cached(start_dir: &Path) -> Result<Discovered, ProjectError> {
    if let Ok(guard) = DISCOVERY_CACHE.lock()
        && let Some(cache) = guard.as_ref()
        && let Some(hit) = cache.get(start_dir)
    {
        return Ok(hit.clone());
    }

    let discovered = discover(start_dir)?;

    if let Ok(mut guard) = DISCOVERY_CACHE.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(start_dir.to_path_buf(), discovered.clone());
    }
    Ok(discovered)
}

fn discover(start_dir: &Path) -> Result<Discovered, ProjectError> {
    let marker = find_marker(start_dir)?;
    let vcs_root = find_vcs_root(start_dir);
    let remote_url = vcs_root.as_deref().and_then(vcs_remote_url);
    Ok(Discovered {
        marker,
        vcs_root,
        remote_url,
    })
}

/// Clear the process-wide discovery cache. Test-only.
#[cfg(test)]
fn clear_discovery_cache() {
    if let Ok(mut guard) = DISCOVERY_CACHE.lock() {
        *guard = None;
    }
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

/// List every local project store binding.
///
/// Bindings are local machine policy, so this intentionally reads only
/// `data_dir/projects` and does not scan canonical stores. Canonical project
/// aliases are shared memory metadata and are surfaced by `hm projects show`
/// for a selected project instead.
pub fn list_bindings(data_dir: &Path) -> Result<Vec<ProjectBinding>, ProjectError> {
    let root = data_dir.join("projects");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(ProjectError::Binding {
                path: root,
                message: err.to_string(),
            });
        }
    };

    let mut bindings = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| ProjectError::Binding {
            path: root.clone(),
            message: err.to_string(),
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
            continue;
        }
        let contents = fs::read_to_string(&path).map_err(|err| ProjectError::Binding {
            path: path.clone(),
            message: err.to_string(),
        })?;
        bindings.push(toml::from_str::<ProjectBinding>(&contents).map_err(|err| {
            ProjectError::Binding {
                path: path.clone(),
                message: err.to_string(),
            }
        })?);
    }
    bindings.sort_by(|left, right| left.project_id.cmp(&right.project_id));
    Ok(bindings)
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

/// Return whether a project id is a safe single path component.
///
/// Project ids become directory names under `memories/projects/<id>/`. A
/// hostile or buggy synced store can put arbitrary strings in `aliases.toml`
/// (`project_id`/`aliases`) or even craft odd directory names, and curated
/// discovery joins those ids straight onto the store root at the highest
/// (`curated`/`human`) trust level. Anything other than a single, normal path
/// component (`..`, an absolute or rooted path, a Windows prefix, a path with
/// separators, `.`, or empty) could escape the store and inject attacker-chosen
/// `.md` files into agent context/search, so it is rejected here at the identity
/// boundary that all consumers share.
///
/// Legitimate ids are flat slugs such as
/// `github-com-cgraf78-hive-memory-018f5f57`, which pass unchanged.
pub fn is_safe_project_id(project_id: &str) -> bool {
    if project_id.is_empty() {
        return false;
    }
    let path = Path::new(project_id);
    let mut components = path.components();
    // Exactly one component, and it must be a plain file/dir name. `Normal`
    // rejects `..` (`ParentDir`), `.` (`CurDir`), `/` roots (`RootDir`),
    // and Windows drive/UNC prefixes (`Prefix`).
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(value)), None) => value.to_str() == Some(project_id),
        _ => false,
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
///
/// Every returned id is filtered through [`is_safe_project_id`] so a hostile
/// synced `aliases.toml` cannot inject path-escaping ids (`..`, absolute, or
/// multi-component) into the claimed set. Poisoned entries are dropped and the
/// scan continues, mirroring [`related_project_ids`], so one bad alias does not
/// blind the rest of a project's legitimate metadata.
pub fn claimed_project_ids(store_root: &Path) -> Result<BTreeSet<String>, ProjectError> {
    let mut ids = BTreeSet::new();
    let insert_safe = |ids: &mut BTreeSet<String>, candidate: String| {
        if is_safe_project_id(&candidate) {
            ids.insert(candidate);
        }
    };
    for directory_id in project_directories(store_root)? {
        insert_safe(&mut ids, directory_id.clone());
        if let Some(aliases) = load_aliases(store_root, &directory_id)? {
            insert_safe(&mut ids, aliases.project_id);
            for alias in aliases.aliases {
                insert_safe(&mut ids, alias);
            }
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
    // Sanitize at this identity boundary so every consumer (curated discovery,
    // doctor, search/context) is covered, not just the curated join. Ids that
    // would escape `memories/projects/` are dropped rather than turned into a
    // hard error: a single poisoned alias entry must not blind a project to the
    // rest of its legitimate memory. See `is_safe_project_id`.
    let mut ids = BTreeSet::new();
    let insert_safe = |ids: &mut BTreeSet<String>, candidate: String| {
        if is_safe_project_id(&candidate) {
            ids.insert(candidate);
        }
    };
    insert_safe(&mut ids, project_id.to_owned());
    for directory_id in project_directories(store_root)? {
        let Some(aliases) = load_aliases(store_root, &directory_id)? else {
            continue;
        };
        if aliases.project_id == project_id
            || directory_id == project_id
            || aliases.aliases.iter().any(|alias| alias == project_id)
        {
            insert_safe(&mut ids, directory_id);
            insert_safe(&mut ids, aliases.project_id);
            for alias in aliases.aliases {
                insert_safe(&mut ids, alias);
            }
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

#[derive(Clone)]
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

/// VCS marker directory/file names recognized for shell-free root discovery,
/// ordered by precedence. `.git` is checked first because it is by far the most
/// common and, for colocated `jj`, is the backend that actually holds the
/// remote config. Each is a repo-root sentinel: the first ancestor containing
/// any of them is the root.
const VCS_MARKERS: [&str; 4] = [".git", ".hg", ".jj", ".svn"];

/// Find the enclosing VCS repository root without spawning a subprocess.
///
/// Walks from `start` up through its ancestors looking for a VCS marker. `.git`
/// may be a directory (normal clone) OR a file (worktrees/submodules store a
/// `gitdir:` redirect there), so directory-vs-file is not used to filter; mere
/// existence of any marker name marks the root. This replaces
/// `git rev-parse --show-toplevel`, which on machines with a slow `git` shim
/// dominated recall latency.
fn find_vcs_root(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if VCS_MARKERS.iter().any(|marker| dir.join(marker).exists()) {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// Derive the upstream remote URL for a repo root by reading the VCS config file
/// directly, falling back to a subprocess only when parsing yields nothing.
///
/// Parsing the config file (rather than `git remote get-url origin`) keeps the
/// common path shell-free and therefore independent of how slow the `git` binary
/// is. The subprocess fallback exists only for exotic layouts the parser does
/// not cover, and never runs when the config already answers the question.
fn vcs_remote_url(root: &Path) -> Option<String> {
    if let Some(url) = parse_remote_url(root) {
        return Some(url);
    }
    git_origin_url_subprocess(root)
}

/// Parse a remote/upstream URL from the on-disk VCS config for `root`.
fn parse_remote_url(root: &Path) -> Option<String> {
    if let Some(git_dir) = resolve_git_dir(root)
        && let Some(url) = parse_git_config_origin(&git_dir.join("config"))
    {
        return Some(url);
    }
    // Mercurial / Sapling colocated checkout.
    if let Some(url) = parse_hg_default(&root.join(".hg/hgrc")) {
        return Some(url);
    }
    // Non-colocated jj keeps its git backend under `.jj/repo/store/git`.
    parse_git_config_origin(&root.join(".jj/repo/store/git/config"))
}

/// Resolve the real git directory for a repo root, following the `gitdir:`
/// redirect used by worktrees and submodules when `.git` is a file.
fn resolve_git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let metadata = fs::metadata(&dot_git).ok()?;
    if metadata.is_dir() {
        return Some(dot_git);
    }
    if metadata.is_file() {
        let contents = fs::read_to_string(&dot_git).ok()?;
        let target = contents.lines().find_map(|line| {
            line.trim()
                .strip_prefix("gitdir:")
                .map(|value| value.trim().to_owned())
        })?;
        let target = PathBuf::from(target);
        let resolved = if target.is_absolute() {
            target
        } else {
            root.join(target)
        };
        // Worktree gitdirs point at `.git/worktrees/<name>`; the remote config
        // lives in the shared `commondir`, not the per-worktree gitdir.
        if let Some(common) = read_commondir(&resolved) {
            return Some(common);
        }
        return Some(resolved);
    }
    None
}

/// Follow a worktree gitdir's `commondir` pointer to the shared git directory.
fn read_commondir(git_dir: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(git_dir.join("commondir")).ok()?;
    let relative = contents.trim();
    if relative.is_empty() {
        return None;
    }
    let common = PathBuf::from(relative);
    Some(if common.is_absolute() {
        common
    } else {
        git_dir.join(common)
    })
}

/// Extract the `[remote "origin"] url` from a git config file.
///
/// Hand-rolled rather than pulling in a git library: the format is a tiny INI
/// dialect and we only need one key. Section headers and `key = value` lines are
/// matched leniently (whitespace, inline comments) but conservatively enough to
/// pick `origin` over other remotes regardless of declaration order.
fn parse_git_config_origin(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    let mut in_origin = false;
    for raw in contents.lines() {
        let line = raw.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_origin = is_origin_section(section);
            continue;
        }
        if !in_origin {
            continue;
        }
        if let Some(value) = config_value(line, "url") {
            return Some(value);
        }
    }
    None
}

/// True for a `remote "origin"` section header body (the text inside `[...]`).
fn is_origin_section(section: &str) -> bool {
    let mut parts = section.split_whitespace();
    if parts.next() != Some("remote") {
        return false;
    }
    // Subsection name is quoted: `remote "origin"`.
    parts.next().map(|name| name.trim_matches('"')) == Some("origin")
}

/// Parse the Mercurial/Sapling `[paths] default = ...` upstream URL.
fn parse_hg_default(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    let mut in_paths = false;
    for raw in contents.lines() {
        let line = raw.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_paths = section.trim() == "paths";
            continue;
        }
        if in_paths && let Some(value) = config_value(line, "default") {
            return Some(value);
        }
    }
    None
}

/// Parse `key = value` from one INI line, returning the value for `key`.
///
/// Stops at `#`/`;` inline comments and ignores blank/comment lines so a stray
/// comment after the URL never leaks into the project id.
fn config_value(line: &str, key: &str) -> Option<String> {
    if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
        return None;
    }
    let (name, value) = line.split_once('=')?;
    if name.trim() != key {
        return None;
    }
    let value = value.trim();
    let value = value
        .split_once(" #")
        .or_else(|| value.split_once(" ;"))
        .map_or(value, |(head, _)| head);
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// Last-resort remote lookup via `git`, used only when config parsing fails.
fn git_origin_url_subprocess(root: &Path) -> Option<String> {
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

/// Machine-stable hash key for the path-fallback project id.
///
/// The path fallback is the last identity rung (no explicit/env id, no marker,
/// no VCS remote), so the only signal is the directory location. Hashing the
/// absolute path makes the id host-variant — `/home/cgraf/git/foo` and
/// `/Users/chris/git/foo` are the same logical project but differ by OS home
/// dir, so project-scoped memory written on one machine is invisible on the
/// other. To keep the id stable across hosts, key off the `$HOME`-relative path
/// (namespaced with `~/`) when the root lives under `$HOME`; two machines with
/// the same layout under their respective homes then derive the same id.
///
/// Residual: paths outside `$HOME` (or when `$HOME` is unknown) fall back to the
/// absolute path and stay host-local — declare a `.hive-memory-project` marker
/// or use a VCS remote for cross-host stability there.
fn stable_path_key(root: &Path) -> String {
    path_key_relative_to(root, home_dir().as_deref())
}

/// Pure core of [`stable_path_key`], parameterized on `home` so it is testable
/// without mutating the process `$HOME`. Returns `~`/`~/<rel>` when `root` is
/// under `home`, else the absolute path.
fn path_key_relative_to(root: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(rel) = root.strip_prefix(home)
    {
        let rel = rel.to_string_lossy();
        return if rel.is_empty() {
            "~".to_owned()
        } else {
            format!("~/{rel}")
        };
    }
    root.to_string_lossy().into_owned()
}

/// Canonicalized `$HOME`, if known. Canonicalized so it matches the
/// already-canonicalized project root (symlinked homes, macOS `/var` ->
/// `/private/var`); `strip_prefix` is purely lexical and would otherwise miss.
fn home_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty())?;
    let home = PathBuf::from(home);
    Some(fs::canonicalize(&home).unwrap_or(home))
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
    fn claimed_project_ids_drops_path_escaping_aliases() {
        let dir = temp_dir("claimed-poisoned");
        let projects = dir.join("memories/projects/current-project");
        fs::create_dir_all(&projects).expect("project dir");
        // A hostile synced store can list path-escaping ids in `aliases.toml`.
        // The claimed set must drop them so no downstream path-sink consumer can
        // be steered outside `memories/projects/`, while legitimate ids survive.
        fs::write(
            projects.join("aliases.toml"),
            "schema_version = 1\nproject_id = \"current-project\"\naliases = [\"../../../../tmp/evil\", \"/etc/passwd\", \"a/b\", \"old-project\"]\n",
        )
        .expect("write aliases");

        let ids = claimed_project_ids(&dir).expect("claimed ids");

        assert!(ids.contains("current-project"));
        assert!(ids.contains("old-project"));
        assert!(!ids.contains("../../../../tmp/evil"));
        assert!(!ids.contains("/etc/passwd"));
        assert!(!ids.contains("a/b"));
        assert!(ids.iter().all(|id| is_safe_project_id(id)));
    }

    #[test]
    fn related_project_ids_link_current_directory_and_old_ids() {
        let dir = temp_dir("related-aliases");
        let options = write::AtomicWriteOptions {
            fsync: write::FsyncPolicy::Never,
            ..write::AtomicWriteOptions::default()
        };
        add_alias(&dir, "old-project", "current-project", &options).expect("add alias");
        // A second, unrelated project with its own alias chain. It shares no id
        // with the first, so it must NOT bleed into the first project's related
        // set — without this negative case the link test would also pass for a
        // buggy implementation that returned every known project id.
        add_alias(&dir, "old-other", "unrelated-project", &options).expect("add unrelated alias");

        let from_current = related_project_ids(&dir, "current-project").expect("related ids");
        let from_old = related_project_ids(&dir, "old-project").expect("related ids");

        assert_eq!(from_current, from_old);
        assert!(from_current.contains("current-project"));
        assert!(from_current.contains("old-project"));
        // Negative: the unrelated project and its alias must be excluded.
        assert!(!from_current.contains("unrelated-project"));
        assert!(!from_current.contains("old-other"));
        // The related set is exactly the linked pair, nothing more.
        assert_eq!(
            from_current,
            BTreeSet::from(["current-project".to_owned(), "old-project".to_owned()])
        );
    }

    #[test]
    fn is_safe_project_id_accepts_normal_slugs() {
        assert!(is_safe_project_id(
            "github-com-cgraf78-hive-memory-018f5f57"
        ));
        assert!(is_safe_project_id("current-project"));
        assert!(is_safe_project_id("a"));
        assert!(is_safe_project_id("under_score.dot"));
    }

    #[test]
    fn is_safe_project_id_rejects_escapes() {
        assert!(!is_safe_project_id(""));
        assert!(!is_safe_project_id("."));
        assert!(!is_safe_project_id(".."));
        assert!(!is_safe_project_id("../etc"));
        assert!(!is_safe_project_id("../../../../tmp/evil"));
        assert!(!is_safe_project_id("a/b"));
        assert!(!is_safe_project_id("/etc/passwd"));
        assert!(!is_safe_project_id("/tmp/evil"));
        // Trailing separator collapses to one component but is not a flat slug.
        assert!(!is_safe_project_id("evil/"));
    }

    #[test]
    fn related_project_ids_drops_path_escaping_aliases() {
        let dir = temp_dir("related-poisoned");
        let projects = dir.join("memories/projects/current-project");
        fs::create_dir_all(&projects).expect("project dir");
        // A hostile or buggy synced store can list arbitrary strings here. The
        // canonical id stays usable, but the escaping aliases must be dropped
        // so curated discovery never joins them onto the store root.
        fs::write(
            projects.join("aliases.toml"),
            "schema_version = 1\nproject_id = \"current-project\"\naliases = [\"../../../../tmp/evil\", \"/etc\", \"old-project\"]\n",
        )
        .expect("write aliases");

        let ids = related_project_ids(&dir, "current-project").expect("related ids");

        assert!(ids.contains("current-project"));
        assert!(ids.contains("old-project"));
        assert!(!ids.contains("../../../../tmp/evil"));
        assert!(!ids.contains("/etc"));
        assert!(ids.iter().all(|id| is_safe_project_id(id)));
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
        // macOS exposes the temp directory through /var while canonical paths
        // resolve through /private/var. The resolver intentionally stores the
        // canonical marker root so project identity is stable across equivalent
        // path spellings.
        assert_eq!(
            resolved.project_root,
            root.canonicalize().expect("canonical root")
        );
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

    /// Write a minimal git config with one `origin` remote under `root/.git`.
    fn write_git_config(root: &Path, url: &str) {
        let git_dir = root.join(".git");
        fs::create_dir_all(&git_dir).expect("git dir");
        fs::write(
            git_dir.join("config"),
            format!("[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"),
        )
        .expect("git config");
    }

    #[test]
    fn marker_walk_finds_git_dir_root() {
        let root = temp_dir("vcs-git-dir");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        let nested = root.join("a/b/c");
        fs::create_dir_all(&nested).expect("nested");

        let found = find_vcs_root(&nested).expect("root found");

        assert_eq!(found, root);
    }

    #[test]
    fn marker_walk_finds_git_file_root() {
        let root = temp_dir("vcs-git-file");
        // Worktrees and submodules store `.git` as a file, not a directory.
        fs::write(root.join(".git"), "gitdir: /elsewhere/.git/worktrees/wt\n").expect("git file");

        let found = find_vcs_root(&root).expect("root found");

        assert_eq!(found, root);
    }

    #[test]
    fn marker_walk_finds_hg_root() {
        let root = temp_dir("vcs-hg");
        fs::create_dir_all(root.join(".hg")).expect("hg dir");
        let nested = root.join("deep/leaf");
        fs::create_dir_all(&nested).expect("nested");

        let found = find_vcs_root(&nested).expect("root found");

        assert_eq!(found, root);
    }

    #[test]
    fn marker_walk_returns_none_without_vcs() {
        let root = temp_dir("vcs-none");
        let nested = root.join("x/y");
        fs::create_dir_all(&nested).expect("nested");

        assert_eq!(find_vcs_root(&nested), None);
    }

    #[test]
    fn parses_ssh_and_https_git_origin() {
        let ssh_root = temp_dir("git-ssh");
        write_git_config(&ssh_root, "git@github.com:cgraf78/hive-memory.git");
        let https_root = temp_dir("git-https");
        write_git_config(&https_root, "https://github.com/cgraf78/hive-memory.git");

        let ssh_url = vcs_remote_url(&ssh_root).expect("ssh url");
        let https_url = vcs_remote_url(&https_root).expect("https url");

        // Equivalent URL spellings must normalize to one identity, matching the
        // ids the old `git remote get-url` path produced.
        assert_eq!(
            derived_id(&normalize_remote_url(&ssh_url)),
            derived_id(&normalize_remote_url(&https_url))
        );
        assert_eq!(
            derived_id(&normalize_remote_url(&ssh_url)),
            "github-com-cgraf78-hive-memory-f8a6daf797a6"
        );
    }

    #[test]
    fn parses_origin_without_trailing_dot_git() {
        let root = temp_dir("git-no-suffix");
        write_git_config(&root, "https://github.com/cgraf78/hive-memory");

        let url = vcs_remote_url(&root).expect("url");

        assert_eq!(
            derived_id(&normalize_remote_url(&url)),
            "github-com-cgraf78-hive-memory-f8a6daf797a6"
        );
    }

    #[test]
    fn picks_origin_among_multiple_remotes_regardless_of_order() {
        let root = temp_dir("git-multi-remote");
        let git_dir = root.join(".git");
        fs::create_dir_all(&git_dir).expect("git dir");
        // `upstream` declared before `origin` to prove section selection, not
        // first-url-wins, drives the choice.
        fs::write(
            git_dir.join("config"),
            "[remote \"upstream\"]\n\turl = git@github.com:other/fork.git\n[remote \"origin\"]\n\turl = git@github.com:cgraf78/hive-memory.git\n",
        )
        .expect("git config");

        let url = vcs_remote_url(&root).expect("url");

        assert_eq!(url, "git@github.com:cgraf78/hive-memory.git");
    }

    #[test]
    fn resolves_git_file_redirect_to_real_config() {
        // Simulate a worktree: `.git` is a file pointing at a worktree gitdir
        // whose `commondir` redirects to the shared git dir holding the remote.
        let main = temp_dir("git-worktree-main");
        let common = main.join(".git");
        fs::create_dir_all(&common).expect("common dir");
        fs::write(
            common.join("config"),
            "[remote \"origin\"]\n\turl = git@github.com:cgraf78/hive-memory.git\n",
        )
        .expect("config");
        let worktree_gitdir = common.join("worktrees/wt");
        fs::create_dir_all(&worktree_gitdir).expect("worktree gitdir");
        fs::write(worktree_gitdir.join("commondir"), "../..\n").expect("commondir");

        let checkout = temp_dir("git-worktree-checkout");
        fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", worktree_gitdir.display()),
        )
        .expect("git file");

        let url = vcs_remote_url(&checkout).expect("url");

        assert_eq!(url, "git@github.com:cgraf78/hive-memory.git");
    }

    #[test]
    fn parses_hg_default_path() {
        let root = temp_dir("hg-default");
        let hg_dir = root.join(".hg");
        fs::create_dir_all(&hg_dir).expect("hg dir");
        fs::write(
            hg_dir.join("hgrc"),
            "[paths]\ndefault = https://github.com/cgraf78/hive-memory\n",
        )
        .expect("hgrc");

        let url = vcs_remote_url(&root).expect("url");

        assert_eq!(
            derived_id(&normalize_remote_url(&url)),
            "github-com-cgraf78-hive-memory-f8a6daf797a6"
        );
    }

    #[test]
    fn remote_url_is_none_when_config_has_no_origin() {
        let root = temp_dir("git-no-origin");
        let git_dir = root.join(".git");
        fs::create_dir_all(&git_dir).expect("git dir");
        // No origin and no real git binary will succeed here, so the fallback
        // also yields nothing.
        fs::write(
            git_dir.join("config"),
            "[remote \"upstream\"]\n\turl = git@github.com:other/fork.git\n",
        )
        .expect("config");

        assert_eq!(parse_remote_url(&root), None);
    }

    #[test]
    fn resolve_uses_parsed_git_origin_for_id() {
        clear_discovery_cache();
        let root = temp_dir("resolve-git-id");
        write_git_config(&root, "git@github.com:cgraf78/hive-memory.git");

        let resolved = resolve_project(ResolveProjectInput {
            hint: root.clone(),
            explicit_project_id: None,
            env_project_id: None,
        })
        .expect("resolve project");

        assert_eq!(resolved.source, ProjectIdSource::GitRemote);
        assert_eq!(
            resolved.project_id,
            "github-com-cgraf78-hive-memory-f8a6daf797a6"
        );
    }

    #[test]
    fn discovery_cache_returns_same_result_without_rewalking() {
        clear_discovery_cache();
        let root = temp_dir("resolve-cache");
        write_git_config(&root, "git@github.com:cgraf78/hive-memory.git");
        let start = starting_dir(&absolutize_hint(&root).expect("absolutize"));

        let first = discover_cached(&start).expect("first discover");
        // Mutating the config after the first walk proves the second read is
        // served from cache, not re-parsed from disk.
        fs::write(root.join(".git/config"), "[core]\n").expect("clobber config");
        let second = discover_cached(&start).expect("second discover");

        assert_eq!(first.remote_url, second.remote_url);
        assert_eq!(
            first.remote_url.as_deref(),
            Some("git@github.com:cgraf78/hive-memory.git")
        );
    }

    #[test]
    fn resolve_falls_back_to_path_identity_without_marker_or_vcs() {
        clear_discovery_cache();
        // A bare directory with no `.hive-memory-project` marker and none of the
        // VCS sentinels in `VCS_MARKERS` exercises the final identity rung: the
        // path fallback that `resolve_project` reaches only after marker and
        // git-remote discovery both come up empty.
        let root = temp_dir("resolve-path-fallback");
        for marker in VCS_MARKERS {
            assert!(
                !root.join(marker).exists(),
                "fixture must have no {marker} so resolution reaches the path rung"
            );
        }

        let resolved = resolve_project(ResolveProjectInput {
            hint: root.clone(),
            explicit_project_id: None,
            env_project_id: None,
        })
        .expect("resolve project");

        // The whole point of this test: the source is the path rung, not any of
        // the higher-precedence rungs that other tests already cover.
        assert_eq!(resolved.source, ProjectIdSource::Path);
        // Root is the canonicalized hint (matches the resolver's contract; on
        // macOS /var canonicalizes through /private/var).
        let canonical = root.canonicalize().expect("canonical root");
        assert_eq!(resolved.project_root, canonical);
        // The id must be the `<dir-slug>-<12 hex>` shape produced by `path_id`,
        // and must actually match what `path_id` derives for this root so the
        // assertion is not a loose regex that any string could pass.
        let expected_id = path_id(&canonical, &stable_path_key(&canonical));
        assert_eq!(resolved.project_id, expected_id);
        let (slug, hash) = resolved
            .project_id
            .rsplit_once('-')
            .expect("id has a -<hash> suffix");
        assert!(!slug.is_empty(), "slug part must be non-empty");
        assert_eq!(hash.len(), 12, "hash suffix is 12 hex chars: {hash}");
        assert!(
            hash.chars().all(|ch| ch.is_ascii_hexdigit()),
            "hash suffix must be hex: {hash}"
        );
    }

    #[test]
    fn discovery_cache_isolates_distinct_start_dirs() {
        clear_discovery_cache();
        // Two unrelated repos with different origins. The cache is keyed per
        // start dir, so each must retain its OWN discovered identity; a bug that
        // collapsed all keys into one slot would let B's resolution return A's
        // remote (or vice versa). The single-key cache test cannot catch that.
        let root_a = temp_dir("cache-isolation-a");
        write_git_config(&root_a, "git@github.com:cgraf78/alpha.git");
        let root_b = temp_dir("cache-isolation-b");
        write_git_config(&root_b, "git@github.com:cgraf78/bravo.git");

        let resolved_a = resolve_project(ResolveProjectInput {
            hint: root_a.clone(),
            explicit_project_id: None,
            env_project_id: None,
        })
        .expect("resolve a");
        // Resolve B AFTER A is cached, then clobber A's config. B is a first-time
        // resolution, so it must read its own on-disk config and must NOT be
        // contaminated by A's cached entry.
        fs::write(root_a.join(".git/config"), "[core]\n").expect("clobber a config");
        let resolved_b = resolve_project(ResolveProjectInput {
            hint: root_b.clone(),
            explicit_project_id: None,
            env_project_id: None,
        })
        .expect("resolve b");

        assert_eq!(resolved_a.source, ProjectIdSource::GitRemote);
        assert_eq!(resolved_b.source, ProjectIdSource::GitRemote);
        assert_eq!(
            resolved_a.project_id,
            derived_id(&normalize_remote_url("git@github.com:cgraf78/alpha.git"))
        );
        assert_eq!(
            resolved_b.project_id,
            derived_id(&normalize_remote_url("git@github.com:cgraf78/bravo.git"))
        );
        // The two ids must differ: cross-key leakage would make them equal.
        assert_ne!(resolved_a.project_id, resolved_b.project_id);
    }

    #[test]
    fn path_key_is_home_relative_and_host_stable() {
        // Same layout under different OS home dirs must yield the same key.
        let linux = Path::new("/home/cgraf/git/foo");
        let macos = Path::new("/Users/chris/git/foo");
        let k_linux = path_key_relative_to(linux, Some(Path::new("/home/cgraf")));
        let k_macos = path_key_relative_to(macos, Some(Path::new("/Users/chris")));
        assert_eq!(k_linux, "~/git/foo");
        assert_eq!(k_linux, k_macos);
    }

    #[test]
    fn path_id_is_identical_across_hosts_for_same_relative_layout() {
        // The actual bug: project_id must match across machines so project-scoped
        // memory written on one is recalled on the other.
        let linux = Path::new("/home/cgraf/git/foo");
        let macos = Path::new("/Users/chris/git/foo");
        let id_linux = path_id(
            linux,
            &path_key_relative_to(linux, Some(Path::new("/home/cgraf"))),
        );
        let id_macos = path_id(
            macos,
            &path_key_relative_to(macos, Some(Path::new("/Users/chris"))),
        );
        assert_eq!(id_linux, id_macos);
        assert!(id_linux.starts_with("foo-"));
    }

    #[test]
    fn path_key_for_home_root_is_tilde() {
        assert_eq!(
            path_key_relative_to(Path::new("/home/cgraf"), Some(Path::new("/home/cgraf"))),
            "~"
        );
    }

    #[test]
    fn path_key_outside_home_falls_back_to_absolute() {
        // Paths outside $HOME stay host-local (documented residual).
        assert_eq!(
            path_key_relative_to(Path::new("/opt/work/foo"), Some(Path::new("/home/cgraf"))),
            "/opt/work/foo"
        );
        // Unknown $HOME also falls back to the absolute path.
        assert_eq!(
            path_key_relative_to(Path::new("/home/cgraf/git/foo"), None),
            "/home/cgraf/git/foo"
        );
    }

    #[test]
    fn path_key_does_not_strip_partial_home_prefix() {
        // /home/cgraf2 is NOT under /home/cgraf; strip_prefix is path-component
        // aware so this must fall back to absolute, not become "~2/...".
        assert_eq!(
            path_key_relative_to(
                Path::new("/home/cgraf2/foo"),
                Some(Path::new("/home/cgraf"))
            ),
            "/home/cgraf2/foo"
        );
    }
}
