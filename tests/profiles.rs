//! Named profile selection, migration and connection isolation.

use bee::config::{Config, Environment, project_config_path};
use bee::provider::Providers;

const CONNECTIONS: &str = "\n[providers.one]\ntype='openai'\nmodel='base'\nmodels=['suggestion']\n[providers.two]\ntype='openai'\ncredential={env='OTHER_ACCOUNT'}\n";

fn load(text: &str) -> bee::Result<Providers> {
    let dir = tempfile::tempdir().unwrap();
    let path = project_config_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    Providers::from_config(&Config::load(&Environment::new(None, None, None), dir.path()).unwrap())
}

#[test]
fn fresh_and_legacy_installations_have_a_default_without_rewriting_configuration() {
    assert_eq!(load("").unwrap().profile_name(), "default");
    assert_eq!(Providers::default().profile_name(), "default");
    for groups in ["", "[profiles]\n"] {
        let providers = load(&format!("provider='one'\n{groups}{CONNECTIONS}")).unwrap();
        assert_eq!(providers.profile_name(), "default");
        assert_eq!(providers.names().collect::<Vec<_>>(), ["one", "two"]);
        assert_eq!(providers.default_name(), Some("one"));
        assert_eq!(providers.get("one").unwrap().default_model(), Some("base"));
    }
    assert!(
        load(CONNECTIONS).is_err(),
        "legacy ambiguity remains an error"
    );
    assert!(load(&format!("provider='absent'\n{CONNECTIONS}")).is_err());
}

#[test]
fn explicit_profiles_scope_connections_without_requiring_a_global_default() {
    let groups = "[profiles.work]\nconnections=['one']\n[profiles.personal]\nconnections=['two']\n";
    let work = load(&format!("profile='work'\n{groups}{CONNECTIONS}")).unwrap();
    assert_eq!(work.profile_name(), "work");
    assert_eq!(work.names().collect::<Vec<_>>(), ["one"]);
    assert_eq!(work.default_name(), Some("one"));
    assert!(work.get("two").is_none());
    let default = load(&format!("{groups}{CONNECTIONS}")).unwrap();
    assert_eq!(default.profile_name(), "default");
    assert!(
        default.is_empty(),
        "implicit default cannot expose unreferenced accounts"
    );
    assert!(load(&format!("profile='absent'\n{groups}{CONNECTIONS}")).is_err());
}

#[test]
fn model_preferences_are_independent_and_inherit_only_omitted_connection_fields() {
    let groups = "[profiles.work]\nconnections=['one']\n[profiles.work.models.one]\nmodel='work-model'\nmodels=[]\n[profiles.personal]\nconnections=['one']\n[profiles.personal.models.one]\nmodels=['personal-suggestion']\n";
    let work = load(&format!("profile='work'\n{groups}{CONNECTIONS}")).unwrap();
    let personal = load(&format!("profile='personal'\n{groups}{CONNECTIONS}")).unwrap();
    let work_connection = work.get("one").unwrap();
    let personal_connection = personal.get("one").unwrap();
    assert_eq!(work_connection.default_model(), Some("work-model"));
    assert!(work_connection.models().is_empty());
    assert_eq!(personal_connection.default_model(), Some("base"));
    assert_eq!(personal_connection.models(), ["personal-suggestion"]);
    assert_eq!(
        work_connection.credential(),
        personal_connection.credential()
    );
    assert_eq!(work_connection.name(), personal_connection.name());
}

#[test]
fn malformed_membership_and_preferences_fail_even_for_inactive_profiles() {
    for group in [
        "connections=['missing']",
        "connections=['one','one']",
        "connections=['one','two']",
        "connections=['one']\nprovider='two'",
        "connections=['one']\n[profiles.bad.models.two]\nmodel='x'",
        "connections=['one']\n[profiles.bad.models.one]\nmodel=''",
        "connections=['one']\n[profiles.bad.models.one]\nmodels=['x',' ']",
        "connections='one'",
        "connections=['one']\nunknown='secret-canary'",
    ] {
        let error = load(&format!("[profiles.bad]\n{group}\n{CONNECTIONS}")).unwrap_err();
        assert!(!format!("{error} {error:?}").contains("secret-canary"));
    }
    let valid = load(&format!(
        "profile='work'\n[profiles.work]\nconnections=['one','two']\nprovider='two'\n{CONNECTIONS}"
    ))
    .unwrap();
    assert_eq!(valid.default_name(), Some("two"));
}

#[test]
fn the_profile_environment_layer_respects_file_precedence_and_origins() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("config/bee/config.toml");
    let selected = root.path().join("selected.toml");
    let project = project_config_path(root.path());
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&global, "profile='global'").unwrap();
    let env = Environment::new(Some(root.path().join("config")), None, None);
    assert_eq!(
        Config::load(&env, root.path())
            .unwrap()
            .get("profile")
            .unwrap()
            .as_str(),
        Some("global")
    );
    let env = env.with_variable("BEE_PROFILE", "environment");
    let merged = Config::load(&env, root.path()).unwrap();
    assert_eq!(merged.get("profile").unwrap().as_str(), Some("environment"));
    assert_eq!(
        merged.origin("profile").unwrap().to_str(),
        Some("<environment BEE_PROFILE>")
    );
    assert_eq!(merged.sources(), [global.as_path()]);
    std::fs::write(&selected, "profile='selected'").unwrap();
    let env = Environment::new(Some(root.path().join("config")), None, Some(selected))
        .with_variable("BEE_PROFILE", "environment");
    assert_eq!(
        Config::load(&env, root.path())
            .unwrap()
            .get("profile")
            .unwrap()
            .as_str(),
        Some("selected")
    );
    std::fs::write(&project, "profile='project'").unwrap();
    assert_eq!(
        Config::load(&env, root.path())
            .unwrap()
            .get("profile")
            .unwrap()
            .as_str(),
        Some("project")
    );
}

#[test]
fn environment_selection_without_a_global_path_is_validated_without_leaking_values() {
    let root = tempfile::tempdir().unwrap();
    let env = Environment::new(None, None, None).with_variable("BEE_PROFILE", "secret-canary");
    let config = Config::load(&env, root.path()).unwrap();
    let error = Providers::from_config(&config).unwrap_err();
    assert!(!format!("{error} {error:?} {config:?} {env:?}").contains("secret-canary"));
    assert!(error.to_string().contains("BEE_PROFILE"));
    let empty = Environment::new(None, None, None).with_variable("BEE_PROFILE", "");
    assert_eq!(
        Providers::from_config(&Config::load(&empty, root.path()).unwrap())
            .unwrap()
            .profile_name(),
        "default"
    );
}

#[test]
fn higher_layers_replace_membership_and_merge_model_preferences_independently() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("global/bee/config.toml");
    let project = project_config_path(root.path());
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&global, format!("profile='work'\n[profiles.work]\nconnections=['one','two']\nprovider='one'\n[profiles.work.models.one]\nmodel='inherited-profile-model'\nmodels=['old-suggestion']\n{CONNECTIONS}")).unwrap();
    std::fs::write(&project, "[profiles.work]\nconnections=['one']\n[profiles.work.models.one]\nmodels=['new-suggestion']\n").unwrap();
    let env = Environment::new(Some(root.path().join("global")), None, None);
    let providers = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
    assert_eq!(providers.names().collect::<Vec<_>>(), ["one"]);
    let connection = providers.get("one").unwrap();
    assert_eq!(connection.model(), Some("inherited-profile-model"));
    assert_eq!(connection.models(), ["new-suggestion"]);
}

#[test]
fn profile_environment_merging_retains_type_conflict_validation() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("bee/config.toml");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, "profile=12").unwrap();
    let env = Environment::new(Some(root.path().into()), None, None)
        .with_variable("BEE_PROFILE", "secret-canary");
    let error = Config::load(&env, root.path()).unwrap_err();
    assert!(!format!("{error} {error:?}").contains("secret-canary"));
    assert!(matches!(error, bee::Error::ConfigConflict { .. }));
}
