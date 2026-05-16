//! Configuration loading and validation for `hm`.
//!
//! The config layer is a public contract for every higher-level workflow:
//! commands, render adapters, and non-interactive agent hooks all need the same
//! store-affinity and privacy decisions. Keep policy here instead of forcing
//! hooks or adapters to rediscover context on their own.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

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
    "adapters",
];

const STORE_KEYS: &[&str] = &["root", "expected_id", "description", "sensitivity"];
const STORAGE_KEYS: &[&str] = &["kind", "case_sensitive", "atomic_rename", "fsync"];
const DEFAULTS_KEYS: &[&str] = &[
    "write_scope",
    "search_scopes",
    "context_sources",
    "render_scopes",
    "event_sidecar",
    "hook_context_max_tokens",
    "context_cache_max_age",
];
const AGENT_KEYS: &[&str] = &[
    "default_store",
    "read_stores",
    "write_stores",
    "allow_all_stores",
];
const PRIVACY_KEYS: &[&str] = &[
    "default_render_policy",
    "allow_all_stores_flag",
    "warn_sensitive_broad_render",
    "secret_refuses_cloud_roots",
    "allow_secret_writes",
    "allow_hook_secret_writes",
];
const OFFLINE_KEYS: &[&str] = &["enabled", "mode"];
const PERFORMANCE_KEYS: &[&str] = &[
    "context_warm_p95_ms",
    "context_cold_p95_ms",
    "context_store_size_target",
];
const ADAPTER_KEYS: &[&str] = &[
    "enabled",
    "stores",
    "scopes",
    "output",
    "install_target",
    "install_mode",
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
    /// Configured stores keyed by local alias.
    pub stores: BTreeMap<String, StoreConfig>,
    /// Optional per-agent access policy keyed by agent id.
    pub agents: BTreeMap<String, AgentConfig>,
    /// Privacy and secret-handling policy shared by commands and hooks.
    pub privacy: PrivacyConfig,
    /// Render/install targets for agent-visible context files.
    pub adapters: BTreeMap<String, AdapterConfig>,
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
    Public,
    Internal,
    #[default]
    Private,
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

/// Privacy controls that affect rendering, hook writes, and secret stores.
///
/// The defaults are intentionally conservative around secrets while still
/// keeping ordinary single-store setups simple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacyConfig {
    /// Default policy for rendering agent-visible context.
    pub default_render_policy: String,
    /// Whether CLI users may explicitly opt into all-store operations.
    pub allow_all_stores_flag: bool,
    /// Whether broad renders over sensitive stores should emit warnings.
    pub warn_sensitive_broad_render: bool,
    /// Whether secret stores are rejected under common sync roots by default.
    pub secret_refuses_cloud_roots: bool,
    /// Whether any command may write secret-classified material.
    pub allow_secret_writes: bool,
    /// Whether non-interactive hooks may write secret-classified material.
    pub allow_hook_secret_writes: bool,
}

/// Render/install adapter configuration for an agent surface.
///
/// Adapters describe generated files such as Claude/Codex includes. They are
/// separate from `AgentConfig` because rendering a context file and giving an
/// agent write access to a store are related, but not identical, policy choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterConfig {
    /// Whether this adapter participates in render/install operations.
    pub enabled: bool,
    /// Store aliases this adapter may render from.
    pub stores: Vec<String>,
    /// Memory scopes this adapter may expose.
    pub scopes: Vec<String>,
    /// Generated include file path.
    pub output: Option<PathBuf>,
    /// Agent-owned file that should include or link to `output`.
    pub install_target: Option<PathBuf>,
    /// Install strategy. V1 supports `include` only.
    pub install_mode: Option<String>,
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
/// config, because continuing would use an ambiguous store, unsafe secret
/// policy, or invalid adapter target.
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
    /// Adapter policy references no configured store.
    UnknownAdapterStore {
        /// Adapter id owning the bad reference.
        adapter: String,
        /// Missing store alias.
        store: String,
    },
    /// Adapter requested an unsupported install mode.
    InvalidAdapterInstallMode {
        /// Adapter id owning the bad value.
        adapter: String,
        /// Unsupported install mode.
        install_mode: String,
    },
    /// Enabled adapter has nowhere to render its include file.
    EnabledAdapterMissingOutput {
        /// Adapter id missing output.
        adapter: String,
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
            Self::UnknownAdapterStore { adapter, store } => {
                write!(f, "adapters.{adapter} references unknown store: {store}")
            }
            Self::InvalidAdapterInstallMode {
                adapter,
                install_mode,
            } => write!(
                f,
                "adapters.{adapter}.install_mode must be include, got {install_mode}"
            ),
            Self::EnabledAdapterMissingOutput { adapter } => {
                write!(f, "enabled adapter {adapter} requires output")
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
    /// Agent hooks and adapters should use this API rather than inspecting
    /// `Config::agents` directly, because this function centralizes the
    /// conservative missing-agent behavior required by the spec.
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

        let adapters = raw
            .adapters
            .into_iter()
            .map(|(name, adapter)| {
                let adapter = AdapterConfig::from_raw(adapter, env)?;
                validate_adapter(&name, &adapter, &stores)?;
                Ok((name, adapter))
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
            stores,
            agents,
            privacy,
            adapters,
        })
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
            default_render_policy: raw
                .default_render_policy
                .unwrap_or_else(|| "conservative".to_owned()),
            allow_all_stores_flag: raw.allow_all_stores_flag.unwrap_or(true),
            warn_sensitive_broad_render: raw.warn_sensitive_broad_render.unwrap_or(true),
            secret_refuses_cloud_roots: raw.secret_refuses_cloud_roots.unwrap_or(true),
            allow_secret_writes: raw.allow_secret_writes.unwrap_or(false),
            allow_hook_secret_writes: raw.allow_hook_secret_writes.unwrap_or(false),
        }
    }
}

impl AdapterConfig {
    fn from_raw<F>(raw: RawAdapterConfig, env: &F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        Ok(Self {
            enabled: raw.enabled.unwrap_or(false),
            stores: raw.stores.unwrap_or_default(),
            scopes: raw.scopes.unwrap_or_default(),
            output: raw.output.map(|path| expand_path(&path, env)).transpose()?,
            install_target: raw
                .install_target
                .map(|path| expand_path(&path, env))
                .transpose()?,
            install_mode: raw.install_mode,
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
    agents: BTreeMap<String, RawAgentConfig>,
    privacy: RawPrivacyConfig,
    adapters: BTreeMap<String, RawAdapterConfig>,
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
            agents: BTreeMap::new(),
            privacy: RawPrivacyConfig::default(),
            adapters: BTreeMap::new(),
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
struct RawAgentConfig {
    default_store: Option<String>,
    read_stores: Option<Vec<String>>,
    write_stores: Option<Vec<String>>,
    allow_all_stores: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPrivacyConfig {
    default_render_policy: Option<String>,
    allow_all_stores_flag: Option<bool>,
    warn_sensitive_broad_render: Option<bool>,
    secret_refuses_cloud_roots: Option<bool>,
    allow_secret_writes: Option<bool>,
    allow_hook_secret_writes: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawAdapterConfig {
    enabled: Option<bool>,
    stores: Option<Vec<String>>,
    scopes: Option<Vec<String>>,
    output: Option<String>,
    install_target: Option<String>,
    install_mode: Option<String>,
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
    collect_dynamic_table_warnings(&mut warnings, table, "adapters", ADAPTER_KEYS);

    warnings
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

fn validate_adapter(
    name: &str,
    adapter: &AdapterConfig,
    stores: &BTreeMap<String, StoreConfig>,
) -> Result<(), ConfigError> {
    validate_store_refs(adapter.stores.iter().map(String::as_str), stores, |store| {
        ConfigError::UnknownAdapterStore {
            adapter: name.to_owned(),
            store: store.to_owned(),
        }
    })?;

    if adapter.enabled && adapter.output.is_none() {
        return Err(ConfigError::EnabledAdapterMissingOutput {
            adapter: name.to_owned(),
        });
    }

    if let Some(mode) = &adapter.install_mode
        && mode != "include"
    {
        return Err(ConfigError::InvalidAdapterInstallMode {
            adapter: name.to_owned(),
            install_mode: mode.clone(),
        });
    }

    Ok(())
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
        assert!(loaded.warnings.is_empty());
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
    fn loads_enabled_adapter_with_output() {
        let loaded = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"

            [adapters.codex]
            enabled = true
            stores = ["personal"]
            output = "${HOME}/.codex/hive-memory.generated.md"
            install_mode = "include"
            "#,
            env,
        )
        .expect("config loads");

        let adapter = &loaded.config.adapters["codex"];
        assert!(adapter.enabled);
        assert_eq!(
            adapter.output,
            Some(PathBuf::from(
                "/home/tester/.codex/hive-memory.generated.md"
            ))
        );
    }

    #[test]
    fn rejects_enabled_adapter_without_output() {
        let err = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"

            [adapters.codex]
            enabled = true
            stores = ["personal"]
            "#,
            env,
        )
        .expect_err("config fails");

        assert_eq!(
            err,
            ConfigError::EnabledAdapterMissingOutput {
                adapter: "codex".to_owned()
            }
        );
    }

    #[test]
    fn rejects_adapter_store_outside_configured_stores() {
        let err = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"

            [adapters.codex]
            stores = ["work"]
            "#,
            env,
        )
        .expect_err("config fails");

        assert_eq!(
            err,
            ConfigError::UnknownAdapterStore {
                adapter: "codex".to_owned(),
                store: "work".to_owned()
            }
        );
    }

    #[test]
    fn rejects_adapter_install_mode_other_than_include() {
        let err = LoadedConfig::from_str_with_env(
            r#"
            default_store = "personal"

            [stores.personal]
            root = "/tmp/personal"

            [adapters.codex]
            stores = ["personal"]
            install_mode = "symlink"
            "#,
            env,
        )
        .expect_err("config fails");

        assert_eq!(
            err,
            ConfigError::InvalidAdapterInstallMode {
                adapter: "codex".to_owned(),
                install_mode: "symlink".to_owned()
            }
        );
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
