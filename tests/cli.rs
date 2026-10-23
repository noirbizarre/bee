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
fn a_bare_run_exits_cleanly_when_the_client_closes_stdin() {
    // An empty stdin is a client that connected and left. Hanging here would
    // leave an orphaned agent behind every client crash.
    bee()
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}
