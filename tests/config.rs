//! The layered configuration, in-process.
//!
//! Every test describes its own `Environment` and project directory, so none
//! touches the process environment or a developer's real configuration.

use std::fs;
use std::path::{Path, PathBuf};

use bee::Error;
use bee::config::{Config, Environment, project_config_path};
use pretty_assertions::assert_eq;
use rstest::rstest;
use tempfile::TempDir;

/// A secret that must never appear in any diagnostic or debug output.
const SECRET: &str = "sk-super-secret-token";

/// Write `text` to `path`, creating its parent directories.
fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("test paths have parents")).unwrap();
    fs::write(path, text).unwrap();
}

/// A sandbox with a home, a project and a spare directory for `BEE_CONFIG`.
struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            root: TempDir::new().unwrap(),
        }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn global(&self) -> PathBuf {
        self.home().join(".config/bee/config.toml")
    }

    fn selected(&self) -> PathBuf {
        self.root.path().join("selected.toml")
    }

    fn project_dir(&self) -> PathBuf {
        self.root.path().join("project")
    }

    fn project(&self) -> PathBuf {
        project_config_path(&self.project_dir())
    }

    /// An environment with `HOME` set and nothing else.
    fn env(&self) -> Environment {
        Environment::new(None, Some(self.home()), None)
    }

    fn env_with_selected(&self) -> Environment {
        Environment::new(None, Some(self.home()), Some(self.selected()))
    }

    fn load(&self, env: &Environment) -> bee::Result<Config> {
        fs::create_dir_all(self.project_dir()).unwrap();
        Config::load(env, &self.project_dir())
    }
}

/// Render an error the way the binary does, to check what a user would see.
fn rendered(error: Error) -> String {
    format!("{:?}", miette::Report::new(error))
}

#[test]
fn xdg_config_home_selects_the_global_directory() {
    let env = Environment::new(Some("/custom/xdg".into()), Some("/home/user".into()), None);

    assert_eq!(
        env.global_config_path(),
        Some(PathBuf::from("/custom/xdg/bee/config.toml"))
    );
}

#[test]
fn the_global_directory_defaults_to_dot_config_under_home() {
    let env = Environment::new(None, Some("/home/user".into()), None);

    assert_eq!(
        env.global_config_path(),
        Some(PathBuf::from("/home/user/.config/bee/config.toml"))
    );
}

#[rstest]
#[case::empty("")]
#[case::relative("relative/xdg")]
fn an_invalid_xdg_config_home_is_ignored(#[case] xdg: &str) {
    // The XDG specification says to ignore these, not to fail.
    let env = Environment::new(Some(xdg.into()), Some("/home/user".into()), None);

    assert_eq!(
        env.global_config_path(),
        Some(PathBuf::from("/home/user/.config/bee/config.toml"))
    );
}

#[test]
fn credentials_live_under_the_xdg_data_directory() {
    let env = Environment::new(None, Some("/home/user".into()), None)
        .with_xdg_data_home(Some("/custom/data".into()));

    assert_eq!(
        env.credentials_path(),
        Some(PathBuf::from("/custom/data/bee/credentials.json"))
    );
}

#[rstest]
#[case::unset(None)]
#[case::empty(Some(""))]
#[case::relative(Some("relative/data"))]
fn without_a_valid_xdg_data_home_credentials_live_under_the_home(#[case] xdg: Option<&str>) {
    let env = Environment::new(None, Some("/home/user".into()), None)
        .with_xdg_data_home(xdg.map(Into::into));

    assert_eq!(
        env.credentials_path(),
        Some(PathBuf::from(
            "/home/user/.local/share/bee/credentials.json"
        ))
    );
}

#[test]
fn without_home_or_xdg_there_is_nowhere_to_keep_credentials() {
    assert_eq!(Environment::new(None, None, None).credentials_path(), None);
}

#[test]
fn a_described_environment_knows_only_the_variables_it_was_given() {
    let env = Environment::new(None, None, None)
        .with_variable("PRESENT", "value")
        .with_variable("EMPTY", "");

    assert_eq!(env.variable("PRESENT").as_deref(), Some("value"));
    assert_eq!(env.variable("EMPTY"), None, "an empty variable is unset");
    // `PATH` is set in any real process; a described environment must not see it.
    assert_eq!(env.variable("PATH"), None);
}

#[test]
fn the_debug_output_of_an_environment_lists_variable_names_only() {
    let env = Environment::new(None, None, None).with_variable("MY_KEY", SECRET);

    let debug = format!("{env:?}");

    assert!(debug.contains("MY_KEY"), "{debug}");
    assert!(!debug.contains(SECRET), "{debug}");
}

#[test]
fn without_home_or_xdg_the_global_layer_is_skipped() {
    let env = Environment::new(None, None, None);

    assert_eq!(env.global_config_path(), None);

    let sandbox = Sandbox::new();
    assert!(sandbox.load(&env).unwrap().sources().is_empty());
}

#[test]
fn with_no_config_files_at_all_the_configuration_is_empty() {
    let sandbox = Sandbox::new();

    let config = sandbox.load(&sandbox.env()).unwrap();

    assert!(config.table().is_empty());
    assert!(config.sources().is_empty());
}

#[test]
fn an_empty_bee_config_counts_as_unset() {
    let env = Environment::new(None, Some("/home/user".into()), Some(PathBuf::new()));

    assert_eq!(env.selected_config_path(), None);
}

#[test]
fn bee_config_selects_an_intermediate_file() {
    let sandbox = Sandbox::new();
    write(&sandbox.selected(), "model = \"from-selected\"");

    let config = sandbox.load(&sandbox.env_with_selected()).unwrap();

    assert_eq!(config.get("model").unwrap().as_str(), Some("from-selected"));
    assert_eq!(config.origin("model"), Some(sandbox.selected().as_path()));
}

#[test]
fn the_project_file_overrides_bee_config_which_overrides_global() {
    let sandbox = Sandbox::new();
    write(
        &sandbox.global(),
        "a = \"global\"\nb = \"global\"\nc = \"global\"",
    );
    write(&sandbox.selected(), "b = \"selected\"\nc = \"selected\"");
    write(&sandbox.project(), "c = \"project\"");

    let config = sandbox.load(&sandbox.env_with_selected()).unwrap();

    let value = |key: &str| config.get(key).unwrap().as_str().unwrap().to_owned();
    assert_eq!(value("a"), "global");
    assert_eq!(value("b"), "selected");
    assert_eq!(value("c"), "project");
    assert_eq!(config.origin("a"), Some(sandbox.global().as_path()));
    assert_eq!(config.origin("b"), Some(sandbox.selected().as_path()));
    assert_eq!(config.origin("c"), Some(sandbox.project().as_path()));
    assert_eq!(
        config.sources(),
        [sandbox.global(), sandbox.selected(), sandbox.project()]
    );
}

#[test]
fn non_overlapping_keys_from_every_layer_are_all_present() {
    let sandbox = Sandbox::new();
    write(&sandbox.global(), "from_global = 1");
    write(&sandbox.selected(), "from_selected = 2");
    write(&sandbox.project(), "from_project = 3");

    let config = sandbox.load(&sandbox.env_with_selected()).unwrap();

    assert_eq!(config.get("from_global").unwrap().as_integer(), Some(1));
    assert_eq!(config.get("from_selected").unwrap().as_integer(), Some(2));
    assert_eq!(config.get("from_project").unwrap().as_integer(), Some(3));
}

#[test]
fn nested_tables_merge_key_by_key() {
    let sandbox = Sandbox::new();
    write(
        &sandbox.global(),
        "[provider]\nname = \"global\"\nretries = 3\n[provider.limits]\ntokens = 100",
    );
    write(
        &sandbox.project(),
        "[provider]\nname = \"project\"\n[provider.limits]\nrequests = 5",
    );

    let config = sandbox.load(&sandbox.env()).unwrap();

    assert_eq!(
        config.get("provider.name").unwrap().as_str(),
        Some("project")
    );
    assert_eq!(
        config.get("provider.retries").unwrap().as_integer(),
        Some(3)
    );
    assert_eq!(
        config.get("provider.limits.tokens").unwrap().as_integer(),
        Some(100)
    );
    assert_eq!(
        config.get("provider.limits.requests").unwrap().as_integer(),
        Some(5)
    );
    assert_eq!(
        config.origin("provider.retries"),
        Some(sandbox.global().as_path())
    );
    assert_eq!(
        config.origin("provider.limits.requests"),
        Some(sandbox.project().as_path())
    );
}

#[test]
fn a_list_in_a_higher_layer_replaces_the_lower_one() {
    let sandbox = Sandbox::new();
    write(&sandbox.global(), "tags = [\"a\", \"b\", \"c\"]");
    write(&sandbox.project(), "tags = [\"z\"]");

    let config = sandbox.load(&sandbox.env()).unwrap();

    let tags: Vec<_> = config
        .get("tags")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|tag| tag.as_str().unwrap())
        .collect();
    assert_eq!(tags, ["z"]);
}

#[rstest]
#[case::table_over_scalar("x = 1", "[x]\ny = 1", "integer", "table")]
#[case::scalar_over_table("[x]\ny = 1", "x = 1", "table", "integer")]
#[case::string_over_integer("x = 1", "x = \"one\"", "integer", "string")]
#[case::float_over_integer("x = 1", "x = 1.5", "integer", "float")]
#[case::table_over_array("x = [1]", "[x]\ny = 1", "array", "table")]
fn a_type_change_between_layers_is_a_conflict_naming_the_key_and_both_files(
    #[case] lower: &str,
    #[case] higher: &str,
    #[case] lower_type: &str,
    #[case] higher_type: &str,
) {
    let sandbox = Sandbox::new();
    write(&sandbox.global(), lower);
    write(&sandbox.project(), higher);

    let error = sandbox.load(&sandbox.env()).unwrap_err();

    let Error::ConfigConflict {
        key,
        lower_path,
        lower_type: actual_lower,
        higher_path,
        higher_type: actual_higher,
    } = &error
    else {
        panic!("expected a conflict, got {error:?}");
    };
    assert_eq!(key, "x");
    assert_eq!(Path::new(lower_path), sandbox.global());
    assert_eq!(Path::new(higher_path), sandbox.project());
    assert_eq!(*actual_lower, lower_type);
    assert_eq!(*actual_higher, higher_type);
}

#[test]
fn a_conflict_deep_in_a_table_names_the_dotted_key() {
    let sandbox = Sandbox::new();
    write(&sandbox.global(), "[a.b]\nc = 1");
    write(&sandbox.project(), "[a.b]\nc = \"one\"");

    let error = sandbox.load(&sandbox.env()).unwrap_err();

    assert!(
        matches!(&error, Error::ConfigConflict { key, .. } if key == "a.b.c"),
        "{error:?}"
    );
}

#[test]
fn a_conflict_never_echoes_the_values() {
    let sandbox = Sandbox::new();
    write(&sandbox.global(), "key = 1");
    write(&sandbox.project(), &format!("key = \"{SECRET}\""));

    let error = sandbox.load(&sandbox.env()).unwrap_err();

    assert!(!rendered(error).contains(SECRET));
}

#[test]
fn a_missing_bee_config_file_is_an_actionable_error() {
    let sandbox = Sandbox::new();

    let error = sandbox.load(&sandbox.env_with_selected()).unwrap_err();

    let text = rendered(error);
    assert!(text.contains("bee::config::selected_missing"), "{text}");
    assert!(text.contains("selected.toml"), "{text}");
    assert!(text.contains("unset `BEE_CONFIG`"), "{text}");
}

#[test]
fn a_missing_global_or_project_file_is_skipped() {
    let sandbox = Sandbox::new();
    write(&sandbox.selected(), "x = 1");

    let config = sandbox.load(&sandbox.env_with_selected()).unwrap();

    assert_eq!(config.sources(), [sandbox.selected()]);
}

#[rstest]
#[case::global(0)]
#[case::selected(1)]
#[case::project(2)]
fn a_malformed_file_reports_its_path_line_and_column(#[case] layer: usize) {
    let sandbox = Sandbox::new();
    let paths = [sandbox.global(), sandbox.selected(), sandbox.project()];
    // `BEE_CONFIG` is selected below, so its file must exist for the layer
    // under test to be the one that fails.
    write(&sandbox.selected(), "");
    write(&paths[layer], "fine = 1\nbroken = = 2\n");

    let error = sandbox.load(&sandbox.env_with_selected()).unwrap_err();

    let Error::ConfigParse {
        path, line, column, ..
    } = &error
    else {
        panic!("expected a parse error, got {error:?}");
    };
    assert_eq!(Path::new(path), paths[layer]);
    assert_eq!(*line, 2);
    assert!(
        *column > 1,
        "the column points into the line, not its start"
    );
    assert!(rendered(error).contains("bee::config::parse"));
}

#[test]
fn a_parse_error_never_echoes_the_offending_text() {
    let sandbox = Sandbox::new();
    // An unquoted value is invalid TOML, and the parser would quote the line.
    write(&sandbox.project(), &format!("api_key = {SECRET}\n"));

    let error = sandbox.load(&sandbox.env()).unwrap_err();

    let display = error.to_string();
    let text = rendered(error);
    assert!(!display.contains(SECRET), "{display}");
    assert!(!text.contains(SECRET), "{text}");
}

#[test]
fn the_debug_output_of_a_configuration_hides_values() {
    let sandbox = Sandbox::new();
    write(&sandbox.project(), &format!("api_key = \"{SECRET}\""));

    let config = sandbox.load(&sandbox.env()).unwrap();

    let debug = format!("{config:?}");
    assert!(debug.contains("api_key"), "keys stay visible: {debug}");
    assert!(!debug.contains(SECRET), "{debug}");
}

#[test]
fn an_unreadable_project_file_is_an_error() {
    let sandbox = Sandbox::new();
    // A directory where the file should be: unreadable on every platform and
    // without needing chmod, which a root-run CI job would ignore.
    fs::create_dir_all(sandbox.project()).unwrap();

    let error = sandbox.load(&sandbox.env()).unwrap_err();

    assert!(matches!(error, Error::ConfigRead { .. }), "{error:?}");
    assert!(rendered(error).contains("bee::config::read"));
}

#[test]
fn a_file_that_is_not_utf8_is_an_error() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.project().parent().unwrap()).unwrap();
    fs::write(sandbox.project(), [0xff, 0xfe, 0x00]).unwrap();

    let error = sandbox.load(&sandbox.env()).unwrap_err();

    assert!(matches!(error, Error::ConfigRead { .. }), "{error:?}");
}

#[test]
fn the_project_directory_is_not_searched_upwards() {
    let sandbox = Sandbox::new();
    // The file sits in a parent of the project directory, where an upward
    // search would find it.
    write(&sandbox.root.path().join(".config/bee.toml"), "x = 1");

    let config = sandbox.load(&sandbox.env()).unwrap();

    assert!(config.get("x").is_none());
}
