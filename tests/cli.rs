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
fn models_commands_browse_dynamic_cached_providers_and_preserve_manual_ids_offline() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("selected.toml");
    std::fs::write(&config, "[catalog]\nrefresh=false\n[providers.local]\nmodel='private/unknown'\ncatalog_provider='dynamic'\nprotocols={openai-chat={}}\n").unwrap();
    let cache = dir.path().join("cache/bee/models.dev.json");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, serde_json::to_vec(&serde_json::json!({"version":1,"fetched_at":1,"catalog":{"dynamic":{"name":"Dynamic provider","models":{"public":{"name":"Public model","tool_call":true}}}}})).unwrap()).unwrap();
    let output = isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["models", "providers", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(output["catalog"]["stale"], true);
    assert_eq!(output["result"]["providers"][0]["id"], "dynamic");
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["models", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("private/unknown"))
        .stdout(predicate::str::contains("Public model"))
        .stderr(predicate::str::contains("stale"));
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["models", "list", "--catalog-provider", "dynamic", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("models.dev"));
}

#[test]
fn disabled_catalog_keeps_manual_model_queries_working_and_refresh_fails_actionably() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("selected.toml");
    std::fs::write(&config, "[catalog]\nenabled=false\n[providers.local]\nmodel='private/model'\nprotocols={openai-chat={}}\n").unwrap();
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["models", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("private/model"));
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["models", "refresh"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bee::models::discovery"));
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["models", "providers"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("configure model IDs manually"));
}

#[test]
fn invalid_catalog_settings_do_not_echo_values_or_unknown_keys() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("selected.toml");
    for setting in [
        format!("enabled='{SECRET}'"),
        format!("{SECRET}=true"),
        "refresh_interval_secs=0".into(),
    ] {
        std::fs::write(&config, format!("[catalog]\n{setting}")).unwrap();
        isolated(dir.path())
            .env("BEE_CONFIG", &config)
            .write_stdin("")
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("bee::models::setting"))
            .stderr(predicate::str::contains(SECRET).not());
    }
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
        // Where credentials are kept: never the developer's own.
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env_remove("BEE_CONFIG")
        .env_remove("BEE_TEST_KEY")
        .current_dir(dir);
    command
}

const SECRET: &str = "sk-super-secret-token";

#[test]
fn invalid_bash_policy_fails_before_protocol_output_without_echoing_values() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("selected.toml");
    std::fs::write(&file, format!("[tools.bash]\ntimeout_ms = '{SECRET}'")).unwrap();
    isolated(dir.path())
        .env("BEE_CONFIG", &file)
        .write_stdin("")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("bee::bash::setting"))
        .stderr(predicate::str::contains("tools.bash.timeout_ms"))
        .stderr(predicate::str::contains(SECRET).not());
}

#[test]
fn invalid_agent_limits_fail_before_protocol_output_without_echoing_values() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("selected.toml");
    std::fs::write(&file, format!("[agent]\nmax_steps = '{SECRET}'")).unwrap();
    isolated(dir.path())
        .env("BEE_CONFIG", &file)
        .write_stdin("")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("bee::agent::setting"))
        .stderr(predicate::str::contains("agent.max_steps"))
        .stderr(predicate::str::contains(SECRET).not());
}

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
    std::fs::write(
        &global,
        "[providers.gw]\nprotocols = { openai-chat = {} }\nmodel = \"x\"\n",
    )
    .unwrap();
    let project = dir.path().join(".config/bee.toml");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&project, "[providers.gw]\nmodel = \"y\"\n").unwrap();

    isolated(dir.path())
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}

#[test]
fn an_invalid_provider_profile_fails_startup_without_echoing_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join(".config/bee.toml");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(
        &project,
        format!("[providers.p]\nprotocols = {{ openai-chat = {{ base_url = \"https://u:{SECRET}@x.example\" }} }}\n"),
    )
    .unwrap();

    isolated(dir.path())
        .write_stdin("")
        .assert()
        .failure()
        // Stdout is protocol-only, so a failed launch must leave it empty.
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("bee::provider::invalid"))
        .stderr(predicate::str::contains(
            "providers.p.protocols.openai-chat.base_url",
        ))
        .stderr(predicate::str::contains(SECRET).not());
}

// ---- `bee auth` ---------------------------------------------------------------
#[test]
fn invalid_model_mappings_fail_startup_without_echoing_model_keys_or_protocol_values() {
    for value in [
        format!("'{SECRET}'"),
        "1".into(),
        "'openai-responses'".into(),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join(".config/bee.toml");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, format!("[providers.p]\nprotocols={{openai-chat={{}}}}\nmodel_protocols={{'{SECRET}'={value}}}")).unwrap();
        isolated(dir.path())
            .write_stdin("")
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("model_protocols"))
            .stderr(predicate::str::contains(SECRET).not());
    }
}

const AUTH_CONFIG: &str = r#"
provider = "stored"

[providers.stored]
protocols = { openai-chat = {} }
credential = { store = true }

[providers.browser]
protocols = { openai-chat = {} }
credential = { store = true, login = "openrouter" }

[providers.env]
protocols = { anthropic-messages = {} }
credential = { env = "BEE_TEST_KEY" }

[providers.local]
protocols = { openai-chat = { base_url = "http://localhost:11434/v1" } }
"#;

/// A project directory configured with [`AUTH_CONFIG`].
fn configured() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join(".config/bee.toml");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&project, AUTH_CONFIG).unwrap();
    dir
}

// These commands exercise the real private-file store, which is Unix-only.
#[cfg(unix)]
#[test]
fn a_token_can_be_stored_checked_and_forgotten_without_it_ever_being_printed() {
    let dir = configured();
    let file = dir.path().join("data/bee/credentials.json");

    isolated(dir.path())
        .args(["auth", "login", "--with-token"])
        .write_stdin(format!("{SECRET}\n"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Stored the API token for `stored`",
        ))
        .stderr(predicate::str::contains(SECRET).not());
    assert!(file.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the credential file must be private");
    }

    isolated(dir.path())
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("stored credential present"))
        .stdout(predicate::str::contains(SECRET).not())
        .stderr(predicate::str::is_empty());

    isolated(dir.path())
        .args(["auth", "logout"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Removed the stored credential"));
    assert!(!file.exists(), "the last credential takes the file with it");

    isolated(dir.path())
        .args(["auth", "status", "stored"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no stored credential"));
}

#[cfg(unix)]
#[test]
fn status_lists_every_profile_and_the_state_of_its_environment_variable() {
    let dir = configured();

    isolated(dir.path())
        .env("BEE_TEST_KEY", SECRET)
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("`BEE_TEST_KEY` is set"))
        .stdout(predicate::str::contains("unauthenticated endpoint"))
        .stdout(predicate::str::contains("browser sign-in"))
        .stdout(predicate::str::contains("(default)"))
        .stdout(predicate::str::contains(SECRET).not());

    isolated(dir.path())
        .args(["auth", "status", "env"])
        .assert()
        .success()
        .stdout(predicate::str::contains("is NOT set"));
}

#[test]
fn environment_credential_status_needs_no_file_store() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join(".config/bee.toml");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    // Exclude stored profiles: status checks every configured profile's source.
    std::fs::write(
        project,
        "[providers.env]\nprotocols = { openai-chat = {} }\ncredential = { env = \"BEE_TEST_KEY\" }\n",
    )
    .unwrap();

    isolated(dir.path())
        .env("BEE_TEST_KEY", SECRET)
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("`BEE_TEST_KEY` is set"))
        .stdout(predicate::str::contains(SECRET).not());
    isolated(dir.path())
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("is NOT set"));
    assert!(!dir.path().join("data/bee/credentials.json").exists());
}

#[cfg(not(unix))]
#[test]
fn file_store_commands_report_unsupported_storage_without_exposing_the_token() {
    let dir = configured();
    for args in [
        vec!["auth", "login", "stored", "--with-token"],
        vec!["auth", "logout", "stored"],
        vec!["auth", "status"],
    ] {
        isolated(dir.path())
            .args(args)
            .write_stdin(format!("{SECRET}\n"))
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("bee::auth::store_unsupported"))
            .stderr(predicate::str::contains(SECRET).not());
    }
    assert!(!dir.path().join("data").exists());
}

#[test]
fn an_empty_token_is_refused_and_nothing_is_stored() {
    let dir = configured();

    isolated(dir.path())
        .args(["auth", "login", "stored", "--with-token"])
        .write_stdin("\n")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("bee::auth::empty_token"));
    assert!(!dir.path().join("data/bee/credentials.json").exists());
}

#[test]
fn a_profile_that_does_not_use_the_store_cannot_be_logged_in() {
    let dir = configured();

    isolated(dir.path())
        .args(["auth", "login", "env", "--with-token"])
        .write_stdin(format!("{SECRET}\n"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("bee::auth::not_stored"))
        .stderr(predicate::str::contains(SECRET).not());
}

#[test]
fn a_browser_sign_in_needs_a_login_flow_on_the_profile() {
    let dir = configured();

    isolated(dir.path())
        .args(["auth", "login", "stored"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bee::auth::no_login_flow"));
}

#[test]
fn an_unknown_profile_lists_the_defined_ones() {
    let dir = configured();

    isolated(dir.path())
        .args(["auth", "status", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bee::auth::unknown_profile"))
        .stderr(predicate::str::contains("browser"));
}

#[test]
fn the_flags_that_exclude_each_other_are_refused_by_the_parser() {
    let dir = configured();

    isolated(dir.path())
        .args(["auth", "login", "--with-token", "--paste-code"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[cfg(unix)]
#[test]
fn a_credential_file_others_can_read_stops_every_auth_command() {
    use std::os::unix::fs::PermissionsExt;

    let dir = configured();
    isolated(dir.path())
        .args(["auth", "login", "--with-token"])
        .write_stdin(format!("{SECRET}\n"))
        .assert()
        .success();
    let file = dir.path().join("data/bee/credentials.json");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

    isolated(dir.path())
        .args(["auth", "status"])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("bee::auth::insecure_permissions"))
        .stderr(predicate::str::contains(SECRET).not());
}

#[test]
fn auth_with_no_configured_profile_says_so_instead_of_failing() {
    let dir = tempfile::tempdir().unwrap();

    isolated(dir.path())
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "No provider profiles are configured",
        ));
}
