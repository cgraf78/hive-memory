//! Top-level diagnostics for `hm`.
//!
//! `hm stores doctor` answers "is this store root healthy?" This module answers
//! the broader operational question hooks and dotfiles update care about: can
//! the configured stores, local project bindings, and adapter links be trusted
//! before an agent relies on them?

use crate::{config, note, project, render, secret, store};
use serde::Serialize;
use std::fs;
use std::path::Path;

/// Input for one top-level doctor run.
#[derive(Debug, Clone)]
pub struct DoctorInput<'a> {
    /// Effective config after normal config loading/validation.
    pub config: &'a config::Config,
    /// Run the hook/update-safe subset.
    pub quick: bool,
}

/// Top-level diagnostic report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    /// Whether no error-severity checks were found.
    pub ok: bool,
    /// Count summary for human and JSON callers.
    pub summary: DoctorSummary,
    /// Individual checks in deterministic order.
    pub checks: Vec<DoctorCheck>,
}

/// Count summary for a doctor report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DoctorSummary {
    /// Number of error-severity checks. Non-zero makes the report fail.
    pub errors: usize,
    /// Number of warning-severity checks. Warnings do not make `ok` false.
    pub warnings: usize,
}

/// One doctor check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorCheck {
    /// Stable machine-readable id for tests and hook adapters.
    pub id: String,
    /// Severity of this finding.
    pub severity: DoctorSeverity,
    /// Status string from the JSON contract: `pass`, `warn`, or `fail`.
    pub status: DoctorStatus,
    /// Human diagnostic.
    pub message: String,
    /// Paths involved in the finding.
    pub paths: Vec<String>,
}

/// Finding severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorSeverity {
    /// Informational check that should not be shown as a problem.
    Info,
    /// Non-fatal issue that may need user attention.
    Warning,
    /// Fatal issue that makes the report fail.
    Error,
}

impl std::fmt::Display for DoctorSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        };
        f.write_str(value)
    }
}

/// Check status from the stable JSON contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorStatus {
    /// Check passed without warnings or errors.
    Pass,
    /// Check completed with a non-fatal warning.
    Warn,
    /// Check found a fatal error.
    Fail,
}

/// Run top-level diagnostics.
///
/// This function is intentionally read-only. Repair and cleanup commands can
/// build on these checks later, but lifecycle hooks need a cheap diagnostic that
/// never mutates stores or agent instruction files.
pub fn run(input: DoctorInput<'_>) -> DoctorReport {
    let mut checks = Vec::new();
    checks.push(pass("config", "config parsed and validated", Vec::new()));
    checks.push(pass(
        "default-store",
        format!("default store is {}", input.config.default_store),
        Vec::new(),
    ));

    check_stores(input.config, &mut checks);
    check_project_bindings(input.config, &mut checks);
    check_adapters(input.config, input.quick, &mut checks);
    // Secret scanning is deliberately kept off the quick path. Hooks and
    // update-time health checks need cheap structural validation, while this
    // audit walks note content and is intended for explicit human review.
    if !input.quick {
        check_note_secrets(input.config, &mut checks);
    }

    summarize(checks)
}

fn check_stores(config: &config::Config, checks: &mut Vec<DoctorCheck>) {
    for (name, store_config) in &config.stores {
        let report = store::doctor_store(store::StoreDoctorInput {
            name: name.as_str(),
            config: store_config,
        });
        if report.issues.is_empty() {
            checks.push(pass(
                format!("store.{name}"),
                format!("store {name} is available"),
                vec![store_config.root.display().to_string()],
            ));
            continue;
        }
        for issue in report.issues {
            let paths = vec![store_config.root.display().to_string()];
            match issue.level {
                store::StoreDoctorLevel::Warning => checks.push(warn(
                    format!("store.{name}"),
                    format!("store {name}: {}", issue.message),
                    paths,
                )),
                store::StoreDoctorLevel::Error => checks.push(error(
                    format!("store.{name}"),
                    format!("store {name}: {}", issue.message),
                    paths,
                )),
            }
        }
    }
}

fn check_project_bindings(config: &config::Config, checks: &mut Vec<DoctorCheck>) {
    let dir = config.data_dir.join("projects");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            checks.push(pass(
                "project-bindings",
                "no local project bindings configured",
                vec![dir.display().to_string()],
            ));
            return;
        }
        Err(err) => {
            checks.push(error(
                "project-bindings",
                format!("failed to read local project bindings: {err}"),
                vec![dir.display().to_string()],
            ));
            return;
        }
    };

    let mut saw_binding = false;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                checks.push(error(
                    "project-bindings",
                    format!("failed to read local project binding entry: {err}"),
                    vec![dir.display().to_string()],
                ));
                continue;
            }
        };
        let path = entry.path();
        if !path_is_toml(&path) {
            continue;
        }
        saw_binding = true;
        match fs::read_to_string(&path)
            .map_err(|err| err.to_string())
            .and_then(|contents| {
                toml::from_str::<project::ProjectBinding>(&contents).map_err(|err| err.to_string())
            }) {
            Ok(binding) if config.stores.contains_key(&binding.store) => checks.push(pass(
                format!("project-binding.{}", binding.project_id),
                format!(
                    "project {} is bound to configured store {}",
                    binding.project_id, binding.store
                ),
                vec![path.display().to_string()],
            )),
            Ok(binding) => checks.push(warn(
                format!("project-binding.{}", binding.project_id),
                format!(
                    "project {} is bound to unknown store {}",
                    binding.project_id, binding.store
                ),
                vec![path.display().to_string()],
            )),
            Err(message) => checks.push(error(
                "project-bindings",
                format!("failed to parse local project binding: {message}"),
                vec![path.display().to_string()],
            )),
        }
    }

    if !saw_binding {
        checks.push(pass(
            "project-bindings",
            "no local project bindings configured",
            vec![dir.display().to_string()],
        ));
    }
}

fn check_adapters(config: &config::Config, _quick: bool, checks: &mut Vec<DoctorCheck>) {
    for (name, adapter) in config
        .adapters
        .iter()
        .filter(|(_name, adapter)| adapter.enabled)
    {
        let Some(output) = adapter.output.as_ref() else {
            checks.push(error(
                format!("adapter.{name}.output"),
                format!("enabled adapter {name} has no output configured"),
                Vec::new(),
            ));
            continue;
        };
        inspect_adapter_output(name, output, checks);

        // Quick mode is the path used by dotfiles update and hooks, so adapter
        // visibility belongs in the quick-safe set. Heavier future checks such
        // as full secret scans can branch on `_quick` without weakening install
        // validation.
        inspect_adapter_install(name, output, adapter.install_target.as_deref(), checks);
    }
}

fn inspect_adapter_output(name: &str, output: &Path, checks: &mut Vec<DoctorCheck>) {
    match render::inspect_rendered_file(output) {
        Ok(report) if report.valid => checks.push(pass(
            format!("adapter.{name}.output"),
            format!("adapter {name} output marker is valid"),
            vec![output.display().to_string()],
        )),
        Ok(_report) => checks.push(warn(
            format!("adapter.{name}.output"),
            format!("adapter {name} output is missing; run `hm render {name}`"),
            vec![output.display().to_string()],
        )),
        Err(err) => checks.push(warn(
            format!("adapter.{name}.output"),
            format!("adapter {name} output is not a valid generated file: {err}"),
            vec![output.display().to_string()],
        )),
    }
}

fn inspect_adapter_install(
    name: &str,
    output: &Path,
    install_target: Option<&Path>,
    checks: &mut Vec<DoctorCheck>,
) {
    let Some(install_target) = install_target else {
        checks.push(warn(
            format!("adapter.{name}.install"),
            format!("enabled adapter {name} has no install_target configured"),
            Vec::new(),
        ));
        return;
    };

    match render::inspect_adapter_install(render::InspectAdapterInstallInput {
        adapter: name,
        output,
        install_target,
    }) {
        Ok(report) if report.installed && report.include_matches => checks.push(pass(
            format!("adapter.{name}.install"),
            format!("adapter {name} marker is installed"),
            vec![report.target.display().to_string()],
        )),
        Ok(report) if !report.target_exists => checks.push(warn(
            format!("adapter.{name}.install"),
            format!("adapter {name} install target is missing; run `hm render {name} --install`"),
            vec![report.target.display().to_string()],
        )),
        Ok(report) if !report.installed => checks.push(warn(
            format!("adapter.{name}.install"),
            format!("adapter {name} marker is not installed; run `hm render {name} --install`"),
            vec![report.target.display().to_string()],
        )),
        Ok(report) => checks.push(error(
            format!("adapter.{name}.install"),
            format!(
                "adapter {name} marker points at {}, expected @{}",
                report.include.unwrap_or_default(),
                output.display()
            ),
            vec![report.target.display().to_string()],
        )),
        Err(err) => checks.push(error(
            format!("adapter.{name}.install"),
            format!("adapter {name} install target cannot be inspected: {err}"),
            vec![install_target.display().to_string()],
        )),
    }
}

fn check_note_secrets(config: &config::Config, checks: &mut Vec<DoctorCheck>) {
    for (store_name, store_config) in &config.stores {
        if store_config.sensitivity == config::Sensitivity::Secret {
            checks.push(pass(
                format!("store.{store_name}.note-secrets"),
                format!("store {store_name} is a secret store; note secret scan skipped"),
                vec![store_config.root.display().to_string()],
            ));
            continue;
        }

        let notes_root = store_config.root.join("inbox/notes");
        let note_paths = match collect_markdown_files(&notes_root) {
            Ok(paths) => paths,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                checks.push(pass(
                    format!("store.{store_name}.note-secrets"),
                    format!("store {store_name} has no notes to scan"),
                    vec![notes_root.display().to_string()],
                ));
                continue;
            }
            Err(err) => {
                checks.push(error(
                    format!("store.{store_name}.note-secrets"),
                    format!("failed to scan notes for likely secrets: {err}"),
                    vec![notes_root.display().to_string()],
                ));
                continue;
            }
        };

        let mut issues = 0usize;
        for path in note_paths {
            match scan_note_for_secrets(&path) {
                Ok(detectors) if detectors.is_empty() => {}
                Ok(detectors) => {
                    issues += 1;
                    checks.push(warn(
                        format!("store.{store_name}.note-secrets"),
                        format!(
                            "note contains likely secret material; detectors: {}",
                            detectors.join(",")
                        ),
                        vec![path.display().to_string()],
                    ));
                }
                Err(message) => {
                    issues += 1;
                    checks.push(warn(
                        format!("store.{store_name}.note-secrets"),
                        format!("failed to parse note during secret scan: {message}"),
                        vec![path.display().to_string()],
                    ));
                }
            }
        }

        if issues == 0 {
            checks.push(pass(
                format!("store.{store_name}.note-secrets"),
                format!("store {store_name} notes contain no likely secrets"),
                vec![notes_root.display().to_string()],
            ));
        }
    }
}

fn scan_note_for_secrets(path: &Path) -> Result<Vec<String>, String> {
    let contents = fs::read_to_string(path).map_err(|err| err.to_string())?;
    let parsed = note::parse_note(&contents).map_err(|err| err.to_string())?;
    // Reuse the write-time detectors against the body only: front matter holds
    // hm metadata, and diagnostics must identify detectors without echoing a
    // matched value back into terminal scrollback, logs, or agent transcripts.
    Ok(secret::detect(&parsed.body)
        .into_iter()
        .map(|finding| finding.detector_id)
        .collect())
}

fn collect_markdown_files(root: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut paths = Vec::new();
    collect_markdown_files_into(root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn collect_markdown_files_into(
    root: &Path,
    paths: &mut Vec<std::path::PathBuf>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_markdown_files_into(&path, paths)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("md") {
            paths.push(path);
        }
    }
    Ok(())
}

fn summarize(checks: Vec<DoctorCheck>) -> DoctorReport {
    let mut summary = DoctorSummary::default();
    for check in &checks {
        match check.severity {
            DoctorSeverity::Info => {}
            DoctorSeverity::Warning => summary.warnings += 1,
            DoctorSeverity::Error => summary.errors += 1,
        }
    }
    DoctorReport {
        ok: summary.errors == 0,
        summary,
        checks,
    }
}

fn pass(id: impl Into<String>, message: impl Into<String>, paths: Vec<String>) -> DoctorCheck {
    DoctorCheck {
        id: id.into(),
        severity: DoctorSeverity::Info,
        status: DoctorStatus::Pass,
        message: message.into(),
        paths,
    }
}

fn warn(id: impl Into<String>, message: impl Into<String>, paths: Vec<String>) -> DoctorCheck {
    DoctorCheck {
        id: id.into(),
        severity: DoctorSeverity::Warning,
        status: DoctorStatus::Warn,
        message: message.into(),
        paths,
    }
}

fn error(id: impl Into<String>, message: impl Into<String>, paths: Vec<String>) -> DoctorCheck {
    DoctorCheck {
        id: id.into(),
        severity: DoctorSeverity::Error,
        status: DoctorStatus::Fail,
        message: message.into(),
        paths,
    }
}

fn path_is_toml(path: &Path) -> bool {
    path.extension().and_then(|value| value.to_str()) == Some("toml")
}
