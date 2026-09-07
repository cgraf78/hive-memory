use serde::Deserialize;
use serde_yaml_ng::{Mapping, Value};
use std::collections::HashMap;

const LOCKED_INSTALL: &str =
    "cargo install --path . --locked --root \"$RUNNER_TEMP/hive-memory-install\"";
const INSTALLED_SMOKE: &str = "\"$RUNNER_TEMP/hive-memory-install/bin/hm\" --version";
const LOCKED_PERFORMANCE_BUDGET: &str =
    "cargo test --release --locked --test perf_budget -- --ignored --nocapture --test-threads=1";
const LOCKED_CLOUD_SYNC_SIM: &str =
    "cargo test --locked --test cloud_sync_sim -- --ignored --nocapture";
const PACKAGE_SMOKE: &str = r#"archive=$(scripts/package-release.sh "$RUST_TARGET" linux-x86_64-musl)
scripts/smoke-release.sh linux-x86_64-musl
install_root=$(mktemp -d)
trap 'rm -rf "$install_root"' EXIT
mkdir -p "$install_root/home"
HOME="$install_root/home" ./install.sh --archive "$archive" \
  --data-home "$install_root/data" --bin-dir "$install_root/bin" \
  --man-dir "$install_root/man"
"$install_root/bin/hm" --version
test -f "$install_root/data/cgraf78/hive-memory/man/man1/hm.1""#;
const SHARED_RUST_WORKFLOW: &str = "cgraf78/actions/.github/workflows/rust-ci.yml@";
// These inputs encode fleet policy in the reusable workflow. Hive only owns
// product-specific setup, packaging, and runtime commands; repeating a shared
// default here would create a second place for that policy to drift. The musl
// tools switch is the Hive-specific exception: its graph is pure Rust, so
// enabling that shared opt-in would add an unused apt/network dependency.
const FORBIDDEN_RUST_INPUTS: &[&str] = &[
    "rust-toolchain",
    "msrv-toolchain",
    "msrv-command",
    "test-command",
    "fmt-command",
    "clippy-command",
    "build-command",
    "doc-command",
    "audit-command",
    "package-smoke-install-musl-tools",
];

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
    uses: Option<String>,
    #[serde(rename = "with")]
    inputs: Option<Mapping>,
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

    let rust = workflow.jobs.get("rust").ok_or("missing shared rust job")?;
    if !rust
        .uses
        .as_deref()
        .is_some_and(|uses| uses.starts_with(SHARED_RUST_WORKFLOW))
    {
        return Err("rust job must use the shared cgraf78/actions workflow".into());
    }
    let inputs = rust
        .inputs
        .as_ref()
        .ok_or("shared rust job must define its product inputs")?;
    for input in FORBIDDEN_RUST_INPUTS {
        if mapping_value(inputs, input).is_some() {
            return Err(format!(
                "rust job must inherit shared {input} policy without an override"
            ));
        }
    }
    if mapping_value(inputs, "package-smoke-musl-target").and_then(Value::as_str)
        != Some("x86_64-unknown-linux-musl")
    {
        return Err("package smoke must declare its shared Rust target".into());
    }
    let package_smoke = mapping_value(inputs, "package-smoke-command")
        .and_then(Value::as_str)
        .ok_or("missing package-smoke-command")?;
    if package_smoke.trim() != PACKAGE_SMOKE {
        return Err("package smoke must build and smoke the prepared Rust target".into());
    }
    if workflow.jobs.contains_key("rustsec") {
        return Err("RustSec must be provided by the shared rust workflow".into());
    }

    let performance_budget = workflow
        .jobs
        .get("performance-budget")
        .ok_or("missing performance-budget job")?;
    require_unconditional_step(performance_budget, LOCKED_PERFORMANCE_BUDGET)?;

    let cloud_sync_sim = workflow
        .jobs
        .get("cloud-sync-sim")
        .ok_or("missing cloud-sync-sim job")?;
    require_unconditional_step(cloud_sync_sim, LOCKED_CLOUD_SYNC_SIM)
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
    include_str!("../.github/workflows/test.yml").to_owned()
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
        &format!("        run: {LOCKED_CLOUD_SYNC_SIM}"),
        &format!("        run: echo skipped\n      # run: {LOCKED_CLOUD_SYNC_SIM}"),
    );
    assert!(validate_workflow(&yaml).is_err());
}

#[test]
fn a_missing_schedule_trigger_is_rejected() {
    // Rename the key instead of matching the whole block so explanatory cron
    // comments can evolve without turning this mutation test into a no-op.
    let yaml = valid_workflow().replacen("  schedule:\n", "  not_schedule:\n", 1);
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
        &format!("        run: {LOCKED_PERFORMANCE_BUDGET}"),
        &format!(
            "        env:\n          DEAD_COMMAND: {LOCKED_PERFORMANCE_BUDGET}\n        run: echo skipped"
        ),
    );
    assert!(validate_workflow(&yaml).is_err());
}

#[test]
fn a_standalone_rustsec_job_is_rejected() {
    let yaml = valid_workflow().replace(
        "  source-install:\n",
        "  rustsec:\n    runs-on: ubuntu-24.04\n\n  source-install:\n",
    );
    assert!(validate_workflow(&yaml).is_err());
}

#[test]
fn shared_rust_policy_overrides_are_rejected() {
    for (input, value) in [
        ("rust-toolchain", "nightly"),
        ("msrv-toolchain", "1.88"),
        ("msrv-command", "cargo check"),
        ("test-command", "cargo test"),
        ("fmt-command", "true"),
        ("clippy-command", "cargo clippy"),
        ("build-command", "cargo build"),
        ("doc-command", "cargo doc --no-deps"),
        ("audit-command", r#""""#),
        ("package-smoke-install-musl-tools", "true"),
    ] {
        let yaml = valid_workflow().replacen(
            "    with:\n",
            &format!("    with:\n      {input}: {value}\n"),
            1,
        );
        assert!(
            validate_workflow(&yaml).is_err(),
            "{input} override unexpectedly passed"
        );
    }
}

#[test]
fn missing_or_drifted_product_inputs_are_rejected() {
    let missing_inputs = valid_workflow().replacen("    with:\n", "    not_with:\n", 1);
    assert!(validate_workflow(&missing_inputs).is_err());

    let repeated_target = valid_workflow().replace("\"$RUST_TARGET\"", "x86_64-unknown-linux-musl");
    assert!(validate_workflow(&repeated_target).is_err());

    let missing_smoke =
        valid_workflow().replace("        scripts/smoke-release.sh linux-x86_64-musl\n", "");
    assert!(validate_workflow(&missing_smoke).is_err());

    let missing_install = valid_workflow().replace(
        "        HOME=\"$install_root/home\" ./install.sh --archive \"$archive\" \\\n",
        "",
    );
    assert!(validate_workflow(&missing_install).is_err());

    let missing_installed_smoke =
        valid_workflow().replace("        \"$install_root/bin/hm\" --version\n", "");
    assert!(validate_workflow(&missing_installed_smoke).is_err());
}

#[test]
fn an_unlocked_product_test_is_rejected() {
    let yaml = valid_workflow().replace(
        LOCKED_CLOUD_SYNC_SIM,
        "cargo test --test cloud_sync_sim -- --ignored --nocapture",
    );
    assert!(validate_workflow(&yaml).is_err());
}

#[test]
fn malformed_yaml_is_rejected() {
    assert!(validate_dependabot("version: [\n").is_err());
    assert!(validate_workflow("jobs: [\n").is_err());
}
