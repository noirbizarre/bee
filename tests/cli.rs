//! Smoke tests for the `bee` binary's observable contract.

use assert_cmd::Command;
use predicates::prelude::*;

fn bee() -> Command {
    Command::cargo_bin("bee").expect("the bee binary is built by cargo test")
}

#[test]
fn cloudflare_environment_login_is_account_bound_and_logout_never_falls_back_to_environment() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("xdg/bee/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "[accounts.work]\nprovider='cloudflare'\nsettings={account_id='work-account',gateway_id='work-gateway'}\ncredential={env='WORK_TOKEN'}\n[accounts.personal]\nprovider='cloudflare'\nsettings={account_id='personal-account',gateway_id='personal-gateway'}\ncredential={env='PERSONAL_TOKEN'}\n").unwrap();
    for (account, variable, token) in [
        ("work", "WORK_TOKEN", "private-work-token"),
        ("personal", "PERSONAL_TOKEN", "private-personal-token"),
    ] {
        isolated(root.path())
            .env(variable, token)
            .args(["auth", "login", account])
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(token).not());
    }
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("private-work-token"));
    assert!(!text.contains("private-personal-token"));
    isolated(root.path())
        .args(["auth", "logout", "work"])
        .assert()
        .success();
    isolated(root.path())
        .env("WORK_TOKEN", "private-work-token")
        .args(["auth", "status", "work"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no stored credential"));
    isolated(root.path())
        .args(["auth", "status", "personal"])
        .assert()
        .success()
        .stdout(predicate::str::contains("stored credential present"));
}

#[test]
fn noninteractive_cloudflare_login_requires_configured_ids_before_reading_a_token() {
    let root = tempfile::tempdir().unwrap();
    isolated(root.path())
        .args(["auth", "login", "cloudflare", "--with-token"])
        .write_stdin("private-token\n")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("account_id"))
        .stderr(predicate::str::contains("gateway_id"))
        .stderr(predicate::str::contains("private-token").not());
    assert!(!root.path().join("xdg/bee/config.toml").exists());
}

#[test]
fn cloud_auth_status_is_offline_and_login_logout_leave_shared_adc_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("cloud.toml");
    let adc = dir.path().join("adc.json");
    // Invalid contents prove status and store commands never acquire or rewrite ADC.
    std::fs::write(&adc, SECRET).unwrap();
    std::fs::write(
        &config,
        "[accounts.cloud]\nprovider='google-cloud'\ngoogle_cloud={project='fixture-project'}",
    )
    .unwrap();
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .env("GOOGLE_APPLICATION_CREDENTIALS", &adc)
        .args(["auth", "status", "cloud"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Application Default Credentials"))
        .stdout(predicate::str::contains(SECRET).not());
    for operation in ["login", "logout"] {
        isolated(dir.path())
            .env("BEE_CONFIG", &config)
            .env("GOOGLE_APPLICATION_CREDENTIALS", &adc)
            .args(["auth", operation, "cloud"])
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("ADC is managed externally"))
            .stderr(predicate::str::contains(SECRET).not());
    }
    assert_eq!(std::fs::read_to_string(adc).unwrap(), SECRET);
}

#[test]
fn version_aliases_report_the_package_version_without_loading_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("selected.toml");
    std::fs::write(&config, format!("invalid = {SECRET}")).unwrap();
    for flag in ["--version", "-V"] {
        isolated(dir.path())
            .env("BEE_CONFIG", &config)
            .arg(flag)
            .assert()
            .success()
            .stdout(format!("bee {}\n", env!("CARGO_PKG_VERSION")))
            .stderr(predicate::str::is_empty());
    }
}

#[test]
fn help_for_every_command_is_available_before_configuration_is_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("selected.toml");
    std::fs::write(&config, format!("invalid = {SECRET}")).unwrap();
    // Help must also work for a missing selected file so it can explain how to repair a broken launch.
    for selected in [config, dir.path().join("absent.toml")] {
        for command in [
            vec![],
            vec!["auth"],
            vec!["auth", "login"],
            vec!["auth", "logout"],
            vec!["auth", "status"],
            vec!["models"],
            vec!["models", "providers"],
            vec!["models", "list"],
            vec!["models", "refresh"],
            vec!["providers"],
            vec!["providers", "list"],
            vec!["providers", "show"],
        ] {
            for flag in ["--help", "-h"] {
                isolated(dir.path())
                    .env("BEE_CONFIG", &selected)
                    .args(&command)
                    .arg(flag)
                    .assert()
                    .success()
                    .stdout(predicate::str::contains("Usage:"))
                    .stdout(predicate::str::contains(SECRET).not())
                    .stderr(predicate::str::is_empty());
            }
        }
    }
}

#[test]
fn invalid_arguments_and_conflicting_flags_exit_before_protocol_startup() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["unknown-command"],
        vec!["--unknown-option"],
        vec!["auth", "login", "--with-token", "--paste-code"],
        vec!["auth", "login", "--with-token", "--no-browser"],
        vec!["auth", "login", "--paste-code", "--no-browser"],
        vec!["models", "list", "local", "--catalog", "openai"],
        vec!["models", "list", "--catalog-provider", "openai"],
        vec!["models", "list", "--catalog", "openai", "--endpoint"],
    ] {
        isolated(dir.path())
            .args(args)
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("error:"))
            .stderr(predicate::str::contains("--help"));
    }
}

#[test]
fn scenario_selection_is_explicit_and_conflicts_with_human_facing_commands() {
    bee()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--scenario"))
        .stdout(predicate::str::contains("permission"));
    for args in [
        vec!["--scenario", "unknown"],
        vec!["--scenario", "success", "auth", "status"],
        vec!["auth", "status", "--scenario", "success"],
        vec!["--scenario", "success", "models", "list"],
        vec!["models", "list", "--scenario", "success"],
    ] {
        bee()
            .args(args)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty());
    }
}

#[test]
fn models_commands_browse_dynamic_cached_providers_and_preserve_manual_ids_offline() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("selected.toml");
    std::fs::write(&config, "[catalog]\nrefresh=false\n[accounts.local]\nprovider='custom'\ndiscovery=false\nmodels=['private/unknown']\ncatalog='dynamic'\nprotocols={openai-chat={base_url='https://example.com/v1'}}\n").unwrap();
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
        .stdout(predicate::str::contains("Public model").not())
        .stderr(predicate::str::contains("stale"));
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["models", "list", "--catalog", "dynamic", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("models.dev"));
}

#[test]
fn disabled_catalog_keeps_manual_model_queries_working_and_refresh_fails_actionably() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("selected.toml");
    std::fs::write(&config, "[catalog]\nenabled=false\n[accounts.local]\nprovider='custom'\ndiscovery=false\nmodels=['private/model']\nprotocols={openai-chat={base_url='https://example.com/v1'}}\n").unwrap();
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
fn invalid_permission_rules_fail_before_protocol_traffic_without_exposing_values() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("selected.toml");
    for rules in [
        format!("permissions='{SECRET}'"),
        format!("permissions=[{{action='read',resource='{SECRET}',effect='invalid'}}]"),
        format!("permissions=[{{action='read',resource='*',effect='allow',{SECRET}=true}}]"),
    ] {
        std::fs::write(&config, rules).unwrap();
        isolated(dir.path())
            .env("BEE_CONFIG", &config)
            .write_stdin("")
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("bee::permissions::setting"))
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
        .env("XDG_STATE_HOME", dir.join("state"))
        .env_remove("BEE_CONFIG")
        .env_remove("BEE_PROFILE")
        .env_remove("BEE_TEST_KEY")
        .current_dir(dir);
    command
}

const SECRET: &str = "sk-super-secret-token";

#[test]
fn model_listing_aggregates_active_accounts_with_qualified_ids_and_display_labels() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("accounts.toml");
    std::fs::write(&config, "[catalog]\nenabled=false\n[accounts.work]\nprovider='openai'\ndisplay_name='Work'\ndiscovery=false\nmodels=['vendor/shared']\nmodel_metadata={'vendor/shared'={display_name='Shared name'}}\n[accounts.personal]\nprovider='anthropic'\ndiscovery=false\nmodels=['vendor/shared']\n[accounts.hidden]\nprovider='openai'\nenabled=false\ndiscovery=false\nmodels=['hidden-model']\n").unwrap();
    let output = isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["models", "list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let models = output["result"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    assert!(
        models
            .iter()
            .any(|model| model["id"] == "work/vendor/shared"
                && model["display_name"] == "Work / Shared name")
    );
    assert!(
        models
            .iter()
            .any(|model| model["id"] == "personal/vendor/shared"
                && model["display_name"] == "Anthropic / vendor/shared")
    );
    assert_eq!(output["result"]["accounts"].as_array().unwrap().len(), 2);
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["models", "list", "work"])
        .assert()
        .success()
        .stdout(predicate::str::contains("work/vendor/shared"))
        .stdout(predicate::str::contains("personal/vendor/shared").not());
}

#[cfg(unix)]
#[test]
fn ambiguous_omitted_auth_targets_never_mutate_credentials_even_with_a_configured_model() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("accounts.toml");
    std::fs::write(&config, "model='one/test'\n[accounts.one]\nprovider='openai'\ndiscovery=false\nmodels=['test']\ncredential={store=true}\n[accounts.two]\nprovider='anthropic'\ncredential={store=true}\n").unwrap();
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["auth", "login", "one", "--with-token"])
        .write_stdin(SECRET)
        .assert()
        .success();
    let path = dir.path().join("data/bee/credentials.json");
    let before = std::fs::read(&path).unwrap();
    for args in [
        vec!["auth", "login", "--with-token"],
        vec!["auth", "logout"],
    ] {
        isolated(dir.path())
            .env("BEE_CONFIG", &config)
            .args(args)
            .write_stdin("replacement-secret")
            .assert()
            .failure()
            // Miette may wrap the account/ID phrase at the terminal's display width.
            .stderr(predicate::str::contains("specify an account"))
            .stderr(predicate::str::contains("omitted login/logout requires"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[cfg(unix)]
#[test]
fn explicit_disabled_account_authentication_preserves_configuration_and_stays_out_of_status() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("accounts.toml");
    let original = "[accounts.active]\nprovider='openai'\ncredential={store=true}\n[accounts.disabled]\nprovider='anthropic'\nenabled=false\ncredential={store=true}\n";
    std::fs::write(&config, original).unwrap();
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["auth", "login", "disabled", "--with-token"])
        .write_stdin(SECRET)
        .assert()
        .success();
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["auth", "status", "disabled"])
        .assert()
        .success()
        .stdout(predicate::str::contains("stored credential present"));
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("active"))
        .stdout(predicate::str::contains("disabled").not());
    // The disabled account cannot make a sole-active omitted target ambiguous.
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["auth", "login", "--with-token"])
        .write_stdin("active-secret")
        .assert()
        .success();
    isolated(dir.path())
        .env("BEE_CONFIG", &config)
        .args(["auth", "logout", "disabled"])
        .assert()
        .success();
    assert_eq!(std::fs::read_to_string(config).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn stored_instance_reauthentication_does_not_require_a_global_configuration_root_or_lock() {
    for no_global_root in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let selected = dir.path().join("managed.toml");
        let original = "[accounts.account]\nprovider='openai'\ncredential={store=true}";
        std::fs::write(&selected, original).unwrap();
        // A directory at the lock path makes accidental lock attempts fail even as root.
        std::fs::create_dir_all(dir.path().join("xdg/bee/config.toml.lock")).unwrap();
        let mut command = isolated(dir.path());
        if no_global_root {
            command
                .env_remove("HOME")
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("USERPROFILE");
        }
        command
            .env("BEE_CONFIG", &selected)
            .args(["auth", "login", "account", "--with-token"])
            .write_stdin(format!("{SECRET}\n"))
            .assert()
            .success()
            .stderr(predicate::str::contains(SECRET).not());
        assert_eq!(std::fs::read_to_string(selected).unwrap(), original);
        assert!(dir.path().join("data/bee/credentials.json").exists());
        assert!(!dir.path().join("xdg/bee/config.toml").exists());
    }
}

#[test]
fn provider_registry_inspection_does_not_load_configuration_or_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let selected = dir.path().join("broken.toml");
    std::fs::write(&selected, format!("invalid={SECRET}")).unwrap();
    for args in [
        vec!["providers", "list"],
        vec!["providers", "show", "github"],
    ] {
        isolated(dir.path())
            .env("BEE_CONFIG", &selected)
            .args(args)
            .assert()
            .success()
            .stdout(predicate::str::contains("GitHub Copilot"))
            .stdout(predicate::str::contains(SECRET).not())
            .stderr(predicate::str::is_empty());
    }
    isolated(dir.path())
        .args(["providers", "show", SECRET])
        .assert()
        .failure()
        .stderr(predicate::str::contains(SECRET).not());
    assert!(!dir.path().join("data").exists());
}

#[cfg(unix)]
#[test]
fn explicit_native_login_imports_environment_then_uses_instance_storage_after_environment_changes()
{
    let dir = tempfile::tempdir().unwrap();
    isolated(dir.path())
        .env("OPENAI_API_KEY", SECRET)
        .args(["auth", "login", "openai"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("now uses Bee's credential store"))
        .stderr(predicate::str::contains(SECRET).not());
    let config = std::fs::read_to_string(dir.path().join("xdg/bee/config.toml")).unwrap();
    assert!(config.contains("[accounts.openai]"));
    assert!(config.contains("provider = \"openai\""));
    assert!(!config.contains("model ="));
    assert!(config.contains("store = true"));
    assert!(!config.contains(SECRET));
    isolated(dir.path())
        .env_remove("OPENAI_API_KEY")
        .args(["auth", "status", "openai"])
        .assert()
        .success()
        .stdout(predicate::str::contains("stored credential present"));
    let credentials: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("data/bee/credentials.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(credentials["providers"]["openai"]["secret"], SECRET);
    assert_eq!(
        credentials["providers"]["openai"]["provider_type"],
        "openai"
    );
    isolated(dir.path())
        .env("OPENAI_API_KEY", "different-account")
        .args(["auth", "logout", "openai"])
        .assert()
        .success();
    isolated(dir.path())
        .env("OPENAI_API_KEY", "different-account")
        .args(["auth", "status", "openai"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no stored credential"));
}

#[cfg(unix)]
#[test]
fn existing_instance_names_win_over_native_types_and_multiple_accounts_remain_independent() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("xdg/bee/config.toml");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, "[accounts.openai]\nprovider='anthropic'\ncredential={env='PERSONAL_KEY'}\n[accounts.work]\nprovider='anthropic'\ncredential={env='WORK_KEY'}\n").unwrap();
    for (instance, variable, secret) in [
        ("openai", "PERSONAL_KEY", "personal-secret"),
        ("work", "WORK_KEY", "work-secret"),
    ] {
        isolated(dir.path())
            .env(variable, secret)
            .args(["auth", "login", instance])
            .assert()
            .success()
            .stderr(predicate::str::contains(secret).not());
    }
    let credentials: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("data/bee/credentials.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        credentials["providers"]["openai"]["provider_type"],
        "anthropic"
    );
    assert_eq!(
        credentials["providers"]["openai"]["secret"],
        "personal-secret"
    );
    assert_eq!(credentials["providers"]["work"]["secret"], "work-secret");
    isolated(dir.path())
        .args(["auth", "logout", "work"])
        .assert()
        .success();
    isolated(dir.path())
        .args(["auth", "status", "openai"])
        .assert()
        .success()
        .stdout(predicate::str::contains("stored credential present"));
}

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
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("bee::config::selected_missing"))
        .stderr(predicate::str::contains("absent.toml"))
        .stderr(predicate::str::contains("create the file, fix the path"))
        .stderr(predicate::str::contains("BEE_CONFIG"));
}

#[test]
fn valid_layered_configuration_does_not_disturb_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("xdg/bee/config.toml");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(
        &global,
        "model='gw/x'\n[accounts.gw]\nprovider='custom'\ndiscovery=false\nprotocols = { openai-chat = { base_url='https://example.com/v1' } }\nmodels = ['x','y']\n",
    )
    .unwrap();
    let project = dir.path().join(".config/bee.toml");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&project, "model='gw/y'\n").unwrap();

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
        format!("[accounts.p]\nprovider='custom'\nprotocols = {{ openai-chat = {{ base_url = \"https://u:{SECRET}@x.example\" }} }}\n"),
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
            "accounts.p.protocols.openai-chat.base_url",
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
        std::fs::write(&project, format!("[accounts.p]\nprovider='custom'\nprotocols={{openai-chat={{base_url='https://example.com/v1'}}}}\nmodel_protocols={{'{SECRET}'={value}}}")).unwrap();
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
model = "stored/test"

[accounts.stored]
provider = "custom"
discovery = false
models = ["test"]
protocols = { openai-chat = { base_url = "https://api.openai.com/v1" } }
credential = { store = true }

[accounts.browser]
provider = "openrouter"
protocols = { openai-chat = { base_url = "https://openrouter.ai/api/v1" } }
credential = { store = true, login = "openrouter" }

[accounts.env]
provider = "custom"
protocols = { anthropic-messages = { base_url = "https://api.anthropic.com" } }
credential = { env = "BEE_TEST_KEY" }

[accounts.local]
provider = "custom"
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
fn named_profiles_scope_auth_and_models_without_changing_connection_credential_keys() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join(".config/bee.toml");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&project, "[catalog]\nenabled=false\n[accounts.work-api]\nprovider='openai'\ncredential={store=true}\ndiscovery=false\nmodels=['profile-work']\n[accounts.personal-api]\nprovider='openai'\ncredential={store=true}\ndiscovery=false\nmodels=['base-personal']\n[profiles.work]\naccounts=['work-api']\nmodel='work-api/profile-work'\n[profiles.personal]\naccounts=['personal-api']\n").unwrap();
    for (profile, token) in [("work", "work-secret"), ("personal", "personal-secret")] {
        isolated(dir.path())
            .env("BEE_PROFILE", profile)
            .args(["auth", "login", "--with-token"])
            .write_stdin(token)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(token).not());
    }
    let stored: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("data/bee/credentials.json")).unwrap(),
    )
    .unwrap();
    assert!(stored["providers"].get("work-api").is_some());
    assert!(stored["providers"].get("personal-api").is_some());
    assert!(stored["providers"].get("work").is_none());
    isolated(dir.path())
        .env("BEE_PROFILE", "work")
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("work-api"))
        .stdout(predicate::str::contains("personal-api").not());
    isolated(dir.path())
        .env("BEE_PROFILE", "work")
        .args(["auth", "status", "personal-api"])
        .assert()
        .success();
    isolated(dir.path())
        .env("BEE_PROFILE", "work")
        .args(["models", "list", "personal-api"])
        .assert()
        .failure();
    isolated(dir.path())
        .env("BEE_PROFILE", "work")
        .args(["models", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("profile-work"))
        .stdout(predicate::str::contains("base-personal").not());
    isolated(dir.path())
        .env("BEE_PROFILE", "personal")
        .args(["models", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("base-personal"))
        .stdout(predicate::str::contains("profile-work").not());
    isolated(dir.path())
        .env("BEE_PROFILE", "work")
        .args(["auth", "logout"])
        .assert()
        .success();
    isolated(dir.path())
        .env("BEE_PROFILE", "personal")
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("stored credential present"));
}

#[cfg(unix)]
#[test]
fn a_token_can_be_stored_checked_and_forgotten_without_it_ever_being_printed() {
    let dir = configured();
    let file = dir.path().join("data/bee/credentials.json");

    isolated(dir.path())
        .args(["auth", "login", "stored", "--with-token"])
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
        .args(["auth", "logout", "stored"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Removed the stored credential"));
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(document["providers"], serde_json::json!({}));
    assert!(
        !document["profile_revisions"]
            .as_object()
            .unwrap()
            .is_empty(),
        "logout tombstones prevent an older pending login from restoring authority"
    );

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
        "[accounts.env]\nprovider='custom'\nprotocols = { openai-chat = { base_url='https://example.com/v1' } }\ncredential = { env = \"BEE_TEST_KEY\" }\n",
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

#[cfg(unix)]
#[test]
fn opencode_failed_token_onboarding_never_persists_a_connection_or_opens_a_browser_flow() {
    for offering in ["opencode", "opencode-go"] {
        let dir = tempfile::tempdir().unwrap();
        isolated(dir.path())
            .args(["auth", "login", offering, "--with-token"])
            .write_stdin("\n")
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("bee::auth::empty_token"));
        isolated(dir.path())
            .args(["auth", "login", offering])
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("bee::auth::missing_env"));
        assert!(!dir.path().join("data/bee/credentials.json").exists());
        assert!(!dir.path().join("config/bee/config.toml").exists());
    }
}

#[test]
fn project_environment_authority_must_be_removed_before_switching_to_the_store() {
    let dir = configured();

    isolated(dir.path())
        .args(["auth", "login", "env", "--with-token"])
        .write_stdin(format!("{SECRET}\n"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("bee::config::write"))
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
fn chatgpt_profiles_reject_api_token_and_paste_code_login_without_echoing_input() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join(".config/bee.toml");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(project, "[accounts.subscription]\nprovider='chatgpt'").unwrap();
    for flag in ["--with-token", "--paste-code"] {
        isolated(dir.path())
            .args(["auth", "login", "subscription", flag])
            .write_stdin(format!("{SECRET}\n"))
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("bee::auth::chatgpt"))
            .stderr(predicate::str::contains("browser authorization"))
            .stderr(predicate::str::contains(SECRET).not());
    }
    assert!(!dir.path().join("data/bee/credentials.json").exists());
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
        .args(["auth", "login", "stored", "--with-token"])
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
            "No active accounts are configured",
        ));
}
