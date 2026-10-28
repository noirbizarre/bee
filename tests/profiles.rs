//! Independent account membership and qualified profile defaults.

use bee::account::{Accounts as Providers, ModelRef};
use bee::config::{Config, Environment, project_config_path};

const CONNECTIONS: &str = "\n[accounts.one]\nprovider='openai'\ndiscovery=false\nmodels=['base','work-model']\n[accounts.two]\nprovider='openai'\nenabled=false\ndiscovery=false\nmodels=['vendor/model']\ncredential={env='OTHER_ACCOUNT'}\n";

fn load(text: &str) -> bee::Result<Providers> {
    let dir = tempfile::tempdir().unwrap();
    let path = project_config_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    Providers::from_config(&Config::load(
        &Environment::new(None, None, None),
        dir.path(),
    )?)
}

#[test]
fn fresh_installations_and_multiple_accounts_do_not_create_a_model_default() {
    assert_eq!(load("").unwrap().profile_name(), "default");
    assert_eq!(Providers::default().profile_name(), "default");
    for groups in ["", "[profiles]\n"] {
        let providers = load(&format!("{groups}{CONNECTIONS}")).unwrap();
        assert_eq!(providers.profile_name(), "default");
        assert_eq!(providers.names().collect::<Vec<_>>(), ["one"]);
        assert_eq!(providers.configured_model(), None);
    }
    let multiple = load(&format!(
        "[profiles.default]\naccounts=['one','two']\n{CONNECTIONS}"
    ))
    .unwrap();
    assert_eq!(multiple.names().collect::<Vec<_>>(), ["one", "two"]);
    assert_eq!(multiple.configured_model(), None);
    assert!(load(&format!("model='absent/model'\n{CONNECTIONS}")).is_err());
}

#[test]
fn explicit_profiles_scope_connections_without_requiring_a_global_default() {
    let groups = "[profiles.work]\naccounts=['one']\n[profiles.personal]\naccounts=['two']\nmodel='two/vendor/model'\n";
    let work = load(&format!("profile='work'\n{groups}{CONNECTIONS}")).unwrap();
    assert_eq!(work.profile_name(), "work");
    assert_eq!(work.names().collect::<Vec<_>>(), ["one"]);
    assert_eq!(work.configured_model(), None);
    assert!(work.get("two").is_none());
    let default = load(&format!("{groups}{CONNECTIONS}")).unwrap();
    assert_eq!(default.profile_name(), "default");
    assert_eq!(default.names().collect::<Vec<_>>(), ["one"]);
    let personal = load(&format!("profile='personal'\n{groups}{CONNECTIONS}")).unwrap();
    assert_eq!(personal.names().collect::<Vec<_>>(), ["two"]);
    assert_eq!(
        personal.configured_model(),
        Some(&ModelRef::parse("two/vendor/model").unwrap())
    );
    assert!(load(&format!("profile='absent'\n{groups}{CONNECTIONS}")).is_err());
}

#[test]
fn profile_defaults_are_independent_without_mutating_account_declarations() {
    let groups = "model='one/base'\n[profiles.work]\naccounts=['one']\nmodel='one/work-model'\n[profiles.personal]\naccounts=['one']\n";
    let work = load(&format!("profile='work'\n{groups}{CONNECTIONS}")).unwrap();
    let personal = load(&format!("profile='personal'\n{groups}{CONNECTIONS}")).unwrap();
    let work_connection = work.get("one").unwrap();
    let personal_connection = personal.get("one").unwrap();
    assert_eq!(
        work.configured_model(),
        Some(&ModelRef::parse("one/work-model").unwrap())
    );
    assert_eq!(
        personal.configured_model(),
        Some(&ModelRef::parse("one/base").unwrap())
    );
    assert_eq!(work_connection.models(), ["base", "work-model"]);
    assert_eq!(personal_connection.models(), work_connection.models());
    assert_eq!(
        work_connection.credential(),
        personal_connection.credential()
    );
    assert_eq!(work_connection.name(), personal_connection.name());
}

#[test]
fn malformed_membership_and_preferences_fail_even_for_inactive_profiles() {
    for group in [
        "accounts=['missing']",
        "accounts=['one','one']",
        "accounts=['one']\nmodel='two/vendor/model'",
        "accounts=['one']\nprovider='two'",
        "accounts=['one']\n[profiles.bad.models.two]\nmodel='x'",
        "accounts=['one']\nmodel=''",
        "accounts=['one']\nmodel='one/undeclared'",
        "accounts='one'",
        "accounts=['one']\nunknown='secret-canary'",
    ] {
        let error = load(&format!("[profiles.bad]\n{group}\n{CONNECTIONS}")).unwrap_err();
        assert!(!format!("{error} {error:?}").contains("secret-canary"));
    }
    let valid = load(&format!(
        "profile='work'\n[profiles.work]\naccounts=['one','two']\nmodel='two/vendor/model'\n{CONNECTIONS}"
    ))
    .unwrap();
    assert_eq!(
        valid.configured_model().map(|model| model.account.as_str()),
        Some("two")
    );
}

#[test]
fn every_profile_inherits_the_enabled_baseline_independently_of_the_default_profile() {
    let text = "[accounts.omitted]\nprovider='openai'\n[accounts.enabled]\nprovider='openai'\nenabled=true\n[accounts.disabled]\nprovider='openai'\nenabled=false\n[profiles.default]\naccounts=['disabled']\n[profiles.inherited]\n[profiles.expanded]\naccounts=['disabled','enabled','omitted']\n[profiles.empty]\naccounts=[]\n";
    for (profile, expected) in [
        ("default", vec!["disabled"]),
        ("inherited", vec!["enabled", "omitted"]),
        ("expanded", vec!["disabled", "enabled", "omitted"]),
        ("empty", vec![]),
    ] {
        let accounts = load(&format!("profile='{profile}'\n{text}")).unwrap();
        assert_eq!(accounts.names().collect::<Vec<_>>(), expected);
        assert_eq!(accounts.configured_model(), None);
    }
}

#[test]
fn excluded_root_defaults_are_dormant_while_explicit_profile_defaults_require_membership() {
    for (root_model, profile, expected) in [
        ("two/vendor/model", "", None),
        ("one/base", "profile='excluded'\n", None),
        ("one/base", "", Some("one/base")),
    ] {
        let accounts = load(&format!(
            "{profile}model='{root_model}'\n[profiles.excluded]\naccounts=['two']\n{CONNECTIONS}"
        ))
        .unwrap();
        assert_eq!(
            accounts
                .configured_model()
                .map(ModelRef::qualified)
                .as_deref(),
            expected
        );
    }
    assert!(
        load(&format!(
            "[profiles.inactive]\naccounts=['one']\nmodel='two/vendor/model'\n{CONNECTIONS}"
        ))
        .is_err()
    );
    // Dormancy affects selection only; an invalid root reference is still an error.
    assert!(load(&format!("model='two/undeclared'\n{CONNECTIONS}")).is_err());
}

#[test]
fn qualified_references_split_once_and_preserve_opaque_raw_model_suffixes() {
    let reference = ModelRef::parse("Work/vendor/Model.v2").unwrap();
    assert_eq!(reference.account, "Work");
    assert_eq!(reference.model, "vendor/Model.v2");
    assert_eq!(reference.qualified(), "Work/vendor/Model.v2");
    for invalid in [
        "unqualified",
        "/model",
        "account/",
        " account/model",
        "account /model",
        "account/ model",
        "account/model ",
        "account/model\n",
    ] {
        assert!(ModelRef::parse(invalid).is_err());
    }
    for id in ["work/team", " work", "work "] {
        assert!(load(&format!("[accounts.'{id}']\nprovider='openai'")).is_err());
    }
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
fn higher_layers_replace_membership_and_account_models_independently() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("global/bee/config.toml");
    let project = project_config_path(root.path());
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&global, format!("profile='work'\n[profiles.work]\naccounts=['one','two']\nmodel='one/base'\n{CONNECTIONS}")).unwrap();
    std::fs::write(
        &project,
        "[profiles.work]\naccounts=['one']\n[accounts.one]\nmodels=['base','new-suggestion']\n",
    )
    .unwrap();
    let env = Environment::new(Some(root.path().join("global")), None, None);
    let providers = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
    assert_eq!(providers.names().collect::<Vec<_>>(), ["one"]);
    let connection = providers.get("one").unwrap();
    assert_eq!(
        providers.configured_model(),
        Some(&ModelRef::parse("one/base").unwrap())
    );
    assert_eq!(connection.models(), ["base", "new-suggestion"]);
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
