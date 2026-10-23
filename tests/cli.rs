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
    let dir = tempfile::tempdir().unwrap();
    isolated(dir.path())
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}

/// `bee` with a clean environment: an empty home and working directory, so the
/// developer's own configuration cannot leak into the test.
fn isolated(dir: &std::path::Path) -> Command {
    let mut command = bee();
    command
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg"))
        .env_remove("BEE_CONFIG")
        .current_dir(dir);
    command
}

const SECRET: &str = "sk-super-secret-token";

#[test]
fn a_malformed_selected_config_fails_startup_with_a_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("selected.toml");
    std::fs::write(&file, format!("api_key = {SECRET}\n")).unwrap();

    isolated(dir.path())
        .env("BEE_CONFIG", &file)
        .write_stdin("")
        .assert()
        .failure()
        // Stdout is protocol-only, so a failed launch must leave it empty.
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("bee::config::parse"))
        .stderr(predicate::str::contains(SECRET).not());
}

#[test]
fn a_missing_bee_config_file_fails_startup() {
    let dir = tempfile::tempdir().unwrap();

    isolated(dir.path())
        .env("BEE_CONFIG", dir.path().join("absent.toml"))
        .write_stdin("")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("bee::config::selected_missing"));
}

#[test]
fn valid_layered_configuration_does_not_disturb_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("xdg/bee/config.toml");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, "[provider]\nname = \"x\"\n").unwrap();
    let project = dir.path().join(".config/bee.toml");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&project, "[provider]\nmodel = \"y\"\n").unwrap();

    isolated(dir.path())
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}
