use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;

#[test]
fn version_prints_binary_name() {
    let mut cmd = cargo_bin_cmd!("hm");

    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("hm "));
}

#[test]
fn help_describes_project() {
    let mut cmd = cargo_bin_cmd!("hm");

    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Vendor-neutral shared memory infrastructure for AI agents.",
        ));
}
