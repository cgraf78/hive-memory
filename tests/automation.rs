use serde::Deserialize;
use serde_yaml_ng::{Mapping, Value};
use std::collections::HashMap;

const LOCKED_INSTALL: &str =
    "cargo install --path . --locked --root \"$RUNNER_TEMP/hive-memory-install\"";
const INSTALLED_SMOKE: &str = "\"$RUNNER_TEMP/hive-memory-install/bin/hm\" --version";
const AUDITOR_INSTALL: &str = "cargo install cargo-audit --version 0.22.2 --locked";
const LOCKFILE_AUDIT: &str = "cargo audit --file Cargo.lock";
const RUSTSEC_CONDITION: &str =
    "github.event_name == 'schedule' || github.event_name == 'workflow_dispatch'";

#[derive(Deserialize)]
struct Dependabot {
    version: u8,
    updates: Vec<Update>,
}

#[derive(Deserialize)]
struct Update {
    #[serde(rename = "package-ecosystem")]
    ecosystem: String,
    directory: String,
    schedule: Schedule,
}

#[derive(Deserialize)]
struct Schedule {
    interval: String,
}

#[derive(Deserialize)]
struct Workflow {
    jobs: HashMap<String, Job>,
}

#[derive(Deserialize)]
struct Job {
    #[serde(rename = "if")]
    condition: Option<Value>,
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    run: Option<String>,
    #[serde(rename = "if")]
    condition: Option<Value>,
}

fn validate_dependabot(yaml: &str) -> Result<(), String> {
    let config: Dependabot =
        serde_yaml_ng::from_str(yaml).map_err(|error| format!("invalid YAML: {error}"))?;
    if config.version != 2 {
        return Err("expected version 2".into());
    }

    for ecosystem in ["github-actions", "cargo"] {
        let update = config
            .updates
            .iter()
            .find(|update| update.ecosystem == ecosystem && update.directory == "/")
            .ok_or_else(|| format!("missing root {ecosystem} update"))?;
        if update.schedule.interval != "weekly" {
            return Err(format!("{ecosystem} update must run weekly"));
        }
    }
    Ok(())
}

fn validate_workflow(yaml: &str) -> Result<(), String> {
    let document: Value =
        serde_yaml_ng::from_str(yaml).map_err(|error| format!("invalid YAML: {error}"))?;
    require_triggers(&document)?;

    let workflow: Workflow =
        serde_yaml_ng::from_str(yaml).map_err(|error| format!("invalid YAML: {error}"))?;
    let source_install = workflow
        .jobs
        .get("source-install")
        .ok_or("missing source-install job")?;
    if source_install.condition.is_some() {
        return Err("source-install job must be unconditional".into());
    }
    require_unconditional_step(source_install, LOCKED_INSTALL)?;
    require_unconditional_step(source_install, INSTALLED_SMOKE)?;

    let rustsec = workflow.jobs.get("rustsec").ok_or("missing rustsec job")?;
    if rustsec.condition.as_ref().and_then(Value::as_str) != Some(RUSTSEC_CONDITION) {
        return Err("rustsec job has the wrong condition".into());
    }
    require_unconditional_step(rustsec, AUDITOR_INSTALL)?;
    require_unconditional_step(rustsec, LOCKFILE_AUDIT)
}

fn require_triggers(document: &Value) -> Result<(), String> {
    let root = document
        .as_mapping()
        .ok_or("workflow must be a YAML mapping")?;
    let triggers = mapping_value(root, "on")
        .and_then(Value::as_mapping)
        .ok_or("workflow must define a trigger mapping")?;

    match mapping_value(triggers, "schedule") {
        Some(Value::Sequence(entries)) if !entries.is_empty() => {}
        _ => return Err("workflow must define a schedule trigger".into()),
    }
    match mapping_value(triggers, "workflow_dispatch") {
        Some(Value::Null | Value::Mapping(_)) => Ok(()),
        _ => Err("workflow must define a workflow_dispatch trigger".into()),
    }
}

fn mapping_value<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a Value> {
    let string_key = Value::String(key.to_owned());
    mapping.get(&string_key)
}

fn require_unconditional_step(job: &Job, command: &str) -> Result<(), String> {
    let step = job
        .steps
        .iter()
        .find(|step| step.run.as_deref().is_some_and(|run| run.trim() == command))
        .ok_or_else(|| format!("missing run step {command:?}"))?;
    if step.condition.is_some() {
        return Err(format!("run step {command:?} must be unconditional"));
    }
    Ok(())
}

fn valid_dependabot() -> String {
    include_str!("../.github/dependabot.yml").to_owned()
}

fn valid_workflow() -> String {
    include_str!("../.github/workflows/ci.yml").to_owned()
}

#[test]
fn repository_automation_contracts_are_valid() {
    validate_dependabot(&valid_dependabot()).unwrap();
    validate_workflow(&valid_workflow()).unwrap();
}

#[test]
fn comments_do_not_satisfy_a_missing_dependabot_update() {
    let yaml = r#"
version: 2
updates:
  - package-ecosystem: "github-actions"
    directory: "/"
    schedule:
      interval: "weekly"
  # - package-ecosystem: "cargo"
  #   directory: "/"
  #   schedule:
  #     interval: "weekly"
"#;
    assert!(validate_dependabot(yaml).is_err());
}

#[test]
fn comments_do_not_satisfy_a_missing_command() {
    let yaml = valid_workflow().replace(
        &format!("        run: {LOCKFILE_AUDIT}"),
        &format!("        run: echo skipped\n      # run: {LOCKFILE_AUDIT}"),
    );
    assert!(validate_workflow(&yaml).is_err());
}

#[test]
fn a_missing_schedule_trigger_is_rejected() {
    let yaml = valid_workflow().replace("  schedule:\n    - cron: \"0 7 * * *\"\n", "");
    assert!(validate_workflow(&yaml).is_err());
}

#[test]
fn a_missing_manual_trigger_is_rejected() {
    let yaml = valid_workflow().replace("  workflow_dispatch:\n", "");
    assert!(validate_workflow(&yaml).is_err());
}

#[test]
fn a_boolean_true_key_does_not_satisfy_the_trigger_contract() {
    let yaml = valid_workflow().replacen("on:\n", "true:\n", 1);
    assert!(validate_workflow(&yaml).is_err());
}

#[test]
fn a_condition_on_the_source_install_job_is_rejected() {
    let yaml =
        valid_workflow().replace("  source-install:\n", "  source-install:\n    if: false\n");
    assert!(validate_workflow(&yaml).is_err());
}

#[test]
fn misplaced_commands_do_not_satisfy_a_job_step() {
    let yaml = valid_workflow().replace(
        &format!("        run: {LOCKFILE_AUDIT}"),
        &format!(
            "        env:\n          DEAD_COMMAND: {LOCKFILE_AUDIT}\n        run: echo skipped"
        ),
    );
    assert!(validate_workflow(&yaml).is_err());
}

#[test]
fn a_false_job_condition_is_rejected_even_when_the_expected_condition_is_commented() {
    let yaml = valid_workflow().replace(
        &format!("    if: {RUSTSEC_CONDITION}"),
        &format!("    # if: {RUSTSEC_CONDITION}\n    if: false"),
    );
    assert!(validate_workflow(&yaml).is_err());
}

#[test]
fn a_condition_on_a_required_step_is_rejected() {
    let yaml = valid_workflow().replace(
        &format!("        run: {LOCKFILE_AUDIT}"),
        &format!("        if: false\n        run: {LOCKFILE_AUDIT}"),
    );
    assert!(validate_workflow(&yaml).is_err());
}

#[test]
fn malformed_yaml_is_rejected() {
    assert!(validate_dependabot("version: [\n").is_err());
    assert!(validate_workflow("jobs: [\n").is_err());
}
