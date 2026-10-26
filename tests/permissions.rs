//! Ordered routing and monotonic project restrictions, without ACP or filesystem execution.

use bee::config::{Config, Environment};
use bee::permissions::{Check, Effect, Settings};
use std::path::Path;

fn settings(global: &str, selected: &str, project: &str) -> bee::Result<Settings> {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("bee")).unwrap();
    std::fs::create_dir_all(root.path().join("project/.config")).unwrap();
    std::fs::write(root.path().join("bee/config.toml"), global).unwrap();
    std::fs::write(root.path().join("selected.toml"), selected).unwrap();
    std::fs::write(root.path().join("project/.config/bee.toml"), project).unwrap();
    let environment = Environment::new(
        Some(root.path().into()),
        Some("/home/test".into()),
        Some(root.path().join("selected.toml")),
    );
    Settings::from_config(
        &Config::load(&environment, &root.path().join("project"))?,
        &environment,
    )
}

fn rule(action: &str, resource: &str, effect: &str) -> String {
    format!("[[permissions]]\naction = {action:?}\nresource = {resource:?}\neffect = {effect:?}\n")
}

#[test]
fn defaults_allow_explicit_read_only_checks_and_ask_for_unknown_operations() {
    let settings = Settings::default();
    let cwd = Path::new("/workspace");
    assert_eq!(
        settings.evaluate(&[Check::path("read", "file", Effect::Allow)], cwd),
        Effect::Allow
    );
    assert_eq!(
        settings.evaluate(&[Check::command("echo hi")], cwd),
        Effect::Ask
    );
    assert_eq!(
        settings.evaluate(&[Check::value("custom", "*", Effect::Ask)], cwd),
        Effect::Ask
    );
    assert_eq!(settings.evaluate(&[], cwd), Effect::Ask);
}

#[test]
fn last_matching_trusted_rules_are_appended_across_configuration_files() {
    let global = rule("shell", "*", "deny") + &rule("shell", "git *", "ask");
    let selected = rule("shell", "git status *", "allow");
    let settings = settings(&global, &selected, "").unwrap();
    for (command, effect) in [
        ("git status", Effect::Allow),
        ("git status --short", Effect::Allow),
        ("git push", Effect::Ask),
        ("curl example", Effect::Deny),
    ] {
        assert_eq!(
            settings.evaluate(&[Check::command(command)], Path::new("/workspace")),
            effect
        );
    }
}

#[test]
fn project_rules_cannot_erase_or_weaken_trusted_rules_but_can_tighten_them() {
    let trusted =
        rule("shell", "*", "deny") + &rule("edit", "*", "ask") + &rule("read", "*", "allow");
    for project in ["permissions = []".to_owned(), rule("*", "*", "allow")] {
        let settings = settings(&trusted, "", &project).unwrap();
        assert_eq!(
            settings.evaluate(&[Check::command("git push")], Path::new("/workspace")),
            Effect::Deny
        );
        assert_eq!(
            settings.evaluate(
                &[Check::path("edit", "file", Effect::Ask)],
                Path::new("/workspace")
            ),
            Effect::Ask
        );
    }
    let project = rule("read", "*", "ask")
        + &rule("read", "secret*", "deny")
        + &rule("read", "public", "allow");
    let settings = settings(&trusted, "", &project).unwrap();
    for (path, effect) in [
        ("ordinary", Effect::Ask),
        ("secret.env", Effect::Deny),
        ("public", Effect::Allow),
    ] {
        assert_eq!(
            settings.evaluate(
                &[Check::path("read", path, Effect::Allow)],
                Path::new("/workspace")
            ),
            effect
        );
    }
}

#[test]
fn all_resources_must_allow_before_an_operation_can_skip_the_client() {
    let settings = settings(
        &(rule("edit", "*", "allow")
            + &rule("read", "private", "deny")
            + &rule("edit", "review", "ask")),
        "",
        "",
    )
    .unwrap();
    let cwd = Path::new("/workspace");
    assert_eq!(
        settings.evaluate(
            &[
                Check::path("read", "private", Effect::Allow),
                Check::path("edit", "public", Effect::Ask)
            ],
            cwd
        ),
        Effect::Deny
    );
    assert_eq!(
        settings.evaluate(
            &[
                Check::path("edit", "public", Effect::Ask),
                Check::path("edit", "review", Effect::Ask)
            ],
            cwd
        ),
        Effect::Ask
    );
}

#[test]
fn explicitly_selecting_the_project_file_also_loads_its_rules_as_trusted_choices() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".config")).unwrap();
    let path = root.path().join(".config/bee.toml");
    std::fs::write(&path, rule("shell", "git status *", "allow")).unwrap();
    let environment = Environment::new(None, None, Some(path));
    let policy = Settings::from_config(
        &Config::load(&environment, root.path()).unwrap(),
        &environment,
    )
    .unwrap();
    assert_eq!(
        policy.evaluate(&[Check::command("git status")], root.path()),
        Effect::Allow
    );
    assert_eq!(
        policy.evaluate(&[Check::command("git push")], root.path()),
        Effect::Ask
    );
}

#[test]
fn wildcards_match_whole_unicode_values_and_slashes_without_regex_semantics() {
    let wildcard_policy = settings(&rule("re?d", "docs/*.?s", "deny"), "", "").unwrap();
    for (path, effect) in [
        ("docs/deep/é.rs", Effect::Deny),
        ("docs/a.tsx", Effect::Allow),
        ("other/docs/a.rs", Effect::Allow),
    ] {
        assert_eq!(
            wildcard_policy.evaluate(
                &[Check::path("read", path, Effect::Allow)],
                Path::new("/workspace")
            ),
            effect
        );
    }
    let settings = settings(&rule("grep", "[a-z]+", "deny"), "", "").unwrap();
    assert_eq!(
        settings.evaluate(
            &[Check::value("grep", "[a-z]+", Effect::Allow)],
            Path::new("/workspace")
        ),
        Effect::Deny
    );
    assert_eq!(
        settings.evaluate(
            &[Check::value("grep", "abc", Effect::Allow)],
            Path::new("/workspace")
        ),
        Effect::Allow
    );
}

#[cfg(unix)]
#[test]
fn requested_paths_are_lexical_and_home_patterns_do_not_expand_shell_text() {
    let settings = settings(
        &(rule("read", "/workspace/docs/*", "deny")
            + &rule("read", "~/private/*", "deny")
            + &rule("shell", "$HOME/private/*", "allow")),
        "",
        "",
    )
    .unwrap();
    let cwd = Path::new("/workspace");
    for path in [
        "docs/a",
        "./docs/../docs/a",
        "/workspace/docs/a",
        "/home/test/private/a",
    ] {
        assert_eq!(
            settings.evaluate(&[Check::path("read", path, Effect::Allow)], cwd),
            Effect::Deny
        );
    }
    assert_eq!(
        settings.evaluate(&[Check::command("$HOME/private/run")], cwd),
        Effect::Allow
    );
    assert_eq!(
        settings.evaluate(&[Check::command("/home/test/private/run")], cwd),
        Effect::Ask
    );
    // The requested link spelling is the resource; routing does not inspect its referent.
    let root = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/home/test/private", root.path().join("link")).unwrap();
    assert_eq!(
        settings.evaluate(&[Check::path("read", "link/a", Effect::Allow)], root.path()),
        Effect::Allow
    );
}

#[test]
fn malformed_rules_fail_without_exposing_configuration_values() {
    for text in [
        "permissions = 'CONFIG-SECRET'",
        "permissions = [{}]",
        "permissions = [{action='read', resource='CONFIG-SECRET', effect='bogus'}]",
        "permissions = [{action='read', resource='*', effect='allow', extra='CONFIG-SECRET'}]",
        "permissions = [{action='', resource='CONFIG-SECRET', effect='allow'}]",
    ] {
        let error = settings(text, "", "").unwrap_err();
        assert!(matches!(error, bee::Error::PermissionSetting { .. }));
        assert!(!format!("{error:?} {error}").contains("CONFIG-SECRET"));
    }
    let settings = settings(&rule("read", "CONFIG-SECRET", "deny"), "", "").unwrap();
    assert!(!format!("{settings:?}").contains("CONFIG-SECRET"));
}

#[test]
fn literal_stars_in_resources_cannot_bypass_wildcard_denials() {
    let policy = settings(&rule("*", "*", "deny"), "", "").unwrap();
    for check in [
        Check::path("read", "*secret.txt", Effect::Allow),
        Check::command("*command"),
        Check::value("grep", "*query", Effect::Allow),
        Check::value("glob", "**/*.rs", Effect::Allow),
    ] {
        assert_eq!(
            policy.evaluate(&[check], Path::new("/workspace")),
            Effect::Deny
        );
    }
}

#[cfg(unix)]
#[test]
fn absolute_and_home_scoped_allows_do_not_authorize_external_paths() {
    for (pattern, cwd) in [
        ("/workspace/*", "/workspace"),
        ("~/*", "/home/test"),
        ("$HOME/*", "/home/test"),
    ] {
        let policy = settings(&rule("edit", pattern, "allow"), "", "").unwrap();
        assert_eq!(
            policy.evaluate(
                &[Check::path("edit", "nested/file", Effect::Ask)],
                Path::new(cwd)
            ),
            Effect::Allow
        );
        for path in ["/etc/passwd", "../external/file"] {
            assert_eq!(
                policy.evaluate(&[Check::path("edit", path, Effect::Ask)], Path::new(cwd)),
                Effect::Ask
            );
        }
    }
}
