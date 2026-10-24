//! Standard YAML metadata accepts portable skills and never leaks invalid content.

use bee::skills::validate;
use std::path::Path;

#[rstest::rstest]
#[case("missing")]
#[case("---\nname: demo\n---\n")]
#[case("---\nname: demo\ndescription: 123\n---\n")]
#[case("---\nname: demo\ndescription: secret-sentinel\nname: demo\n---\n")]
#[case("---\nname: demo\ndescription: secret-sentinel\nmetadata: {key: 4}\n---\n")]
#[case("---\nname: demo\ndescription: secret-sentinel\nsecret-sentinel: true\n---\n")]
#[case("---\nname: demo\ndescription: [secret-sentinel\n---\n")]
#[case("---\nname: demo--bad\ndescription: secret-sentinel\n---\n")]
fn malformed_metadata_has_actionable_content_free_diagnostics(#[case] text: &str) {
    let diagnostic = validate(text, "demo", Path::new("demo/SKILL.md")).unwrap_err();
    let rendered = format!(
        "{diagnostic} {diagnostic:?} {:?}",
        miette::Report::new(diagnostic.clone())
    );
    assert!(!rendered.contains("secret-sentinel"));
    assert!(rendered.contains("bee::skills::metadata"));
    assert!(rendered.contains("fix SKILL.md"));
}

#[test]
fn multiline_descriptions_and_standard_optional_fields_are_accepted() {
    let text = "---\nname: demo\ndescription: >-\n  Review code\n  when requested.\nlicense: MIT\ncompatibility: Requires git\nallowed-tools: Bash(git:*) Read\nmetadata:\n  version: '1.0'\n---\nAnything in the body\n";
    let skill = validate(text, "demo", Path::new("demo/SKILL.md")).unwrap();
    assert_eq!(skill.description, "Review code when requested.");
}

#[test]
fn international_names_follow_upstream_normalization_and_directory_matching() {
    let text = "---\nname: café\ndescription: Review documentation\n---\n";
    assert!(validate(text, "café", Path::new("café/SKILL.md")).is_ok());
    assert!(validate(text, "different", Path::new("different/SKILL.md")).is_err());
}

#[rstest::rstest]
#[case("[skills]\nextra_roots = [12]")]
#[case("[skills]\nextra_roots = ['']")]
#[case("[skills]\nsecret-sentinel = true")]
#[case("skills = 'secret-sentinel'")]
fn invalid_discovery_settings_do_not_echo_configuration_values_or_unknown_keys(#[case] text: &str) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("bee.toml");
    std::fs::write(&path, text).unwrap();
    let environment = bee::config::Environment::new(None, None, Some(path));
    let config = bee::config::Config::load(&environment, root.path()).unwrap();
    let error = bee::skills::Settings::from_config(&config, root.path())
        .err()
        .unwrap();
    let direct = format!("{error} {error:?}");
    let rendered = format!("{direct} {:?}", miette::Report::new(error));
    assert!(!rendered.contains("secret-sentinel"));
    assert!(rendered.contains("bee::skills::setting"));
}
