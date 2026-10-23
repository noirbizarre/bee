//! Smoke tests for the `bee` binary's observable contract.

use assert_cmd::Command;
use predicates::prelude::*;

fn bee() -> Command {
    Command::cargo_bin("bee").expect("the bee binary is built by cargo test")
}

#[test]
fn the_version_flag_names_the_binary() {
    bee()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("bee "));
}

#[test]
fn the_help_flag_describes_bee_as_an_acpv2_runtime() {
    bee()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("ACPv2"));
}

#[test]
fn a_bare_run_fails_with_the_not_implemented_diagnostic() {
    // Guards against a bootstrap build exiting 0, which a client would read as
    // a healthy agent.
    bee()
        .assert()
        .failure()
        .stderr(predicate::str::contains("bee::error::not_implemented"));
}
