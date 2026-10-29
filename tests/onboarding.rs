//! Explicit built-in login changes only user-owned declarations and membership.

use bee::account::Accounts as Providers;
use bee::config::{Config, Environment, persist, project_config_path};

fn fixture(text: &str) -> (tempfile::TempDir, Environment) {
    let root = tempfile::tempdir().unwrap();
    let env = Environment::new(Some(root.path().join("config")), None, None);
    let path = env.global_config_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    (root, env)
}

#[test]
fn scaleway_login_distinguishes_inherited_and_explicit_environment_authority() {
    for (declaration, inherited) in [
        ("", true),
        ("credential={env='SCW_SECRET_KEY'}", false),
        ("credential={env='WORK_KEY'}", false),
    ] {
        let original = format!("[accounts.work]\nprovider='scaleway'\n{declaration}\n");
        let (root, env) = fixture(&original);
        let draft = persist::begin_login(&env, root.path(), Some("work")).unwrap();
        assert!(draft.missing_fields().is_empty());
        let plan = draft
            .complete(&env, root.path(), toml::Table::new())
            .unwrap();
        assert_eq!(plan.inherits_environment(), inherited);
        assert!(plan.import_credential(&env).is_err());
        // Source changes during a pending login must not publish a different account's authority.
        std::fs::write(
            env.global_config_path().unwrap(),
            "[accounts.work]\nprovider='scaleway'\ncredential={env='CHANGED_KEY'}",
        )
        .unwrap();
        assert!(plan.check(&env, root.path()).is_err());
        assert!(plan.commit(&env, root.path()).is_err());
    }
}

fn cloudflare_inputs() -> toml::Table {
    toml::Table::from_iter([
        (
            "account_id".into(),
            toml::Value::String("private-account".into()),
        ),
        (
            "gateway_id".into(),
            toml::Value::String("private-gateway".into()),
        ),
    ])
}

#[test]
fn cloudflare_login_collects_only_missing_fields_and_publishes_settings_after_preflight() {
    let original = "# keep this comment\n[accounts.work]\nprovider='cloudflare'\nenabled=false\ncredential={env='WORK_TOKEN'}\n[profiles.default]\naccounts=['work']\n";
    let (root, env) = fixture(original);
    let env = env.with_variable("WORK_TOKEN", "private-token");
    assert!(Providers::from_config(&Config::load(&env, root.path()).unwrap()).is_err());
    let draft = persist::begin_login(&env, root.path(), None).unwrap();
    assert_eq!(draft.name(), "work");
    assert_eq!(
        draft
            .missing_fields()
            .iter()
            .map(|field| field.name)
            .collect::<Vec<_>>(),
        ["account_id", "gateway_id"]
    );
    let plan = draft
        .complete(&env, root.path(), cloudflare_inputs())
        .unwrap();
    let credential = plan.import_credential(&env).unwrap().unwrap();
    assert_eq!(credential.secret().expose(), "private-token");
    assert_eq!(
        std::fs::read_to_string(env.global_config_path().unwrap()).unwrap(),
        original
    );
    // Unrelated concurrent edits must survive the delayed configuration publication.
    std::fs::write(
        env.global_config_path().unwrap(),
        format!("{original}\n[agent]\nmax_steps=12\n"),
    )
    .unwrap();
    plan.commit(&env, root.path()).unwrap();
    let text = std::fs::read_to_string(env.global_config_path().unwrap()).unwrap();
    assert!(text.contains("# keep this comment"));
    assert!(!text.contains("private-token"));
    let config = Config::load(&env, root.path()).unwrap();
    let providers = Providers::from_config(&config).unwrap();
    let work = providers.get("work").unwrap();
    assert!(!work.enabled());
    assert_eq!(work.settings()["gateway_id"], "private-gateway");
    assert!(matches!(
        work.credential(),
        Some(bee::provider::CredentialSource::Store { .. })
    ));
    assert_eq!(
        config.get("agent.max_steps").unwrap().as_integer(),
        Some(12)
    );
    assert!(providers.configured_model().is_none());
    assert!(
        persist::begin_login(&env, root.path(), Some("work"))
            .unwrap()
            .missing_fields()
            .is_empty()
    );
}

#[test]
fn cloudflare_first_login_and_partially_configured_login_do_not_invent_routing_context() {
    let (root, env) = fixture("");
    let draft = persist::begin_login(&env, root.path(), Some("cloudflare")).unwrap();
    let plan = draft
        .complete(&env, root.path(), cloudflare_inputs())
        .unwrap();
    plan.commit(&env, root.path()).unwrap();
    let providers = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
    assert!(providers.get("cloudflare").unwrap().models().is_empty());
    let original =
        "[accounts.work]\nprovider='cloudflare'\nsettings={account_id='already-configured'}\n";
    let (root, env) = fixture(original);
    let draft = persist::begin_login(&env, root.path(), None).unwrap();
    assert_eq!(draft.missing_fields().len(), 1);
    assert!(draft.validate_input("account_id", "replacement").is_err());
    let mut additions = cloudflare_inputs();
    additions.remove("account_id");
    let plan = draft.complete(&env, root.path(), additions).unwrap();
    assert_eq!(
        plan.accounts().get("work").unwrap().settings()["account_id"],
        "already-configured"
    );
}

#[test]
fn login_drafts_reject_unrelated_incomplete_accounts_and_malformed_present_fields() {
    for text in [
        "[accounts.work]\nprovider='cloudflare'\nsettings={account_id='bad/value'}",
        "[accounts.work]\nprovider='cloudflare'\n[accounts.other]\nprovider='cloudflare'",
    ] {
        let (root, env) = fixture(text);
        assert!(persist::begin_login(&env, root.path(), Some("work")).is_err());
        assert_eq!(
            std::fs::read_to_string(env.global_config_path().unwrap()).unwrap(),
            text
        );
    }
}

#[test]
fn collected_settings_cannot_overwrite_a_concurrent_target_edit() {
    let original = "[accounts.work]\nprovider='cloudflare'\n";
    let (root, env) = fixture(original);
    let draft = persist::begin_login(&env, root.path(), Some("work")).unwrap();
    let plan = draft
        .complete(&env, root.path(), cloudflare_inputs())
        .unwrap();
    let replacement = format!("{original}settings={{account_id='concurrent-account'}}\n");
    std::fs::write(env.global_config_path().unwrap(), &replacement).unwrap();
    assert!(
        plan.commit(&env, root.path())
            .unwrap_err()
            .to_string()
            .contains("authority changed")
    );
    assert_eq!(
        std::fs::read_to_string(env.global_config_path().unwrap()).unwrap(),
        replacement
    );
}

#[test]
fn login_provenance_ignores_a_separate_account_with_a_dotted_id() {
    let original = "[accounts.work]\nprovider='cloudflare'\n[accounts.'work.personal']\nprovider='openai'\ncredential={env='PERSONAL_TOKEN'}\n";
    let (root, env) = fixture(original);
    let draft = persist::begin_login(&env, root.path(), Some("work")).unwrap();
    let plan = draft
        .complete(&env, root.path(), cloudflare_inputs())
        .unwrap();
    let replacement = original.replace(
        "credential={env='PERSONAL_TOKEN'}",
        "credential={store=true}",
    );
    std::fs::write(env.global_config_path().unwrap(), &replacement).unwrap();
    plan.commit(&env, root.path()).unwrap();
    let config = Config::load(&env, root.path()).unwrap();
    let providers = Providers::from_config(&config).unwrap();
    assert_eq!(
        providers.get("work").unwrap().settings()["gateway_id"],
        "private-gateway"
    );
    assert!(matches!(
        providers.get("work.personal").unwrap().credential(),
        Some(bee::provider::CredentialSource::Store { .. })
    ));
}

#[test]
fn pending_login_rejects_a_new_project_authority_override_without_touching_user_configuration() {
    let original = "[accounts.work]\nprovider='openai'\ncredential={env='ORIGINAL_ACCOUNT'}\n";
    let (root, env) = fixture(original);
    let plan = persist::prepare(&env, root.path(), "work").unwrap();
    let project = project_config_path(root.path());
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(
        &project,
        "[accounts.work]\ncredential={env='PROJECT_ACCOUNT'}\n",
    )
    .unwrap();
    let error = plan.commit(&env, root.path()).unwrap_err();
    assert!(error.to_string().contains("authority changed"));
    assert_eq!(
        std::fs::read_to_string(env.global_config_path().unwrap()).unwrap(),
        original
    );
}

#[test]
fn stored_account_reauthentication_does_not_require_a_writable_configuration_lock() {
    let original = "[accounts.work]\nprovider='openai'\ncredential={store=true}\n";
    let (root, env) = fixture(original);
    let path = env.global_config_path().unwrap();
    // A directory at the lock path deterministically refuses writers, even as root.
    std::fs::create_dir(path.with_extension("toml.lock")).unwrap();
    let plan = persist::prepare(&env, root.path(), "work").unwrap();
    assert!(plan.import_credential(&env).unwrap().is_none());
    plan.commit(&env, root.path()).unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[test]
fn environment_authority_is_not_published_when_the_configuration_lock_cannot_open() {
    let original = "[accounts.work]\nprovider='openai'\ncredential={env='IMPORT_KEY'}\n";
    let (root, env) = fixture(original);
    let plan = persist::prepare(&env, root.path(), "work").unwrap();
    let path = env.global_config_path().unwrap();
    std::fs::create_dir(path.with_extension("toml.lock")).unwrap();
    let error = plan.commit(&env, root.path()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot open the configuration lock")
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[test]
fn environment_import_requires_the_declared_secret_and_redacts_the_complete_credential() {
    let (root, env) =
        fixture("[accounts.work]\nprovider='openai'\ncredential={env='IMPORT_KEY'}\n");
    let plan = persist::prepare(&env, root.path(), "work").unwrap();
    let error = plan.import_credential(&env).unwrap_err();
    assert!(
        matches!(error, bee::Error::AuthMissingEnv { ref profile, ref variable } if profile == "work" && variable == "IMPORT_KEY")
    );
    let imported = plan
        .import_credential(
            &env.clone()
                .with_variable("IMPORT_KEY", "private-imported-secret"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(imported.secret().expose(), "private-imported-secret");
    imported
        .validate_for(plan.accounts().get("work").unwrap())
        .unwrap();
    assert!(!format!("{imported:?}").contains("private-imported-secret"));
    assert!(
        std::fs::read_to_string(env.global_config_path().unwrap())
            .unwrap()
            .contains("env='IMPORT_KEY'")
    );
}

#[test]
fn onboarding_without_a_global_config_location_returns_an_actionable_error() {
    let root = tempfile::tempdir().unwrap();
    let env = Environment::new(None, None, None);
    let error = persist::prepare(&env, root.path(), "openai").err().unwrap();
    assert!(
        error
            .to_string()
            .contains("configure HOME or XDG_CONFIG_HOME")
    );
}

#[test]
fn first_login_creates_missing_user_configuration_only_at_commit() {
    let root = tempfile::tempdir().unwrap();
    let env = Environment::new(Some(root.path().join("config")), None, None);
    let path = env.global_config_path().unwrap();
    let plan = persist::prepare(&env, root.path(), "openai").unwrap();
    assert!(!path.exists());
    plan.commit(&env, root.path()).unwrap();
    let accounts = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
    assert!(matches!(
        accounts.get("openai").unwrap().credential(),
        Some(bee::provider::CredentialSource::Store { .. })
    ));
    assert!(accounts.configured_model().is_none());
}

#[cfg(unix)]
#[test]
fn login_refuses_to_replace_a_symlinked_user_configuration() {
    let root = tempfile::tempdir().unwrap();
    let env = Environment::new(Some(root.path().join("config")), None, None);
    let target = root.path().join("managed.toml");
    let original = "# externally managed\n";
    std::fs::write(&target, original).unwrap();
    let path = env.global_config_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let error = persist::prepare(&env, root.path(), "openai").err().unwrap();
    assert!(
        error
            .to_string()
            .contains("requires a regular user configuration file")
    );
    assert!(path.is_symlink());
    assert_eq!(std::fs::read_to_string(target).unwrap(), original);
}

#[test]
fn a_project_environment_binding_cannot_be_overridden_by_stored_user_authority() {
    let (root, env) = fixture("");
    let project = project_config_path(root.path());
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(
        project,
        "[accounts.work]\nprovider='openai'\ncredential={env='PROJECT_KEY'}\n",
    )
    .unwrap();
    let error = persist::prepare(&env, root.path(), "work").err().unwrap();
    assert!(
        error
            .to_string()
            .contains("higher-priority configuration prevents stored credential authority")
    );
    assert_eq!(
        std::fs::read_to_string(env.global_config_path().unwrap()).unwrap(),
        ""
    );
}

#[test]
fn onboarding_rejects_unknown_account_names_without_creating_a_declaration() {
    let (root, env) = fixture("");
    for name in ["custom", "missing-native-provider"] {
        assert!(persist::prepare(&env, root.path(), name).is_err());
    }
    assert_eq!(
        std::fs::read_to_string(env.global_config_path().unwrap()).unwrap(),
        ""
    );
}

#[test]
fn login_does_not_publish_stored_authority_after_the_environment_binding_changes() {
    let (root, env) =
        fixture("[accounts.work]\nprovider='openai'\ncredential={env='ORIGINAL_ACCOUNT'}");
    let plan = persist::prepare(&env, root.path(), "work").unwrap();
    let replacement = "[accounts.work]\nprovider='openai'\ncredential={env='REPLACEMENT_ACCOUNT'}";
    let path = env.global_config_path().unwrap();
    std::fs::write(&path, replacement).unwrap();
    let error = plan.commit(&env, root.path()).unwrap_err();
    assert!(error.to_string().contains("authority changed"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), replacement);
}

#[test]
fn login_retains_unrelated_configuration_edits_made_during_authorization() {
    let original = "[accounts.work]\nprovider='openai'\ncredential={env='ORIGINAL_ACCOUNT'}\n";
    let (root, env) = fixture(original);
    let plan = persist::prepare(&env, root.path(), "work").unwrap();
    let path = env.global_config_path().unwrap();
    std::fs::write(&path, format!("{original}\n[agent]\nmax_steps=12\n")).unwrap();
    plan.commit(&env, root.path()).unwrap();
    let config = Config::load(&env, root.path()).unwrap();
    assert_eq!(
        config
            .get("agent.max_steps")
            .and_then(toml::Value::as_integer),
        Some(12)
    );
    let providers = Providers::from_config(&config).unwrap();
    assert!(matches!(
        providers.get("work").unwrap().credential(),
        Some(bee::provider::CredentialSource::Store { .. })
    ));
}

#[test]
#[cfg(unix)]
fn opencode_onboarding_uses_independent_stored_keys_and_switches_explicit_env_authority() {
    use bee::auth::{Audience, Credential, CredentialStore, FileStore, Secret};
    let (root, env) = fixture("");
    for name in ["opencode", "opencode-go"] {
        persist::prepare(&env, root.path(), name)
            .unwrap()
            .commit(&env, root.path())
            .unwrap();
    }
    let providers = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
    let store = FileStore::new(root.path().join("credentials.json"));
    for name in ["opencode", "opencode-go"] {
        store
            .put(
                Audience::Provider(name),
                &Credential::new(Secret::new(name)),
            )
            .unwrap();
        let profile = providers.get(name).unwrap();
        bee::auth::require_store(profile).unwrap();
        assert_eq!(profile.discovery(), name == "opencode-go");
        assert_eq!(
            bee::auth::resolve(profile, &env, &store, 0)
                .unwrap()
                .unwrap()
                .secret()
                .expose(),
            name
        );
    }
    store.remove(Audience::Provider("opencode-go")).unwrap();
    assert!(bee::auth::resolve(providers.get("opencode").unwrap(), &env, &store, 0).is_ok());
    assert!(bee::auth::resolve(providers.get("opencode-go").unwrap(), &env, &store, 0).is_err());
    let original = "[accounts.opencode]\nprovider='opencode'\n";
    let (root, env) = fixture(original);
    let plan = persist::prepare(&env, root.path(), "opencode").unwrap();
    assert!(bee::auth::require_store(plan.accounts().get("opencode").unwrap()).is_ok());
    assert_eq!(plan.import_env(), Some("OPENCODE_API_KEY"));
    plan.commit(&env, root.path()).unwrap();
    assert!(
        std::fs::read_to_string(env.global_config_path().unwrap())
            .unwrap()
            .contains("store = true")
    );
}

#[test]
fn built_in_login_preflight_is_read_only_and_commit_preserves_comments_and_existing_default() {
    let original = "# user's header\nmodel='old/custom'\n[accounts.old] # keep this comment\nprovider='openai'\ndiscovery=false\nmodels=['custom']\n";
    let (root, env) = fixture(original);
    let plan = persist::prepare(&env, root.path(), "openrouter").unwrap();
    assert_eq!(plan.accounts().configured_model().unwrap().account, "old");
    assert!(plan.accounts().get("openrouter").unwrap().discovery());
    let path = env.global_config_path().unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    plan.commit(&env, root.path()).unwrap();
    let updated = std::fs::read_to_string(&path).unwrap();
    assert!(updated.contains("# user's header"));
    assert!(updated.contains("[accounts.old] # keep this comment"));
    assert!(updated.contains("model='old/custom'"));
    let providers = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
    assert_eq!(
        providers.configured_model().unwrap().qualified(),
        "old/custom"
    );
    persist::prepare(&env, root.path(), "openrouter")
        .unwrap()
        .commit(&env, root.path())
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
}

#[test]
fn login_adds_only_active_profile_membership_and_retains_its_previous_default() {
    let (root, env) = fixture(
        "profile='work'\n[accounts.old]\nprovider='openai'\n[profiles.work]\naccounts=['old']\n[profiles.personal]\naccounts=[]\n",
    );
    persist::prepare(&env, root.path(), "chatgpt")
        .unwrap()
        .commit(&env, root.path())
        .unwrap();
    let config = Config::load(&env, root.path()).unwrap();
    let providers = Providers::from_config(&config).unwrap();
    assert_eq!(providers.names().collect::<Vec<_>>(), ["chatgpt", "old"]);
    assert!(providers.configured_model().is_none());
    assert!(
        config
            .get("profiles.personal.accounts")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn project_membership_overrides_are_detected_before_user_configuration_changes() {
    let original = "[profiles.work]\naccounts=[]\n";
    let (root, env) = fixture(original);
    let project = project_config_path(root.path());
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&project, "profile='work'\n[profiles.work]\naccounts=[]\n").unwrap();
    let error = persist::prepare(&env, root.path(), "openrouter")
        .err()
        .unwrap();
    assert!(error.to_string().contains(&project.display().to_string()));
    assert_eq!(
        std::fs::read_to_string(env.global_config_path().unwrap()).unwrap(),
        original
    );
}

#[test]
fn inline_user_tables_and_empty_profiles_can_be_bootstrapped_without_losing_configuration() {
    for original in [
        "",
        "profiles={default={accounts=[]}}\n",
        "accounts={old={provider='openai'}}\n",
    ] {
        let (root, env) = fixture(original);
        persist::prepare(&env, root.path(), "chatgpt")
            .unwrap()
            .commit(&env, root.path())
            .unwrap();
        let providers = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
        assert!(providers.get("chatgpt").is_some());
    }
}

#[test]
fn existing_disabled_and_excluded_accounts_keep_their_membership_during_login() {
    for credential in ["store=true", "env='KEY'"] {
        let (root, env) = fixture(&format!(
            "profile='work'\n[accounts.work]\nprovider='openai'\nenabled=false\ncredential={{{credential}}}\n[profiles.work]\naccounts=[]\n"
        ));
        let plan = persist::prepare(&env, root.path(), "work").unwrap();
        assert!(plan.accounts().get("work").is_some());
        plan.commit(&env, root.path()).unwrap();
        let config = Config::load(&env, root.path()).unwrap();
        assert!(Providers::from_config(&config).unwrap().is_empty());
        assert_eq!(
            config
                .get("accounts.work.enabled")
                .and_then(toml::Value::as_bool),
            Some(false)
        );
        assert!(config.get("model").is_none());
        assert!(config.get("profiles.work.model").is_none());
        assert!(
            config
                .get("profiles.work.accounts")
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn new_native_accounts_join_only_the_restricted_active_profile_without_creating_a_default() {
    let (root, env) =
        fixture("profile='work'\n[profiles.work]\naccounts=[]\n[profiles.personal]\naccounts=[]\n");
    persist::prepare(&env, root.path(), "openai")
        .unwrap()
        .commit(&env, root.path())
        .unwrap();
    let config = Config::load(&env, root.path()).unwrap();
    assert_eq!(
        config
            .get("accounts.openai.provider")
            .and_then(toml::Value::as_str),
        Some("openai")
    );
    assert!(config.get("accounts.openai.enabled").is_none());
    assert!(config.get("model").is_none());
    assert!(config.get("provider").is_none());
    assert!(config.get("profiles.work.model").is_none());
    assert_eq!(
        Providers::from_config(&config)
            .unwrap()
            .names()
            .collect::<Vec<_>>(),
        ["openai"]
    );
    assert!(
        config
            .get("profiles.personal.accounts")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
}
