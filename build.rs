use std::process::Command;

const SCHEMA_VERSION: u32 = 1;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");

    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets package version");
    let Some(git) = git_revision() else {
        println!("cargo:rustc-env=HM_CLI_VERSION={version} (schema {SCHEMA_VERSION})");
        return;
    };
    println!("cargo:rustc-env=HM_CLI_VERSION={version} (git {git}, schema {SCHEMA_VERSION})");
}

fn git_revision() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let revision = String::from_utf8(output.stdout).ok()?;
    let revision = revision.trim();
    (!revision.is_empty()).then(|| revision.to_owned())
}
