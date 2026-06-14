//! Configuration loading and validation for `hm`.
//!
//! The config layer is a public contract for every higher-level workflow:
//! commands and non-interactive agent hooks all need the same store-affinity and
//! privacy decisions. Keep policy here instead of forcing hooks to rediscover
//! context on their own.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration as StdDuration;

const TOP_LEVEL_KEYS: &[&str] = &[
    "schema_version",
    "default_store",
    "data_dir",
    "state_dir",
    "cache_dir",
    "host_id",
    "user_id",
    "stores",
    "storage",
    "defaults",
    "agents",
    "privacy",
    "offline",
    "performance",
    "classifier",
];

const STORE_KEYS: &[&str] = &["root", "expected_id", "description", "sensitivity"];
const STORAGE_KEYS: &[&str] = &["kind", "case_sensitive", "atomic_rename", "fsync"];
const DEFAULTS_KEYS: &[&str] = &[
    "write_scope",
    "search_scopes",
    "context_sources",
    "event_sidecar",
    "hook_context_max_tokens",
    "context_cache_max_age",
    "context_strategy",
];
const AGENT_KEYS: &[&str] = &[
    "default_store",
    "read_stores",
    "write_stores",
    "allow_all_stores",
];
const PRIVACY_KEYS: &[&str] = &[
    "allow_all_stores_flag",
    "secret_refuses_cloud_roots",
    "allow_secret_writes",
    "allow_hook_secret_writes",
];
const OFFLINE_KEYS: &[&str] = &["enabled", "mode", "archive_retention_days"];
const PERFORMANCE_KEYS: &[&str] = &[
    "context_warm_p95_ms",
    "context_cold_p95_ms",
    "context_store_size_target",
];
const CLASSIFIER_KEYS: &[&str] = &[
    "mode",
    "backend",
    "command",
    "model",
    "batch_limit",
    "min_interval",
    "timeout_seconds",
    "apply_confidence",
];
const CLOUD_ROOT_PREFIXES: &[&str] = &[
    "${HOME}/gdrive",
    "${HOME}/Google Drive",
    "${HOME}/Dropbox",
    "${HOME}/iCloud",
    "${HOME}/Library/Mobile Documents",
    "${HOME}/SkyDrive",
    "${HOME}/OneDrive",
    "${HOME}/Sync",
    "${HOME}/syncthing",
];

/// Loaded, validated configuration plus non-fatal warnings.
///
/// Warnings are deliberately returned alongside the usable config so callers can
/// surface typos without blocking forward-compatible files. Fatal policy issues
/// still return [`ConfigError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    /// Fully expanded and validated runtime configuration.
    pub config: Config,
    /// Non-fatal diagnostics, such as unknown keys that serde would ignore.
    pub warnings: Vec<ConfigWarning>,
}

/// Filesystem locations for layered `hm` config files.
///
/// Commands should use this type rather than hardcoding config paths. The
/// default local override convention stays centralized here so launchers, hooks,
/// and CLI commands all load the same files in the same order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    /// Primary durable config file.
    pub main: PathBuf,
    /// Optional machine-local override file. Missing files are ignored.
    pub local_override: Option<PathBuf>,
}

/// Effective `hm` configuration after defaults and validation.
///
/// This is the shape command implementations should consume. It intentionally
/// uses concrete values instead of `Option` for policy defaults so adjacent code
/// can ask what policy is in force rather than reimplementing fallback logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Config schema version accepted by this build.
    pub schema_version: u32,
    /// Global fallback store used when a caller has no stronger affinity.
    pub default_store: String,
    /// Tool-owned durable support data, outside individual memory stores.
    pub data_dir: PathBuf,
    /// Tool-owned mutable state, such as caches that should survive restarts.
    pub state_dir: PathBuf,
    /// Rebuildable cache data. Callers must not treat this as durable memory.
    pub cache_dir: PathBuf,
    /// Stable host identity used in write ids and note metadata.
    pub host_id: String,
    /// Human/user identity used for attribution when available.
    pub user_id: String,
    /// Filesystem/storage behavior that affects write durability.
    pub storage: StorageConfig,
    /// Default command policy for writes, search, context, and hooks.
    pub defaults: DefaultsConfig,
    /// Configured stores keyed by local alias.
    pub stores: BTreeMap<String, StoreConfig>,
    /// Optional per-agent access policy keyed by agent id.
    pub agents: BTreeMap<String, AgentConfig>,
    /// Privacy and secret-handling policy shared by commands and hooks.
    pub privacy: PrivacyConfig,
    /// Offline write and local outbox policy.
    pub offline: OfflineConfig,
    /// Performance budgets used by benchmark and doctor tooling.
    pub performance: PerformanceConfig,
    /// Background LLM classification policy.
    pub classifier: ClassifierConfig,
}

/// Storage behavior from config.
///
/// V1 has only the filesystem backend, but keeping the storage policy explicit
/// lets write commands choose durability behavior without hardcoding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageConfig {
    /// Backend kind. V1 expects `filesystem`.
    pub kind: String,
    /// Case-sensitivity mode: `auto`, `true`, or `false`.
    pub case_sensitive: String,
    /// Atomic rename support mode: `auto`, `true`, or `false`.
    pub atomic_rename: String,
    /// Fsync policy used by canonical writes.
    pub fsync: FsyncMode,
}

/// Config-level fsync policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FsyncMode {
    /// Never ask the filesystem to sync data explicitly.
    Never,
    /// Try to sync, but treat unsupported or failed syncs as warnings.
    #[default]
    BestEffort,
    /// Treat data or directory sync failure as a failed write.
    Required,
}

impl From<FsyncMode> for crate::write::FsyncPolicy {
    fn from(value: FsyncMode) -> Self {
        match value {
            FsyncMode::Never => Self::Never,
            FsyncMode::BestEffort => Self::BestEffort,
            FsyncMode::Required => Self::Required,
        }
    }
}

/// Command defaults from config.
///
/// These values are policy, not CLI presentation. Commands should ask this
/// struct for defaults instead of quietly choosing their own scope/source rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultsConfig {
    /// Default scope for `hm remember` and `hm note`.
    pub write_scope: String,
    /// Default scopes for search commands.
    pub search_scopes: Vec<String>,
    /// Default source classes included in context.
    pub context_sources: Vec<String>,
    /// Whether `hm note` writes a JSON sidecar by default.
    pub event_sidecar: EventSidecarPolicy,
    /// Hook-mode context token budget.
    pub hook_context_max_tokens: u32,
    /// Max age string for hook context cache fallback.
    pub context_cache_max_age: String,
    /// Session-start selection strategy: `recency` (legacy include-all) or
    /// `relevance` (apply the inject classifier). Stored as a string so an
    /// unrecognized value degrades to legacy behavior instead of failing the
    /// hook path; resolved via `inject::Strategy::from_config`.
    pub context_strategy: String,
}

/// Background LLM classification policy.
///
/// `mode = "off"` keeps LLM classification opt-in. Hot paths only use this
/// config for local spawn/stamp decisions; backend probing belongs to the worker
/// process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifierConfig {
    /// `auto` | `on` | `off`.
    pub mode: String,
    /// Optional explicit backend: `claude` | `codex` | `gemini` | `command`.
    pub backend: Option<String>,
    /// Custom argv when `backend = "command"`; prompt is written to stdin.
    pub command: Vec<String>,
    /// Optional model override passed to known backends.
    pub model: Option<String>,
    /// Max records judged per worker run.
    pub batch_limit: u32,
    /// Minimum interval between automatic runs, such as `6h`.
    pub min_interval: String,
    /// Hard timeout per LLM subprocess.
    pub timeout_seconds: u64,
    /// Apply verdicts at/above this confidence: `high` | `medium`.
    pub apply_confidence: String,
}

/// Sidecar policy for note writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventSidecarPolicy {
    /// `hm note` writes only the Markdown note unless the caller opts in.
    Never,
    /// `hm note` writes a paired JSON event unless the caller opts out.
    #[default]
    Always,
}

/// Configuration for a named memory store.
///
/// Store names are local aliases. Once a store exists, its manifest identity is
/// authoritative; `expected_id` lets config bind an alias to that stable store
/// id without making folder names the source of truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreConfig {
    /// Filesystem root for this local store alias.
    pub root: PathBuf,
    /// Optional stable manifest id expected at `root`.
    pub expected_id: Option<String>,
    /// Human-facing description for diagnostics and generated output.
    pub description: Option<String>,
    /// Store sensitivity used by policy checks before reading the manifest.
    pub sensitivity: Sensitivity,
}

/// Sensitivity class used by store-level policy decisions.
///
/// V1 does not encrypt data at rest, so `Secret` is a policy flag that tightens
/// write and location checks rather than a promise of cryptographic protection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sensitivity {
    /// Intended for memory that can be broadly shared.
    Public,
    /// Intended for non-public team or organization context.
    Internal,
    /// Intended for one user's ordinary personal/project memory.
    #[default]
    Private,
    /// Intended for explicitly approved secret material.
    Secret,
}

impl FromStr for Sensitivity {
    type Err = SensitivityParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "public" => Ok(Self::Public),
            "internal" => Ok(Self::Internal),
            "private" => Ok(Self::Private),
            "secret" => Ok(Self::Secret),
            _ => Err(SensitivityParseError {
                value: input.to_owned(),
            }),
        }
    }
}

impl Display for Sensitivity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Private => "private",
            Self::Secret => "secret",
        };
        write!(f, "{value}")
    }
}

/// Error returned when parsing a sensitivity value from CLI or env input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitivityParseError {
    /// Original value that did not match the supported sensitivity vocabulary.
    pub value: String,
}

impl Display for SensitivityParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid sensitivity {}; expected one of public, internal, private, secret",
            self.value
        )
    }
}

impl Error for SensitivityParseError {}

/// Per-agent store policy from config.
///
/// This is configured by agent id, such as `codex` or `claude`, and controls
/// which stores that agent can read from, write to, and use by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// Store this agent should write/read by default.
    pub default_store: String,
    /// Stores this agent may read without requesting broad access.
    pub read_stores: Vec<String>,
    /// Stores this agent may write without requesting broad access.
    pub write_stores: Vec<String>,
    /// Whether explicit all-store operations are allowed for this agent.
    pub allow_all_stores: bool,
}

/// Resolved policy for an agent, including conservative defaults when absent.
///
/// Hooks should use this instead of reading `agents` directly. Missing agent
/// config is not an error in single-store installs; it resolves to the global
/// default store with no broad-store access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveAgentPolicy {
    /// Resolved default store for the requesting agent.
    pub default_store: String,
    /// Resolved readable stores after applying missing-agent defaults.
    pub read_stores: Vec<String>,
    /// Resolved writable stores after applying missing-agent defaults.
    pub write_stores: Vec<String>,
    /// Resolved broad-store permission after applying defaults.
    pub allow_all_stores: bool,
}

/// Privacy controls that affect hook writes and secret stores.
///
/// The defaults are intentionally conservative around secrets while still
/// keeping ordinary single-store setups simple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacyConfig {
    /// Whether CLI users may explicitly opt into all-store operations.
    pub allow_all_stores_flag: bool,
    /// Whether secret stores are rejected under common sync roots by default.
    pub secret_refuses_cloud_roots: bool,
    /// Whether any command may write secret-classified material.
    pub allow_secret_writes: bool,
    /// Whether non-interactive hooks may write secret-classified material.
    pub allow_hook_secret_writes: bool,
}

/// Offline write policy for the local outbox.
///
/// The outbox is a recovery mechanism, not an implicit sync backend. Keeping
/// this policy in config lets users explicitly disable local durability for
/// environments where a missing store should fail immediately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineConfig {
    /// Whether unavailable-store writes may use the local outbox.
    pub enabled: bool,
    /// Operator preference for unavailable-store fallback.
    pub mode: OfflineMode,
    /// Days to retain per-store flush archives before future cleanup.
    pub archive_retention_days: u32,
}

impl OfflineConfig {
    /// Return whether write commands may enqueue when the target store is offline.
    ///
    /// `Always` is still a fallback policy rather than a request to bypass a
    /// reachable store. Normal online writes should publish canonically first
    /// so the outbox does not become a shadow sync queue.
    pub fn write_fallback_enabled(&self) -> bool {
        self.enabled && self.mode != OfflineMode::Never
    }
}

/// Offline fallback mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OfflineMode {
    /// Use the outbox only for recoverable unavailable-store errors.
    #[default]
    Auto,
    /// Prefer the outbox whenever a command elects to use offline fallback.
    Always,
    /// Refuse unavailable-store writes instead of queueing them locally.
    Never,
}

/// V1 performance budget configuration.
///
/// Commands do not branch on these values in the hot path. They are exposed as
/// config so benchmark/doctor tooling and future CI can enforce the same
/// user-visible latency contract that the spec documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerformanceConfig {
    /// Warm-cache p95 budget for `hm context`, in milliseconds.
    pub context_warm_p95_ms: u32,
    /// Cold-cache p95 budget for `hm context`, in milliseconds.
    pub context_cold_p95_ms: u32,
    /// Synthetic note count used to calibrate the context budget.
    pub context_store_size_target: u32,
}

/// Non-fatal configuration diagnostics.
///
/// These are warnings rather than errors so v1 can tolerate future config keys
/// while still catching local typos during `hm doctor` or startup reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigWarning {
    /// Unknown key directly under the config root table.
    UnknownTopLevelKey(String),
    /// Unknown key under a known nested config table.
    UnknownSubkey(String),
}

impl Display for ConfigWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTopLevelKey(key) => write!(f, "unknown config key: {key}"),
            Self::UnknownSubkey(key) => write!(f, "unknown config key: {key}"),
        }
    }
}

/// Fatal configuration failures.
///
/// Errors here mean callers should not proceed with commands that rely on the
/// config, because continuing would use an ambiguous store or unsafe secret
/// policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// TOML parsing or deserialization failed.
    Parse(String),
    /// Default config path could not be resolved because `HOME` is unset.
    HomeNotSet,
    /// Required config file could not be read.
    ReadConfig {
        /// Config path that failed to read.
        path: PathBuf,
        /// Original I/O error rendered for CLI diagnostics.
        message: String,
    },
    /// `default_store` was omitted.
    MissingDefaultStore,
    /// A configured store omitted `root`.
    MissingStoreRoot {
        /// Store alias with the missing root.
        store: String,
    },
    /// Store alias violates the v1 name grammar.
    InvalidStoreName(String),
    /// `default_store` references no configured store.
    UnknownDefaultStore(String),
    /// Agent policy references no configured store.
    UnknownAgentStore {
        /// Agent id owning the bad reference.
        agent: String,
        /// Missing store alias.
        store: String,
    },
    /// Secret store root points under a common cloud-sync location.
    SecretStoreOnCloudRoot {
        /// Store alias rejected by policy.
        store: String,
        /// Expanded root path that matched a sync prefix.
        root: PathBuf,
    },
    /// Hook secret writes cannot be broader than normal secret writes.
    HookSecretWritesRequireSecretWrites,
    /// A config key carried a value outside its allowed set.
    InvalidValue {
        /// Dotted key, e.g. `classifier.mode`.
        key: String,
        /// The rejected value.
        value: String,
        /// Allowed values, for the error message.
        allowed: &'static [&'static str],
    },
    /// A `${...` path expansion was missing its closing brace.
    UnterminatedExpansion(String),
}

impl Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "failed to parse config: {message}"),
            Self::HomeNotSet => write!(f, "HOME is not set; cannot find default config path"),
            Self::ReadConfig { path, message } => {
                write!(f, "failed to read config {}: {message}", path.display())
            }
            Self::MissingDefaultStore => write!(f, "default_store is required"),
            Self::MissingStoreRoot { store } => {
                write!(f, "stores.{store}.root is required")
            }
            Self::InvalidStoreName(name) => write!(f, "invalid store name: {name}"),
            Self::UnknownDefaultStore(name) => {
                write!(f, "default_store references unknown store: {name}")
            }
            Self::UnknownAgentStore { agent, store } => {
                write!(f, "agents.{agent} references unknown store: {store}")
            }
            Self::SecretStoreOnCloudRoot { store, root } => write!(
                f,
                "secret store {store} must not use cloud-synced root {}",
                root.display()
            ),
            Self::HookSecretWritesRequireSecretWrites => write!(
                f,
                "privacy.allow_hook_secret_writes requires privacy.allow_secret_writes"
            ),
            Self::InvalidValue {
                key,
                value,
                allowed,
            } => write!(
                f,
                "invalid config value for {key}: {value}; expected one of: {}",
                allowed.join(", ")
            ),
            Self::UnterminatedExpansion(input) => {
                write!(f, "unterminated variable expansion in {input}")
            }
        }
    }
}

impl Error for ConfigError {}

impl ConfigPaths {
    /// Build config paths from an explicit main config path.
    ///
    /// The local override is always `config.local.toml` beside the selected
    /// main file. That keeps custom test/config roots predictable and matches
    /// the default `~/.config/hive-memory` layout.
    pub fn from_main_path(main: impl Into<PathBuf>) -> Self {
        let main = main.into();
        let local_override = main.parent().map(|parent| parent.join("config.local.toml"));
        Self {
            main,
            local_override,
        }
    }

    /// Resolve config paths using CLI override, env override, then defaults.
    ///
    /// `--config` should pass `explicit_main`; otherwise `HIVE_MEMORY_CONFIG`
    /// wins over the standard `~/.config/hive-memory/config.toml` path.
    pub fn resolve(explicit_main: Option<&Path>) -> Result<Self, ConfigError> {
        if let Some(path) = explicit_main {
            return Ok(Self::from_main_path(path));
        }

        if let Some(path) = std::env::var_os("HIVE_MEMORY_CONFIG") {
            return Ok(Self::from_main_path(path));
        }

        let Some(home) = std::env::var_os("HOME") else {
            return Err(ConfigError::HomeNotSet);
        };
        Ok(Self::from_main_path(
            PathBuf::from(home).join(".config/hive-memory/config.toml"),
        ))
    }

    /// Load and validate the config described by these paths.
    pub fn load(&self) -> Result<LoadedConfig, ConfigError> {
        LoadedConfig::from_files(&self.main, self.local_override.as_deref())
    }
}

impl LoadedConfig {
    /// Parse, expand, and validate one TOML config document using process env.
    ///
    /// This is the public entry point for normal CLI loading once file layering
    /// has produced a single TOML document. It applies built-in defaults,
    /// expands supported path variables, validates cross-references, and returns
    /// warnings for unknown keys.
    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        Self::from_str_with_env(input, |name| std::env::var(name).ok())
    }

    /// Parse and validate main config plus an optional local override document.
    ///
    /// Local override values replace scalars and arrays while recursively
    /// merging tables. This mirrors the user-facing contract: durable shared
    /// defaults live in `config.toml`, and machine/private adjustments live in
    /// `config.local.toml` without forcing users to duplicate the whole file.
    pub fn from_toml_layers(main: &str, local_override: Option<&str>) -> Result<Self, ConfigError> {
        Self::from_toml_layers_with_env(main, local_override, |name| std::env::var(name).ok())
    }

    /// Parse and validate main config plus local override with injected env.
    ///
    /// This is the deterministic test seam for file layering. It intentionally
    /// keeps env/CLI override policy out of the file merge path; those dynamic
    /// overrides should be applied by command loading code before validation.
    pub fn from_toml_layers_with_env<F>(
        main: &str,
        local_override: Option<&str>,
        env: F,
    ) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut merged = parse_table(main)?;
        if let Some(local_override) = local_override {
            let local = parse_table(local_override)?;
            merge_tables(&mut merged, local);
        }

        Self::from_table_with_env(merged, env)
    }

    /// Load, merge, and validate config files from explicit paths.
    ///
    /// A missing local override file is ignored because it is an optional
    /// machine-local layer. The primary config file is required; if it is
    /// absent, callers should surface the read error and guide the user toward
    /// `hm init` or config creation.
    pub fn from_files(main: &Path, local_override: Option<&Path>) -> Result<Self, ConfigError> {
        Self::from_files_with_env(main, local_override, |name| std::env::var(name).ok())
    }

    /// Load, merge, and validate config files with injected env.
    ///
    /// This is the file-backed equivalent of
    /// [`LoadedConfig::from_toml_layers_with_env`] and exists so tests can
    /// exercise path expansion without mutating process-wide environment.
    pub fn from_files_with_env<F>(
        main: &Path,
        local_override: Option<&Path>,
        env: F,
    ) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let main_toml = read_required_config(main)?;
        let local_toml = read_optional_config(local_override)?;
        Self::from_toml_layers_with_env(&main_toml, local_toml.as_deref(), env)
    }

    /// Parse, expand, and validate one TOML config document with injected env.
    ///
    /// This exists primarily for deterministic tests and future callers that
    /// need to evaluate config in a controlled environment. Production code
    /// should usually call [`LoadedConfig::from_toml_str`].
    pub fn from_str_with_env<F>(input: &str, env: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let table = parse_table(input)?;
        Self::from_table_with_env(table, env)
    }

    fn from_table_with_env<F>(table: toml::Table, env: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let warnings = collect_warnings(&table);
        let value = toml::Value::Table(table);
        let raw = value
            .try_into()
            .map_err(|err: toml::de::Error| ConfigError::Parse(err.to_string()))?;
        let config = Config::from_raw(raw, &env)?;

        Ok(Self { config, warnings })
    }
}

fn read_required_config(path: &Path) -> Result<String, ConfigError> {
    fs::read_to_string(path).map_err(|err| ConfigError::ReadConfig {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

fn read_optional_config(path: Option<&Path>) -> Result<Option<String>, ConfigError> {
    let Some(path) = path else {
        return Ok(None);
    };

    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ConfigError::ReadConfig {
            path: path.to_path_buf(),
            message: err.to_string(),
        }),
    }
}

fn parse_table(input: &str) -> Result<toml::Table, ConfigError> {
    toml::from_str::<toml::Table>(input).map_err(|err| ConfigError::Parse(err.to_string()))
}

fn merge_tables(base: &mut toml::Table, override_table: toml::Table) {
    for (key, override_value) in override_table {
        match (base.get_mut(&key), override_value) {
            (Some(toml::Value::Table(base_table)), toml::Value::Table(override_table)) => {
                // Tables merge recursively so a local file can override one
                // nested store root without erasing sibling stores or policy.
                merge_tables(base_table, override_table);
            }
            (_, override_value) => {
                // Scalars and arrays replace by design. Appending arrays would
                // make policy allowlists hard to reason about across layers.
                base.insert(key, override_value);
            }
        }
    }
}

impl Config {
    /// Return the effective store policy for an agent id.
    ///
    /// Agent hooks should use this API rather than inspecting `Config::agents`
    /// directly, because this function centralizes the conservative
    /// missing-agent behavior required by the spec.
    pub fn effective_agent_policy(&self, agent: &str) -> EffectiveAgentPolicy {
        self.agents
            .get(agent)
            .map(|agent| EffectiveAgentPolicy {
                default_store: agent.default_store.clone(),
                read_stores: agent.read_stores.clone(),
                write_stores: agent.write_stores.clone(),
                allow_all_stores: agent.allow_all_stores,
            })
            .unwrap_or_else(|| EffectiveAgentPolicy {
                // Hooks should have a deterministic safe policy even before a
                // user adds per-agent config, especially in single-store setups.
                default_store: self.default_store.clone(),
                read_stores: vec![self.default_store.clone()],
                write_stores: vec![self.default_store.clone()],
                allow_all_stores: false,
            })
    }

    /// Parsed `[classifier] min_interval`, for local stamp-age checks.
    pub fn classifier_min_interval(&self) -> StdDuration {
        parse_duration_std(&self.classifier.min_interval).expect("validated at load")
    }

    fn from_raw<F>(raw: RawConfig, env: &F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let default_store = raw.default_store.ok_or(ConfigError::MissingDefaultStore)?;
        let privacy = PrivacyConfig::from_raw(raw.privacy);
        if privacy.allow_hook_secret_writes && !privacy.allow_secret_writes {
            return Err(ConfigError::HookSecretWritesRequireSecretWrites);
        }

        let stores = raw
            .stores
            .into_iter()
            .map(|(name, store)| {
                if !valid_store_name(&name) {
                    return Err(ConfigError::InvalidStoreName(name));
                }

                let root = store
                    .root
                    .ok_or_else(|| ConfigError::MissingStoreRoot {
                        store: name.clone(),
                    })
                    .and_then(|path| expand_path(&path, env))?;

                Ok((
                    name,
                    StoreConfig {
                        root,
                        expected_id: store.expected_id,
                        description: store.description,
                        sensitivity: store.sensitivity.unwrap_or_default(),
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        if !stores.contains_key(&default_store) {
            return Err(ConfigError::UnknownDefaultStore(default_store));
        }

        if privacy.secret_refuses_cloud_roots {
            for (name, store) in &stores {
                if store.sensitivity == Sensitivity::Secret && cloud_root(&store.root, env)? {
                    // A secret store needs an explicit non-synced location by
                    // default; encrypted-at-rest storage is deferred for v1.
                    return Err(ConfigError::SecretStoreOnCloudRoot {
                        store: name.clone(),
                        root: store.root.clone(),
                    });
                }
            }
        }

        let agents = raw
            .agents
            .into_iter()
            .map(|(name, agent)| {
                let agent = AgentConfig::from_raw(agent, &default_store);
                validate_store_refs(agent.stores(), &stores, |store| {
                    ConfigError::UnknownAgentStore {
                        agent: name.clone(),
                        store: store.to_owned(),
                    }
                })?;
                Ok((name, agent))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        Ok(Self {
            schema_version: raw.schema_version.unwrap_or(1),
            default_store,
            data_dir: expand_path(
                raw.data_dir
                    .as_deref()
                    .unwrap_or("${XDG_DATA_HOME:-${HOME}/.local/share}/hive-memory"),
                env,
            )?,
            state_dir: expand_path(
                raw.state_dir
                    .as_deref()
                    .unwrap_or("${XDG_STATE_HOME:-${HOME}/.local/state}/hive-memory"),
                env,
            )?,
            cache_dir: expand_path(
                raw.cache_dir
                    .as_deref()
                    .unwrap_or("${XDG_CACHE_HOME:-${HOME}/.cache}/hive-memory"),
                env,
            )?,
            host_id: raw.host_id.unwrap_or_else(|| "auto".to_owned()),
            user_id: raw.user_id.unwrap_or_else(|| "default".to_owned()),
            storage: StorageConfig::from_raw(raw.storage),
            defaults: DefaultsConfig::from_raw(raw.defaults),
            stores,
            agents,
            privacy,
            offline: OfflineConfig::from_raw(raw.offline),
            performance: PerformanceConfig::from_raw(raw.performance),
            classifier: ClassifierConfig::from_raw(raw.classifier)?,
        })
    }
}

impl StorageConfig {
    fn from_raw(raw: RawStorageConfig) -> Self {
        Self {
            kind: raw.kind.unwrap_or_else(|| "filesystem".to_owned()),
            case_sensitive: raw.case_sensitive.unwrap_or_else(|| "auto".to_owned()),
            atomic_rename: raw.atomic_rename.unwrap_or_else(|| "auto".to_owned()),
            fsync: raw.fsync.unwrap_or_default(),
        }
    }
}

impl DefaultsConfig {
    fn from_raw(raw: RawDefaultsConfig) -> Self {
        Self {
            write_scope: raw.write_scope.unwrap_or_else(|| "global".to_owned()),
            search_scopes: raw
                .search_scopes
                .unwrap_or_else(|| vec!["global".to_owned(), "project".to_owned()]),
            context_sources: raw
                .context_sources
                .unwrap_or_else(|| vec!["curated".to_owned(), "remembered".to_owned()]),
            event_sidecar: raw.event_sidecar.unwrap_or_default(),
            hook_context_max_tokens: raw.hook_context_max_tokens.unwrap_or(4000),
            context_cache_max_age: raw.context_cache_max_age.unwrap_or_else(|| "7d".to_owned()),
            context_strategy: raw
                .context_strategy
                .unwrap_or_else(|| "adaptive".to_owned()),
        }
    }
}

impl AgentConfig {
    fn from_raw(raw: RawAgentConfig, global_default: &str) -> Self {
        let default_store = raw
            .default_store
            .unwrap_or_else(|| global_default.to_owned());
        Self {
            read_stores: raw
                .read_stores
                .unwrap_or_else(|| vec![default_store.clone()]),
            write_stores: raw
                .write_stores
                .unwrap_or_else(|| vec![default_store.clone()]),
            allow_all_stores: raw.allow_all_stores.unwrap_or(false),
            default_store,
        }
    }

    fn stores(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.default_store.as_str())
            .chain(self.read_stores.iter().map(String::as_str))
            .chain(self.write_stores.iter().map(String::as_str))
    }
}

impl PrivacyConfig {
    fn from_raw(raw: RawPrivacyConfig) -> Self {
        Self {
            allow_all_stores_flag: raw.allow_all_stores_flag.unwrap_or(true),
            secret_refuses_cloud_roots: raw.secret_refuses_cloud_roots.unwrap_or(true),
            allow_secret_writes: raw.allow_secret_writes.unwrap_or(false),
            allow_hook_secret_writes: raw.allow_hook_secret_writes.unwrap_or(false),
        }
    }
}

impl OfflineConfig {
    fn from_raw(raw: RawOfflineConfig) -> Self {
        Self {
            enabled: raw.enabled.unwrap_or(true),
            mode: raw.mode.unwrap_or_default(),
            archive_retention_days: raw.archive_retention_days.unwrap_or(30),
        }
    }
}

impl PerformanceConfig {
    fn from_raw(raw: RawPerformanceConfig) -> Self {
        Self {
            context_warm_p95_ms: raw.context_warm_p95_ms.unwrap_or(200),
            context_cold_p95_ms: raw.context_cold_p95_ms.unwrap_or(500),
            context_store_size_target: raw.context_store_size_target.unwrap_or(5000),
        }
    }
}

impl ClassifierConfig {
    fn from_raw(raw: RawClassifierConfig) -> Result<Self, ConfigError> {
        const MODES: &[&str] = &["auto", "on", "off"];
        const BACKENDS: &[&str] = &["claude", "codex", "gemini", "command"];
        const CONFIDENCE: &[&str] = &["high", "medium"];

        let mode = raw.mode.unwrap_or_else(|| "off".to_owned());
        validate_allowed("classifier.mode", &mode, MODES)?;

        if let Some(backend) = raw.backend.as_deref() {
            validate_allowed("classifier.backend", backend, BACKENDS)?;
        }

        let command = raw.command.unwrap_or_default();
        if raw.backend.as_deref() == Some("command") && command.is_empty() {
            return Err(ConfigError::InvalidValue {
                key: "classifier.command".to_owned(),
                value: "[]".to_owned(),
                allowed: &["non-empty argv when backend = \"command\""],
            });
        }

        let batch_limit = raw.batch_limit.unwrap_or(25);
        if batch_limit == 0 {
            return Err(ConfigError::InvalidValue {
                key: "classifier.batch_limit".to_owned(),
                value: batch_limit.to_string(),
                allowed: &["positive integer"],
            });
        }

        let min_interval = raw.min_interval.unwrap_or_else(|| "6h".to_owned());
        if parse_duration_std(&min_interval).is_none() {
            return Err(ConfigError::InvalidValue {
                key: "classifier.min_interval".to_owned(),
                value: min_interval,
                allowed: &["duration like 6h, 30m, 10s, or 1d"],
            });
        }

        let timeout_seconds = raw.timeout_seconds.unwrap_or(60);
        if timeout_seconds == 0 {
            return Err(ConfigError::InvalidValue {
                key: "classifier.timeout_seconds".to_owned(),
                value: timeout_seconds.to_string(),
                allowed: &["positive integer"],
            });
        }

        let apply_confidence = raw.apply_confidence.unwrap_or_else(|| "high".to_owned());
        validate_allowed("classifier.apply_confidence", &apply_confidence, CONFIDENCE)?;

        Ok(Self {
            mode,
            backend: raw.backend,
            command,
            model: raw.model,
            batch_limit,
            min_interval,
            timeout_seconds,
            apply_confidence,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawConfig {
    schema_version: Option<u32>,
    default_store: Option<String>,
    data_dir: Option<String>,
    state_dir: Option<String>,
    cache_dir: Option<String>,
    host_id: Option<String>,
    user_id: Option<String>,
    stores: BTreeMap<String, RawStoreConfig>,
    storage: RawStorageConfig,
    defaults: RawDefaultsConfig,
    agents: BTreeMap<String, RawAgentConfig>,
    privacy: RawPrivacyConfig,
    offline: RawOfflineConfig,
    performance: RawPerformanceConfig,
    classifier: RawClassifierConfig,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            schema_version: Some(1),
            default_store: None,
            data_dir: None,
            state_dir: None,
            cache_dir: None,
            host_id: None,
            user_id: None,
            stores: BTreeMap::new(),
            storage: RawStorageConfig::default(),
            defaults: RawDefaultsConfig::default(),
            agents: BTreeMap::new(),
            privacy: RawPrivacyConfig::default(),
            offline: RawOfflineConfig::default(),
            performance: RawPerformanceConfig::default(),
            classifier: RawClassifierConfig::default(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawStoreConfig {
    root: Option<String>,
    expected_id: Option<String>,
    description: Option<String>,
    sensitivity: Option<Sensitivity>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawStorageConfig {
    kind: Option<String>,
    case_sensitive: Option<String>,
    atomic_rename: Option<String>,
    fsync: Option<FsyncMode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawDefaultsConfig {
    write_scope: Option<String>,
    search_scopes: Option<Vec<String>>,
    context_sources: Option<Vec<String>>,
    event_sidecar: Option<EventSidecarPolicy>,
    hook_context_max_tokens: Option<u32>,
    context_cache_max_age: Option<String>,
    context_strategy: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawAgentConfig {
    default_store: Option<String>,
    read_stores: Option<Vec<String>>,
    write_stores: Option<Vec<String>>,
    allow_all_stores: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPrivacyConfig {
    allow_all_stores_flag: Option<bool>,
    secret_refuses_cloud_roots: Option<bool>,
    allow_secret_writes: Option<bool>,
    allow_hook_secret_writes: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawOfflineConfig {
    enabled: Option<bool>,
    mode: Option<OfflineMode>,
    archive_retention_days: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPerformanceConfig {
    context_warm_p95_ms: Option<u32>,
    context_cold_p95_ms: Option<u32>,
    context_store_size_target: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawClassifierConfig {
    mode: Option<String>,
    backend: Option<String>,
    command: Option<Vec<String>>,
    model: Option<String>,
    batch_limit: Option<u32>,
    min_interval: Option<String>,
    timeout_seconds: Option<u64>,
    apply_confidence: Option<String>,
}

fn collect_warnings(table: &toml::Table) -> Vec<ConfigWarning> {
    let allowed = TOP_LEVEL_KEYS.iter().copied().collect::<BTreeSet<_>>();
    let mut warnings = Vec::new();

    warnings.extend(
        table
            .keys()
            .filter(|key| !allowed.contains(key.as_str()))
            .map(|key| ConfigWarning::UnknownTopLevelKey(key.clone())),
    );

    collect_dynamic_table_warnings(&mut warnings, table, "stores", STORE_KEYS);
    collect_table_warnings(&mut warnings, table, "storage", STORAGE_KEYS);
    collect_table_warnings(&mut warnings, table, "defaults", DEFAULTS_KEYS);
    collect_dynamic_table_warnings(&mut warnings, table, "agents", AGENT_KEYS);
    collect_table_warnings(&mut warnings, table, "privacy", PRIVACY_KEYS);
    collect_table_warnings(&mut warnings, table, "offline", OFFLINE_KEYS);
    collect_table_warnings(&mut warnings, table, "performance", PERFORMANCE_KEYS);
    collect_table_warnings(&mut warnings, table, "classifier", CLASSIFIER_KEYS);

    warnings
}

fn validate_allowed(
    key: &'static str,
    value: &str,
    allowed: &'static [&'static str],
) -> Result<(), ConfigError> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(ConfigError::InvalidValue {
            key: key.to_owned(),
            value: value.to_owned(),
            allowed,
        })
    }
}

/// Parse compact config durations such as `10m`, `2h`, or `1d`.
///
/// Keep this grammar intentionally tiny and explicit. Hook fallback policy and
/// classifier retry cadence should be auditable from config, not dependent on a
/// permissive parser whose accepted syntax changes under us.
pub fn parse_duration_time(input: &str) -> Option<time::Duration> {
    let trimmed = input.trim();
    let unit = trimmed.chars().last()?;
    let number = trimmed[..trimmed.len().saturating_sub(unit.len_utf8())]
        .parse::<i64>()
        .ok()?;
    if number < 0 {
        return None;
    }
    match unit {
        'd' => Some(time::Duration::days(number)),
        'h' => Some(time::Duration::hours(number)),
        'm' => Some(time::Duration::minutes(number)),
        's' => Some(time::Duration::seconds(number)),
        _ => None,
    }
}

/// Parse compact config durations into std durations for filesystem stamps.
pub fn parse_duration_std(input: &str) -> Option<StdDuration> {
    let duration = parse_duration_time(input)?;
    StdDuration::try_from(duration).ok()
}

fn collect_table_warnings(
    warnings: &mut Vec<ConfigWarning>,
    table: &toml::map::Map<String, toml::Value>,
    name: &str,
    allowed_keys: &[&str],
) {
    let Some(section) = table.get(name).and_then(toml::Value::as_table) else {
        return;
    };
    collect_unknown_keys(warnings, name, section, allowed_keys);
}

fn collect_dynamic_table_warnings(
    warnings: &mut Vec<ConfigWarning>,
    table: &toml::map::Map<String, toml::Value>,
    name: &str,
    allowed_keys: &[&str],
) {
    let Some(section) = table.get(name).and_then(toml::Value::as_table) else {
        return;
    };

    for (entry_name, entry) in section {
        let Some(entry_table) = entry.as_table() else {
            continue;
        };
        collect_unknown_keys(
            warnings,
            &format!("{name}.{entry_name}"),
            entry_table,
            allowed_keys,
        );
    }
}

fn collect_unknown_keys(
    warnings: &mut Vec<ConfigWarning>,
    path: &str,
    table: &toml::map::Map<String, toml::Value>,
    allowed_keys: &[&str],
) {
    let allowed = allowed_keys.iter().copied().collect::<BTreeSet<_>>();
    warnings.extend(
        table
            .keys()
            .filter(|key| !allowed.contains(key.as_str()))
            .map(|key| ConfigWarning::UnknownSubkey(format!("{path}.{key}"))),
    );
}

fn validate_store_refs<'a, I, F>(
    stores_to_check: I,
    stores: &BTreeMap<String, StoreConfig>,
    error: F,
) -> Result<(), ConfigError>
where
    I: Iterator<Item = &'a str>,
    F: Fn(&str) -> ConfigError,
{
    for store in stores_to_check {
        if !stores.contains_key(store) {
            return Err(error(store));
        }
    }
    Ok(())
}

fn valid_store_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn cloud_root<F>(root: &Path, env: &F) -> Result<bool, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    for prefix in CLOUD_ROOT_PREFIXES {
        let expanded = expand_path(prefix, env)?;
        if root.starts_with(expanded) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn expand_path<F>(input: &str, env: &F) -> Result<PathBuf, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    expand_vars(input, env).map(PathBuf::from)
}

fn expand_vars<F>(input: &str, env: &F) -> Result<String, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let mut output = input.to_owned();
    while let Some(start) = output.rfind("${") {
        let Some(relative_end) = output[start..].find('}') else {
            return Err(ConfigError::UnterminatedExpansion(input.to_owned()));
        };
        let end = start + relative_end;
        let expr = &output[start + 2..end];
        let value = expand_expr(expr, env);
        output.replace_range(start..=end, &value);
    }
    Ok(output)
}

fn expand_expr<F>(expr: &str, env: &F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    if let Some((name, fallback)) = expr.split_once(":-") {
        env(name)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback.to_owned())
    } else {
        env(expr).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn env(name: &str) -> Option<String> {
        match name {
            "HOME" => Some("/home/tester".to_owned()),
            "XDG_DATA_HOME" => None,
            "XDG_STATE_HOME" => None,
            "XDG_CACHE_HOME" => Some("/tmp/cache".to_owned()),
            _ => None,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hive-memory-config-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn loads_minimal_config_with_defaults() {
        let loaded = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "${HOME}/hive-memory/personal"
            "#,
            env,
        )
        .expect("config loads");

        assert_eq!(loaded.config.schema_version, 1);
        assert_eq!(loaded.config.default_store, "personal");
        assert_eq!(
            loaded.config.stores["personal"].root,
            PathBuf::from("/home/tester/hive-memory/personal")
        );
        assert_eq!(
            loaded.config.data_dir,
            PathBuf::from("/home/tester/.local/share/hive-memory")
        );
        assert_eq!(
            loaded.config.cache_dir,
            PathBuf::from("/tmp/cache/hive-memory")
        );
        assert_eq!(loaded.config.storage.kind, "filesystem");
        assert_eq!(loaded.config.storage.fsync, FsyncMode::BestEffort);
        assert_eq!(loaded.config.defaults.write_scope, "global");
        assert_eq!(
            loaded.config.defaults.search_scopes,
            vec!["global", "project"]
        );
        assert_eq!(
            loaded.config.defaults.event_sidecar,
            EventSidecarPolicy::Always
        );
        // Unset strategy defaults to the recall-safe Adaptive selector: it only
        // withholds explicitly non-startup kinds and never drops untagged
        // content, so the default can raise precision without regressing recall.
        assert_eq!(loaded.config.defaults.context_strategy, "adaptive");
        assert_eq!(
            loaded.config.offline,
            OfflineConfig {
                enabled: true,
                mode: OfflineMode::Auto,
                archive_retention_days: 30,
            }
        );
        assert!(loaded.config.offline.write_fallback_enabled());
        assert_eq!(
            loaded.config.performance,
            PerformanceConfig {
                context_warm_p95_ms: 200,
                context_cold_p95_ms: 500,
                context_store_size_target: 5000,
            }
        );
        assert_eq!(
            loaded.config.classifier,
            ClassifierConfig {
                mode: "off".to_owned(),
                backend: None,
                command: Vec::new(),
                model: None,
                batch_limit: 25,
                min_interval: "6h".to_owned(),
                timeout_seconds: 60,
                apply_confidence: "high".to_owned(),
            }
        );
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn loads_storage_and_command_defaults() {
        let loaded = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"

            [storage]
            fsync = "required"

            [defaults]
            write_scope = "project"
            search_scopes = ["project"]
            context_sources = ["remembered"]
            event_sidecar = "never"
            hook_context_max_tokens = 1234
            context_cache_max_age = "2d"
            context_strategy = "relevance"

            [offline]
            enabled = false
            mode = "never"
            archive_retention_days = 14

            [performance]
            context_warm_p95_ms = 111
            context_cold_p95_ms = 333
            context_store_size_target = 42

            [classifier]
            mode = "on"
            backend = "claude"
            model = "claude-haiku-4-5-20251001"
            batch_limit = 5
            min_interval = "12h"
            timeout_seconds = 30
            apply_confidence = "medium"
            "#,
            env,
        )
        .expect("config loads");

        assert_eq!(loaded.config.storage.fsync, FsyncMode::Required);
        assert_eq!(loaded.config.defaults.write_scope, "project");
        assert_eq!(loaded.config.defaults.search_scopes, vec!["project"]);
        assert_eq!(loaded.config.defaults.context_sources, vec!["remembered"]);
        assert_eq!(
            loaded.config.defaults.event_sidecar,
            EventSidecarPolicy::Never
        );
        assert_eq!(loaded.config.defaults.hook_context_max_tokens, 1234);
        assert_eq!(loaded.config.defaults.context_cache_max_age, "2d");
        assert_eq!(loaded.config.defaults.context_strategy, "relevance");
        assert_eq!(
            loaded.config.offline,
            OfflineConfig {
                enabled: false,
                mode: OfflineMode::Never,
                archive_retention_days: 14,
            }
        );
        assert!(!loaded.config.offline.write_fallback_enabled());
        assert_eq!(
            loaded.config.performance,
            PerformanceConfig {
                context_warm_p95_ms: 111,
                context_cold_p95_ms: 333,
                context_store_size_target: 42,
            }
        );
        assert_eq!(
            loaded.config.classifier,
            ClassifierConfig {
                mode: "on".to_owned(),
                backend: Some("claude".to_owned()),
                command: Vec::new(),
                model: Some("claude-haiku-4-5-20251001".to_owned()),
                batch_limit: 5,
                min_interval: "12h".to_owned(),
                timeout_seconds: 30,
                apply_confidence: "medium".to_owned(),
            }
        );
    }

    #[test]
    fn local_override_replaces_scalars_and_merges_tables() {
        let loaded = LoadedConfig::from_toml_layers_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"
            description = "shared"

            [stores.work]
            root = "/tmp/work"
            "#,
            Some(
                r#"
                default_store = "work"

                [stores.personal]
                root = "/private/personal"
                "#,
            ),
            env,
        )
        .expect("config loads");

        assert_eq!(loaded.config.default_store, "work");
        assert_eq!(
            loaded.config.stores["personal"].root,
            PathBuf::from("/private/personal")
        );
        assert_eq!(
            loaded.config.stores["personal"].description.as_deref(),
            Some("shared")
        );
        assert!(loaded.config.stores.contains_key("work"));
    }

    #[test]
    fn local_override_replaces_arrays_instead_of_appending() {
        let loaded = LoadedConfig::from_toml_layers_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"

            [stores.work]
            root = "/tmp/work"

            [agents.codex]
            default_store = "personal"
            read_stores = ["personal", "work"]
            write_stores = ["personal"]
            "#,
            Some(
                r#"
                [agents.codex]
                read_stores = ["work"]
                "#,
            ),
            env,
        )
        .expect("config loads");

        assert_eq!(loaded.config.agents["codex"].read_stores, vec!["work"]);
        assert_eq!(loaded.config.agents["codex"].write_stores, vec!["personal"]);
    }

    #[test]
    fn file_loader_ignores_missing_local_override() {
        let dir = temp_dir("missing-local");
        let main = dir.join("config.toml");
        let local = dir.join("config.local.toml");
        fs::write(
            &main,
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"
            "#,
        )
        .expect("write main config");

        let loaded =
            LoadedConfig::from_files_with_env(&main, Some(&local), env).expect("config loads");

        assert_eq!(loaded.config.default_store, "personal");
    }

    #[test]
    fn file_loader_requires_main_config() {
        let dir = temp_dir("missing-main");
        let main = dir.join("config.toml");
        let err = LoadedConfig::from_files_with_env(&main, None, env).expect_err("config fails");

        assert!(matches!(err, ConfigError::ReadConfig { path, .. } if path == main));
    }

    #[test]
    fn expands_fallback_when_env_value_is_empty() {
        fn empty_xdg_env(name: &str) -> Option<String> {
            match name {
                "HOME" => Some("/home/tester".to_owned()),
                "XDG_DATA_HOME" => Some(String::new()),
                _ => None,
            }
        }

        let loaded = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"
            "#,
            empty_xdg_env,
        )
        .expect("config loads");

        assert_eq!(
            loaded.config.data_dir,
            PathBuf::from("/home/tester/.local/share/hive-memory")
        );
    }

    #[test]
    fn rejects_missing_default_store() {
        let err = LoadedConfig::from_str_with_env("", env).expect_err("config fails");

        assert_eq!(err, ConfigError::MissingDefaultStore);
    }

    #[test]
    fn rejects_unknown_default_store() {
        let err = LoadedConfig::from_str_with_env(
            r#"
            default_store = "work"

            [stores.personal]
            root = "/tmp/personal"
            "#,
            env,
        )
        .expect_err("config fails");

        assert_eq!(err, ConfigError::UnknownDefaultStore("work".to_owned()));
    }

    #[test]
    fn rejects_invalid_store_name() {
        let err = LoadedConfig::from_str_with_env(
            r#"
            default_store = "Bad"

            [stores.Bad]
            root = "/tmp/bad"
            "#,
            env,
        )
        .expect_err("config fails");

        assert_eq!(err, ConfigError::InvalidStoreName("Bad".to_owned()));
    }

    #[test]
    fn classifier_unknown_key_warns() {
        let loaded = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"

            [classifier]
            bogus = true
            "#,
            env,
        )
        .expect("config loads");

        assert_eq!(
            loaded.warnings,
            vec![ConfigWarning::UnknownSubkey("classifier.bogus".to_owned())]
        );
    }

    #[test]
    fn classifier_invalid_mode_errors() {
        let err = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"

            [classifier]
            mode = "sometimes"
            "#,
            env,
        )
        .expect_err("config fails");

        assert!(matches!(
            err,
            ConfigError::InvalidValue { key, value, .. }
                if key == "classifier.mode" && value == "sometimes"
        ));
    }

    #[test]
    fn classifier_command_backend_requires_command() {
        let err = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"

            [classifier]
            backend = "command"
            "#,
            env,
        )
        .expect_err("config fails");

        assert!(matches!(
            err,
            ConfigError::InvalidValue { key, .. } if key == "classifier.command"
        ));
    }

    #[test]
    fn resolves_missing_agent_to_default_store_only() {
        let loaded = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"
            "#,
            env,
        )
        .expect("config loads");

        let policy = loaded.config.effective_agent_policy("codex");
        assert_eq!(policy.default_store, "personal");
        assert_eq!(policy.read_stores, vec!["personal"]);
        assert_eq!(policy.write_stores, vec!["personal"]);
        assert!(!policy.allow_all_stores);
    }

    #[test]
    fn rejects_agent_store_outside_configured_stores() {
        let err = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"

            [agents.codex]
            default_store = "work"
            read_stores = ["personal"]
            write_stores = ["personal"]
            "#,
            env,
        )
        .expect_err("config fails");

        assert_eq!(
            err,
            ConfigError::UnknownAgentStore {
                agent: "codex".to_owned(),
                store: "work".to_owned()
            }
        );
    }

    #[test]
    fn rejects_secret_store_on_cloud_root_by_default() {
        let err = LoadedConfig::from_str_with_env(
            r#"
            default_store = "secret"

            [stores.secret]
            root = "${HOME}/gdrive/hive-memory/secret"
            sensitivity = "secret"
            "#,
            env,
        )
        .expect_err("config fails");

        assert_eq!(
            err,
            ConfigError::SecretStoreOnCloudRoot {
                store: "secret".to_owned(),
                root: PathBuf::from("/home/tester/gdrive/hive-memory/secret")
            }
        );
    }

    #[test]
    fn rejects_hook_secret_writes_without_secret_writes() {
        let err = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"

            [privacy]
            allow_hook_secret_writes = true
            "#,
            env,
        )
        .expect_err("config fails");

        assert_eq!(err, ConfigError::HookSecretWritesRequireSecretWrites);
    }

    #[test]
    fn warns_on_unknown_top_level_keys() {
        let loaded = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"
            typo = true

            [stores.personal]
            root = "/tmp/personal"
            "#,
            env,
        )
        .expect("config loads");

        assert_eq!(
            loaded.warnings,
            vec![ConfigWarning::UnknownTopLevelKey("typo".to_owned())]
        );
    }

    #[test]
    fn warns_on_unknown_subkeys() {
        let loaded = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"
            extra_store_key = "private"

            [agents.codex]
            default_store = "personal"
            read_stores = ["personal"]
            write_stores = ["personal"]
            write_store = ["personal"]
            "#,
            env,
        )
        .expect("config loads");

        assert_eq!(
            loaded.warnings,
            vec![
                ConfigWarning::UnknownSubkey("stores.personal.extra_store_key".to_owned()),
                ConfigWarning::UnknownSubkey("agents.codex.write_store".to_owned()),
            ]
        );
    }
}
