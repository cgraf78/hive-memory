//! Store manifest and initialization primitives.
//!
//! A store root is the durable boundary for one hive of memory. Config answers
//! "where should this alias point right now"; the manifest answers "what store
//! is actually here." Keeping those responsibilities separate lets folders
//! move, sync, or get renamed without changing the store's identity.

use crate::config::{Sensitivity, StoreConfig};
use crate::write::{self, AtomicWriteOptions, FsyncPolicy};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use uuid::Uuid;

/// Store manifest schema supported by this build.
///
/// V1 ships with no migrators. Future schema bumps should add explicit migrate
/// code before this constant changes.
pub const SUPPORTED_MANIFEST_SCHEMA_VERSION: u32 = 1;

const CREATED_BY: &str = "hive-memory";

/// Canonical directory skeleton every v1 store root should contain.
///
/// Store initialization creates these directories, and doctor uses the same list
/// so layout drift is reported against the exact creation contract.
pub const CANONICAL_DIRS: &[&str] = &[
    "people",
    "rules",
    "memories/global",
    "memories/agents",
    "memories/projects",
    "inbox/events",
    "inbox/notes",
    "generated",
];

/// Ignore policy for store-local generated artifacts.
///
/// Generated output is rebuildable and often adapter/session-specific. Keeping
/// this file in every store makes accidental tracking opt-out by default while
/// still allowing a user to force-add an explicitly shared generated artifact.
pub const GENERATED_GITIGNORE: &str = "*\n!.gitignore\n";

/// Options for creating a new store root.
///
/// This is the API that `hm stores init` should call after CLI parsing. The
/// caller supplies user intent; this module supplies identity, timestamps,
/// defaults, directory layout, and manifest write behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreInitOptions {
    /// Local alias/human name to place in the manifest.
    pub name: String,
    /// Filesystem root where the store should be initialized.
    pub root: PathBuf,
    /// Optional human description for diagnostics and generated docs.
    pub description: Option<String>,
    /// Store sensitivity copied from config/CLI into the manifest.
    pub sensitivity: Sensitivity,
}

/// Inputs for store diagnostics.
///
/// The caller passes the local config alias and store config so diagnostics can
/// compare "where config points" with "what the manifest says is there" without
/// requiring the store module to load global config itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreDoctorInput<'a> {
    /// Local config alias being diagnosed.
    pub name: &'a str,
    /// Configured store values for that alias.
    pub config: &'a StoreConfig,
}

/// Diagnostic result for one configured store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreDoctorReport {
    /// Local config alias that was checked.
    pub name: String,
    /// Filesystem root from config.
    pub root: PathBuf,
    /// Whether a parseable manifest file was found.
    pub manifest_available: bool,
    /// Warnings/errors discovered for this store.
    pub issues: Vec<StoreDoctorIssue>,
}

/// Store diagnostic finding.
///
/// Missing manifests and alias drift are warnings because they are expected in
/// early setup or after a harmless local alias rename. Newer/corrupt manifests
/// are errors because continuing could interpret store data incorrectly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreDoctorIssue {
    /// Finding severity.
    pub level: StoreDoctorLevel,
    /// Human-readable diagnostic message.
    pub message: String,
}

/// Severity for a store diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreDoctorLevel {
    /// Non-fatal setup or drift issue.
    Warning,
    /// Fatal issue that makes the store unsafe to use automatically.
    Error,
}

/// Parsed `manifest.toml` from a store root.
///
/// `store.id` is the stable identity. Code should not treat the configured
/// alias or folder name as durable identity once a manifest exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreManifest {
    /// Manifest schema version.
    pub schema_version: u32,
    /// Tool family that created the manifest.
    pub created_by: String,
    /// RFC3339 timestamp when the store identity was created.
    pub created_at: String,
    /// RFC3339 timestamp when manifest metadata last changed.
    pub updated_at: String,
    /// Stable store identity and human metadata.
    pub store: ManifestStore,
    /// Store-local safety defaults.
    pub policies: ManifestPolicies,
    /// Features that readers may expect to find in this store.
    pub capabilities: ManifestCapabilities,
}

/// Human-facing identity block inside a store manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestStore {
    /// Stable UUID identity for this store.
    pub id: String,
    /// Human-facing store name captured at initialization.
    pub name: String,
    /// Optional human description.
    pub description: Option<String>,
    /// Store sensitivity captured at initialization.
    pub sensitivity: Sensitivity,
}

/// Store-local policy defaults.
///
/// These are intentionally stored with the hive instead of only in user config:
/// collaborators and future hosts need to see the store's own safety defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPolicies {
    /// Whether raw inbox writes should be append-only.
    pub append_only_inbox: bool,
    /// Whether tools may edit curated memory files directly.
    pub allow_direct_curated_edits: bool,
    /// Raw inbox retention policy.
    pub retention: RetentionPolicy,
}

/// Raw-note retention policy for inbox material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Retention mode, such as `keep-raw`.
    pub mode: String,
    /// Optional retention window for modes that expire raw material.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days: Option<u32>,
}

/// Capabilities advertised by this store.
///
/// These flags are written to the store so future tools can reason about what
/// data they may find without relying only on the installed `hm` version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestCapabilities {
    /// Store may contain JSON event sidecars.
    pub json_events: bool,
    /// Store may contain local outbox material.
    pub local_outbox: bool,
    /// Compaction support advertised by this store.
    pub compaction: String,
}

/// Store initialization or manifest I/O failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// Store root already contains a manifest.
    ManifestExists(PathBuf),
    /// Underlying filesystem failure with operation context.
    Io {
        /// Operation that failed.
        action: &'static str,
        /// Path involved in the failure.
        path: PathBuf,
        /// Original error rendered for CLI diagnostics.
        message: String,
    },
    /// Manifest TOML could not be parsed or serialized.
    ParseManifest(String),
    /// Manifest schema is newer than this build can safely read.
    UnsupportedSchema {
        /// Schema version found on disk.
        found: u32,
        /// Newest schema version supported by this build.
        supported: u32,
    },
}

/// V1 migration result.
///
/// V1 supports only schema 1, so migration is intentionally a no-op. Keeping a
/// real return type makes the CLI workflow stable before future migrators exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Number of configured stores inspected.
    pub stores_checked: usize,
    /// Number of migrations that ran.
    pub migrations_run: usize,
    /// Whether this was a dry-run request.
    pub dry_run: bool,
}

impl Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManifestExists(path) => {
                write!(f, "store manifest already exists: {}", path.display())
            }
            Self::Io {
                action,
                path,
                message,
            } => write!(f, "failed to {action} {}: {message}", path.display()),
            Self::ParseManifest(message) => write!(f, "failed to parse store manifest: {message}"),
            Self::UnsupportedSchema { found, supported } => write!(
                f,
                "store manifest schema_version {found} is newer than supported {supported}"
            ),
        }
    }
}

impl Error for StoreError {}

impl StoreManifest {
    /// Create a new manifest using production UUID and timestamp generation.
    ///
    /// New manifests get a UUIDv7 so identities are globally unique and roughly
    /// time-sortable without needing a central coordinator.
    pub fn new(
        name: impl Into<String>,
        description: Option<String>,
        sensitivity: Sensitivity,
    ) -> Self {
        Self::with_identity(
            name,
            description,
            sensitivity,
            Uuid::now_v7().to_string(),
            now_rfc3339(),
        )
    }

    /// Create a manifest with caller-provided identity and timestamp.
    ///
    /// This is public for import/migration tests and any future repair command
    /// that needs deterministic manifest material. Normal initialization should
    /// use [`StoreManifest::new`].
    pub fn with_identity(
        name: impl Into<String>,
        description: Option<String>,
        sensitivity: Sensitivity,
        id: String,
        timestamp: String,
    ) -> Self {
        Self {
            schema_version: SUPPORTED_MANIFEST_SCHEMA_VERSION,
            created_by: CREATED_BY.to_owned(),
            created_at: timestamp.clone(),
            updated_at: timestamp,
            store: ManifestStore {
                id,
                name: name.into(),
                description,
                sensitivity,
            },
            policies: ManifestPolicies::default(),
            capabilities: ManifestCapabilities::default(),
        }
    }

    /// Parse and validate a manifest TOML document.
    ///
    /// This enforces the schema-version read contract early so command code does
    /// not accidentally operate on a store written by a newer incompatible tool.
    pub fn from_toml_str(input: &str) -> Result<Self, StoreError> {
        let manifest: Self =
            toml::from_str(input).map_err(|err| StoreError::ParseManifest(err.to_string()))?;
        manifest.validate_schema()?;
        Ok(manifest)
    }

    /// Serialize this manifest as stable, human-readable TOML.
    pub fn to_toml_string(&self) -> Result<String, StoreError> {
        toml::to_string_pretty(self).map_err(|err| StoreError::ParseManifest(err.to_string()))
    }

    fn validate_schema(&self) -> Result<(), StoreError> {
        if self.schema_version > SUPPORTED_MANIFEST_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema {
                found: self.schema_version,
                supported: SUPPORTED_MANIFEST_SCHEMA_VERSION,
            });
        }
        Ok(())
    }
}

impl Default for ManifestPolicies {
    fn default() -> Self {
        Self {
            append_only_inbox: true,
            allow_direct_curated_edits: false,
            retention: RetentionPolicy {
                mode: "keep-raw".to_owned(),
                days: None,
            },
        }
    }
}

impl Default for ManifestCapabilities {
    fn default() -> Self {
        Self {
            json_events: true,
            local_outbox: true,
            compaction: "manual".to_owned(),
        }
    }
}

/// Initialize a store root and return the manifest that was written.
///
/// This function creates the canonical directory skeleton before writing the
/// manifest. If manifest creation fails, the root may contain empty directories,
/// but it will not contain a partial `manifest.toml`.
pub fn init_store(options: &StoreInitOptions) -> Result<StoreManifest, StoreError> {
    let path = manifest_path(&options.root);
    if path.exists() {
        return Err(StoreError::ManifestExists(path));
    }

    let manifest = StoreManifest::new(
        options.name.clone(),
        options.description.clone(),
        options.sensitivity,
    );
    create_store_root(&options.root, options.sensitivity)?;
    write_generated_gitignore(&options.root)?;
    write_readme(&options.root, &manifest)?;
    write_manifest_atomic(&options.root, &manifest)?;
    Ok(manifest)
}

/// Read and validate `manifest.toml` from a store root.
pub fn read_manifest(root: &Path) -> Result<StoreManifest, StoreError> {
    let path = manifest_path(root);
    let contents = fs::read_to_string(&path).map_err(|err| StoreError::Io {
        action: "read manifest",
        path: path.clone(),
        message: err.to_string(),
    })?;
    StoreManifest::from_toml_str(&contents)
}

/// Run diagnostics for one configured store.
///
/// This is intentionally non-destructive. Missing manifests and config/manifest
/// mismatches are surfaced as findings for `hm stores doctor`; repair commands
/// can build on the same checks later without hiding drift.
pub fn doctor_store(input: StoreDoctorInput<'_>) -> StoreDoctorReport {
    let mut report = StoreDoctorReport {
        name: input.name.to_owned(),
        root: input.config.root.clone(),
        manifest_available: false,
        issues: Vec::new(),
    };

    let manifest_path = manifest_path(&input.config.root);
    if !manifest_path.is_file() {
        report.issues.push(StoreDoctorIssue {
            level: StoreDoctorLevel::Warning,
            message: format!(
                "missing manifest; initialize with `hm stores init {} --root {}`",
                input.name,
                input.config.root.display()
            ),
        });
        return report;
    }

    match read_manifest(&input.config.root) {
        Ok(manifest) => {
            report.manifest_available = true;
            if manifest.store.name != input.name {
                report.issues.push(StoreDoctorIssue {
                    level: StoreDoctorLevel::Warning,
                    message: format!(
                        "manifest store.name is {}, config alias is {}",
                        manifest.store.name, input.name
                    ),
                });
            }

            if let Some(expected_id) = &input.config.expected_id
                && expected_id != &manifest.store.id
            {
                report.issues.push(StoreDoctorIssue {
                    level: StoreDoctorLevel::Error,
                    message: format!(
                        "manifest store.id is {}, config expected_id is {}",
                        manifest.store.id, expected_id
                    ),
                });
            }

            if manifest.store.sensitivity != input.config.sensitivity {
                let stricter =
                    stricter_sensitivity(manifest.store.sensitivity, input.config.sensitivity);
                report.issues.push(StoreDoctorIssue {
                    level: StoreDoctorLevel::Warning,
                    message: format!(
                        "sensitivity mismatch: config={}, manifest={}, effective={}",
                        input.config.sensitivity, manifest.store.sensitivity, stricter
                    ),
                });
            }
        }
        Err(err) => {
            report.issues.push(StoreDoctorIssue {
                level: StoreDoctorLevel::Error,
                message: err.to_string(),
            });
        }
    }

    report
}

/// Return the stricter of two sensitivity classes.
pub fn stricter_sensitivity(left: Sensitivity, right: Sensitivity) -> Sensitivity {
    if sensitivity_rank(left) >= sensitivity_rank(right) {
        left
    } else {
        right
    }
}

/// Report v1 migration status for configured stores.
///
/// No schema migrators exist in v1. The command still checks manifests so a
/// future migration workflow has a real, testable entry point from day one.
pub fn migrate_stores<'a, I>(stores: I, dry_run: bool) -> Result<MigrationReport, StoreError>
where
    I: IntoIterator<Item = StoreDoctorInput<'a>>,
{
    let mut stores_checked = 0;
    for store in stores {
        stores_checked += 1;
        if manifest_path(&store.config.root).is_file() {
            read_manifest(&store.config.root)?;
        }
    }

    Ok(MigrationReport {
        stores_checked,
        migrations_run: 0,
        dry_run,
    })
}

fn sensitivity_rank(sensitivity: Sensitivity) -> u8 {
    match sensitivity {
        Sensitivity::Public => 0,
        Sensitivity::Internal => 1,
        Sensitivity::Private => 2,
        Sensitivity::Secret => 3,
    }
}

fn create_store_root(root: &Path, sensitivity: Sensitivity) -> Result<(), StoreError> {
    fs::create_dir_all(root).map_err(|err| StoreError::Io {
        action: "create store root",
        path: root.to_path_buf(),
        message: err.to_string(),
    })?;
    protect_sensitive_root(root, sensitivity)?;

    for relative in CANONICAL_DIRS {
        let path = root.join(relative);
        fs::create_dir_all(&path).map_err(|err| StoreError::Io {
            action: "create store directory",
            path,
            message: err.to_string(),
        })?;
    }

    Ok(())
}

#[cfg(unix)]
fn protect_sensitive_root(root: &Path, sensitivity: Sensitivity) -> Result<(), StoreError> {
    if !matches!(sensitivity, Sensitivity::Private | Sensitivity::Secret) {
        return Ok(());
    }

    // The root directory is the practical access boundary for the store. Set it
    // explicitly instead of trusting umask so a freshly initialized private or
    // secret hive does not immediately fail doctor on permissive systems.
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).map_err(|err| StoreError::Io {
        action: "set store root permissions",
        path: root.to_path_buf(),
        message: err.to_string(),
    })
}

#[cfg(not(unix))]
fn protect_sensitive_root(_root: &Path, _sensitivity: Sensitivity) -> Result<(), StoreError> {
    Ok(())
}

fn write_generated_gitignore(root: &Path) -> Result<(), StoreError> {
    let path = root.join("generated/.gitignore");
    if path.exists() {
        return Ok(());
    }

    fs::write(&path, GENERATED_GITIGNORE).map_err(|err| StoreError::Io {
        action: "write generated .gitignore",
        path,
        message: err.to_string(),
    })
}

fn write_readme(root: &Path, manifest: &StoreManifest) -> Result<(), StoreError> {
    let path = root.join("README.md");
    if path.exists() {
        return Ok(());
    }

    fs::write(
        &path,
        format!(
            "# {}\n\nHive Memory store. The durable store id is `{}`.\n",
            manifest.store.name, manifest.store.id
        ),
    )
    .map_err(|err| StoreError::Io {
        action: "write store README",
        path,
        message: err.to_string(),
    })
}

fn write_manifest_atomic(root: &Path, manifest: &StoreManifest) -> Result<(), StoreError> {
    let manifest_path = manifest_path(root);
    if manifest_path.exists() {
        return Err(StoreError::ManifestExists(manifest_path));
    }

    let contents = manifest.to_toml_string()?;
    write::write_atomic(
        &manifest_path,
        contents.as_bytes(),
        &AtomicWriteOptions {
            fsync: FsyncPolicy::Required,
            max_attempts: 1,
            skip_parent_fsync: false,
        },
    )
    .map(|_| ())
    .map_err(store_error_from_atomic)
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join("manifest.toml")
}

fn store_error_from_atomic(err: write::AtomicWriteError) -> StoreError {
    match err {
        write::AtomicWriteError::Io {
            action,
            path,
            message,
        } => StoreError::Io {
            action,
            path,
            message,
        },
        other => StoreError::Io {
            action: "write manifest",
            path: PathBuf::from("manifest.toml"),
            message: other.to_string(),
        },
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting is infallible for current UTC time")
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
            "hive-memory-store-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn test_manifest() -> StoreManifest {
        StoreManifest::with_identity(
            "personal",
            Some("Personal memory".to_owned()),
            Sensitivity::Private,
            "018f5f57-bd9b-7d33-9e21-1f44f0c5a013".to_owned(),
            "2026-05-16T00:00:00Z".to_owned(),
        )
    }

    #[test]
    fn new_manifest_uses_v1_defaults() {
        let manifest = test_manifest();

        assert_eq!(manifest.schema_version, SUPPORTED_MANIFEST_SCHEMA_VERSION);
        assert_eq!(manifest.created_by, CREATED_BY);
        assert!(manifest.policies.append_only_inbox);
        assert!(!manifest.policies.allow_direct_curated_edits);
        assert_eq!(manifest.policies.retention.mode, "keep-raw");
        assert!(manifest.capabilities.json_events);
        assert!(manifest.capabilities.local_outbox);
        assert_eq!(manifest.capabilities.compaction, "manual");
    }

    #[test]
    fn manifest_round_trips_as_toml() {
        let manifest = test_manifest();
        let toml = manifest.to_toml_string().expect("serialize manifest");
        let parsed = StoreManifest::from_toml_str(&toml).expect("parse manifest");

        assert!(toml.contains("[store]"));
        assert!(toml.contains("[policies]"));
        assert!(toml.contains("[capabilities]"));
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn rejects_newer_manifest_schema() {
        let mut manifest = test_manifest();
        manifest.schema_version = SUPPORTED_MANIFEST_SCHEMA_VERSION + 1;
        let toml = manifest.to_toml_string().expect("serialize manifest");
        let err = StoreManifest::from_toml_str(&toml).expect_err("manifest fails");

        assert_eq!(
            err,
            StoreError::UnsupportedSchema {
                found: SUPPORTED_MANIFEST_SCHEMA_VERSION + 1,
                supported: SUPPORTED_MANIFEST_SCHEMA_VERSION
            }
        );
    }

    #[test]
    fn init_store_writes_manifest_and_canonical_dirs() {
        let root = temp_dir("init").join("personal");
        let options = StoreInitOptions {
            name: "personal".to_owned(),
            root: root.clone(),
            description: Some("Personal memory".to_owned()),
            sensitivity: Sensitivity::Private,
        };

        let written = init_store(&options).expect("init store");
        let read = read_manifest(&root).expect("read manifest");

        assert_eq!(read, written);
        assert!(root.join("README.md").is_file());
        assert_eq!(
            fs::read_to_string(root.join("generated/.gitignore")).expect("generated gitignore"),
            GENERATED_GITIGNORE
        );
        for relative in CANONICAL_DIRS {
            assert!(root.join(relative).is_dir(), "missing {relative}");
        }
    }

    #[test]
    fn init_store_refuses_existing_manifest() {
        let root = temp_dir("existing");
        let options = StoreInitOptions {
            name: "personal".to_owned(),
            root: root.clone(),
            description: None,
            sensitivity: Sensitivity::Private,
        };
        init_store(&options).expect("first init");

        let err = init_store(&options).expect_err("second init fails");

        assert_eq!(err, StoreError::ManifestExists(root.join("manifest.toml")));
    }

    #[test]
    fn doctor_warns_for_missing_manifest() {
        let root = temp_dir("doctor-missing");
        let config = StoreConfig {
            root: root.clone(),
            expected_id: None,
            description: None,
            sensitivity: Sensitivity::Private,
        };

        let report = doctor_store(StoreDoctorInput {
            name: "personal",
            config: &config,
        });

        assert!(!report.manifest_available);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].level, StoreDoctorLevel::Warning);
        assert!(report.issues[0].message.contains("missing manifest"));
    }

    #[test]
    fn doctor_warns_for_alias_and_sensitivity_drift() {
        let root = temp_dir("doctor-drift");
        let manifest = StoreManifest::with_identity(
            "old-name",
            None,
            Sensitivity::Secret,
            "018f5f57-bd9b-7d33-9e21-1f44f0c5a013".to_owned(),
            "2026-05-16T00:00:00Z".to_owned(),
        );
        create_store_root(&root, Sensitivity::Secret).expect("create root");
        write_manifest_atomic(&root, &manifest).expect("write manifest");
        let config = StoreConfig {
            root: root.clone(),
            expected_id: None,
            description: None,
            sensitivity: Sensitivity::Private,
        };

        let report = doctor_store(StoreDoctorInput {
            name: "personal",
            config: &config,
        });

        assert!(report.manifest_available);
        assert_eq!(report.issues.len(), 2);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.message.contains("manifest store.name"))
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.message.contains("effective=secret"))
        );
    }

    #[test]
    fn doctor_errors_for_expected_id_mismatch() {
        let root = temp_dir("doctor-expected-id");
        let manifest = StoreManifest::with_identity(
            "personal",
            None,
            Sensitivity::Private,
            "018f5f57-bd9b-7d33-9e21-1f44f0c5a013".to_owned(),
            "2026-05-16T00:00:00Z".to_owned(),
        );
        create_store_root(&root, Sensitivity::Private).expect("create root");
        write_manifest_atomic(&root, &manifest).expect("write manifest");
        let config = StoreConfig {
            root,
            expected_id: Some("wrong-id".to_owned()),
            description: None,
            sensitivity: Sensitivity::Private,
        };

        let report = doctor_store(StoreDoctorInput {
            name: "personal",
            config: &config,
        });

        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].level, StoreDoctorLevel::Error);
        assert!(report.issues[0].message.contains("expected_id"));
    }

    #[test]
    fn migrate_v1_reports_no_migrators() {
        let root = temp_dir("migrate");
        let config = StoreConfig {
            root,
            expected_id: None,
            description: None,
            sensitivity: Sensitivity::Private,
        };

        let report = migrate_stores(
            [StoreDoctorInput {
                name: "personal",
                config: &config,
            }],
            true,
        )
        .expect("migration report");

        assert_eq!(report.stores_checked, 1);
        assert_eq!(report.migrations_run, 0);
        assert!(report.dry_run);
    }
}
