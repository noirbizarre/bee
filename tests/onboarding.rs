//! Explicit built-in login changes only user-owned declarations and membership.

use bee::config::{Config, Environment, persist, project_config_path};
use bee::provider::Providers;

fn fixture(text: &str) -> (tempfile::TempDir, Environment) {
    let root = tempfile::tempdir().unwrap();
    let env = Environment::new(Some(root.path().join("config")), None, None);
    let path = env.global_config_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    (root, env)
}

#[test]
#[cfg(unix)]
fn opencode_onboarding_uses_independent_stored_keys_and_preserves_existing_env_authority() {
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
        assert!(profile.discover_models());
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
    let original = "[providers.opencode]\npreset='opencode'\n";
    let (root, env) = fixture(original);
    let plan = persist::prepare(&env, root.path(), "opencode").unwrap();
    assert!(bee::auth::require_store(plan.providers().get("opencode").unwrap()).is_err());
    plan.commit(&env, root.path()).unwrap();
    assert_eq!(
        std::fs::read_to_string(env.global_config_path().unwrap()).unwrap(),
        original
    );
}

#[test]
fn built_in_login_preflight_is_read_only_and_commit_preserves_comments_and_existing_default() {
    let original =
        "# user's header\n[providers.old] # keep this comment\npreset='openai'\nmodel='custom'\n";
    let (root, env) = fixture(original);
    let plan = persist::prepare(&env, root.path(), "openrouter").unwrap();
    assert_eq!(plan.providers().default_name(), Some("old"));
    assert!(
        plan.providers()
            .get("openrouter")
            .unwrap()
            .discover_models()
    );
    let path = env.global_config_path().unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    plan.commit(&env, root.path()).unwrap();
    let updated = std::fs::read_to_string(&path).unwrap();
    assert!(updated.contains("# user's header"));
    assert!(updated.contains("[providers.old] # keep this comment"));
    assert!(updated.contains("model='custom'"));
    let providers = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
    assert_eq!(providers.default_name(), Some("old"));
    assert_eq!(
        providers.get("old").unwrap().default_model(),
        Some("custom")
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
        "profile='work'\n[providers.old]\npreset='openai'\n[profiles.work]\nconnections=['old']\n[profiles.personal]\nconnections=[]\n",
    );
    persist::prepare(&env, root.path(), "chatgpt")
        .unwrap()
        .commit(&env, root.path())
        .unwrap();
    let config = Config::load(&env, root.path()).unwrap();
    let providers = Providers::from_config(&config).unwrap();
    assert_eq!(providers.names().collect::<Vec<_>>(), ["chatgpt", "old"]);
    assert_eq!(providers.default_name(), Some("old"));
    assert!(
        config
            .get("profiles.personal.connections")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn project_membership_overrides_are_detected_before_user_configuration_changes() {
    let original = "[profiles.work]\nconnections=[]\n";
    let (root, env) = fixture(original);
    let project = project_config_path(root.path());
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(
        &project,
        "profile='work'\n[profiles.work]\nconnections=[]\n",
    )
    .unwrap();
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
        "profiles={default={connections=[]}}\n",
        "providers={old={preset='openai'}}\n",
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
