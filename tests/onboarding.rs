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
