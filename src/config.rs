//! Layered configuration, free of the ACPv2 SDK.
//!
//! File layers are merged, lowest precedence first:
//!
//! 1. the global file, `$XDG_CONFIG_HOME/bee/config.toml` (or
//!    `~/.config/bee/config.toml`);
//! 2. the file named by the `BEE_CONFIG` environment variable;
//! 3. the project file, `<project>/.config/bee.toml`.
//!
//! `BEE_PROFILE` supplies a selector between the global and intermediate files
//! using the same merge rules (ADR-022).
//!
//! The result is a merged TOML table, not a typed schema: the settings that
//! give it meaning (providers, models) belong to the code that consumes them,
//! which deserializes from [`Config::table`]. See ADR-006 for the decisions
//! and the merge rules.
//!
//! Nothing here prints, and no diagnostic or `Debug` output carries a
//! configuration *value*: files are where users put API keys.

use std::collections::BTreeMap;
use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::error::{Error, Result};

/// The environment variable that selects the intermediate configuration file.
pub const BEE_CONFIG_VAR: &str = "BEE_CONFIG";

/// The profile selector applied after global configuration and before override files.
pub const BEE_PROFILE_VAR: &str = "BEE_PROFILE";

/// The process environment that decides where configuration lives.
///
/// Taken as values instead of read on demand so a test can describe any
/// environment without mutating the process's own, which is `unsafe` in
/// edition 2024 and races with every other test thread.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Environment {
    xdg_config_home: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    xdg_cache_home: Option<PathBuf>,
    xdg_state_home: Option<PathBuf>,
    home: Option<PathBuf>,
    bee_config: Option<PathBuf>,
    variables: Variables,
}

/// Where credential variables (`OPENROUTER_API_KEY`...) are looked up.
#[derive(Clone, PartialEq, Eq)]
enum Variables {
    /// The process's own environment.
    Process,
    /// Exactly these, so a test never depends on the machine it runs on.
    Fixed(BTreeMap<String, String>),
}

impl Default for Variables {
    fn default() -> Self {
        Self::Fixed(BTreeMap::new())
    }
}

// Written by hand: a variable's *value* is a credential, and `Environment` ends
// up in `{:?}` and in assertion failures. Names are enough to debug with.
impl fmt::Debug for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("Environment");
        debug
            .field("xdg_config_home", &self.xdg_config_home)
            .field("xdg_data_home", &self.xdg_data_home)
            .field("home", &self.home)
            .field("bee_config", &self.bee_config);
        match &self.variables {
            Variables::Process => debug.field("variables", &"<process>"),
            Variables::Fixed(map) => debug.field("variable_names", &map.keys().collect::<Vec<_>>()),
        };
        debug.finish()
    }
}

impl Environment {
    /// Explicit home directory, also available to hermetic discovery callers.
    pub fn home_dir(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// Describe an environment explicitly.
    ///
    /// Values are interpreted as the process environment would be: an empty
    /// value counts as unset, and a relative `xdg_config_home` is ignored.
    #[must_use]
    pub fn new(
        xdg_config_home: Option<PathBuf>,
        home: Option<PathBuf>,
        bee_config: Option<PathBuf>,
    ) -> Self {
        Self {
            xdg_data_home: None,
            xdg_cache_home: None,
            xdg_state_home: None,
            // No variables: a hermetic description holds only what it is given
            // (see `with_variable`).
            variables: Variables::default(),
            // The XDG specification says a relative `XDG_CONFIG_HOME` is
            // invalid and must be ignored: honouring it would make the global
            // file depend on whatever directory the client launched Bee from.
            xdg_config_home: non_empty(xdg_config_home).filter(|path| path.is_absolute()),
            home: non_empty(home),
            // A relative `BEE_CONFIG` is kept as given and resolves against the
            // process's working directory, which is what a user typing it in a
            // shell expects.
            bee_config: non_empty(bee_config),
        }
    }

    /// Read the process environment.
    #[must_use]
    pub fn from_process() -> Self {
        // Native Windows clients commonly set USERPROFILE rather than HOME.
        // Resolve it here so all injected XDG consumers share the same home.
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .or({
                #[cfg(windows)]
                {
                    std::env::var_os("USERPROFILE")
                }
                #[cfg(not(windows))]
                {
                    None
                }
            });
        Self::new(
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            home.map(PathBuf::from),
            std::env::var_os(BEE_CONFIG_VAR).map(PathBuf::from),
        )
        .with_xdg_data_home(std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .with_xdg_cache_home(std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
        .with_xdg_state_home(std::env::var_os("XDG_STATE_HOME").map(PathBuf::from))
        .with_process_variables()
    }

    /// Set `XDG_DATA_HOME`, where the credential store lives.
    ///
    /// Interpreted like `XDG_CONFIG_HOME`: empty counts as unset and a
    /// relative value is ignored.
    #[must_use]
    pub fn with_xdg_data_home(mut self, xdg_data_home: Option<PathBuf>) -> Self {
        self.xdg_data_home = non_empty(xdg_data_home).filter(|path| path.is_absolute());
        self
    }

    /// Set the XDG cache root without consulting process-global state in tests.
    #[must_use]
    pub fn with_xdg_cache_home(mut self, root: Option<PathBuf>) -> Self {
        self.xdg_cache_home = non_empty(root).filter(|path| path.is_absolute());
        self
    }

    /// Set the durable state root without consulting process-global state.
    #[must_use]
    pub fn with_xdg_state_home(mut self, root: Option<PathBuf>) -> Self {
        self.xdg_state_home = non_empty(root).filter(|path| path.is_absolute());
        self
    }

    /// Durable sessions are state, independent of disposable model caches.
    pub fn history_path(&self) -> Option<PathBuf> {
        self.xdg_state_home
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".local/state")))
            .map(|root| root.join("bee"))
    }

    /// Public model catalog cache; account-specific endpoint results never live here.
    pub fn model_cache_file(&self) -> Option<PathBuf> {
        self.xdg_cache_home
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".cache")))
            .map(|root| root.join("bee/models.dev.json"))
    }

    /// Look credential and profile-selector variables up in the process environment.
    #[must_use]
    pub fn with_process_variables(mut self) -> Self {
        self.variables = Variables::Process;
        self
    }

    /// Define an environment variable. Switches to a fixed set of variables,
    /// dropping any lookup in the process environment.
    #[must_use]
    pub fn with_variable(mut self, name: &str, value: &str) -> Self {
        let mut map = match self.variables {
            Variables::Fixed(map) => map,
            Variables::Process => BTreeMap::new(),
        };
        map.insert(name.to_owned(), value.to_owned());
        self.variables = Variables::Fixed(map);
        self
    }

    /// The value of the variable `name`, if set and not empty.
    ///
    /// An empty variable counts as unset: `export KEY=` is what a shell
    /// leaves behind when a secret manager failed, and sending an empty bearer
    /// token would be a confusing 401 instead of a clear "not set".
    #[must_use]
    pub fn variable(&self, name: &str) -> Option<String> {
        let value = match &self.variables {
            Variables::Process => std::env::var(name).ok(),
            Variables::Fixed(map) => map.get(name).cloned(),
        };
        value.filter(|value| !value.is_empty())
    }

    /// The credential store file, if a base directory can be determined.
    ///
    /// Under the XDG *data* directory, not the configuration one: it is state
    /// Bee writes, and keeping it out of `~/.config` keeps it out of the
    /// dotfile repositories that directory is often committed to. `None`
    /// means neither `XDG_DATA_HOME` nor `HOME` is usable.
    #[must_use]
    pub fn credentials_path(&self) -> Option<PathBuf> {
        let base = match (&self.xdg_data_home, &self.home) {
            (Some(xdg), _) => xdg.clone(),
            (None, Some(home)) => home.join(".local").join("share"),
            (None, None) => return None,
        };
        Some(base.join("bee").join("credentials.json"))
    }

    /// The global configuration file, if a base directory can be determined.
    ///
    /// The same rule on every platform, macOS included: the XDG base directory
    /// specification, not the platform's own convention, so one dotfile
    /// repository works everywhere. `None` means neither `XDG_CONFIG_HOME` nor
    /// `HOME` is usable (a bare container); that is not an error, there is just
    /// no global layer.
    #[must_use]
    pub fn global_config_path(&self) -> Option<PathBuf> {
        let base = match (&self.xdg_config_home, &self.home) {
            (Some(xdg), _) => xdg.clone(),
            (None, Some(home)) => home.join(".config"),
            (None, None) => return None,
        };
        Some(base.join("bee").join("config.toml"))
    }

    /// The file `BEE_CONFIG` selects, if any.
    #[must_use]
    pub fn selected_config_path(&self) -> Option<&Path> {
        self.bee_config.as_deref()
    }
}

/// The project configuration file for `project_dir`.
///
/// Exactly this directory, never a parent: a search upwards would make the
/// effective configuration depend on directories outside the project.
#[must_use]
pub fn project_config_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".config").join("bee.toml")
}

/// The effective configuration: every layer merged.
#[derive(Clone, Default, PartialEq)]
pub struct Config {
    table: Table,
    // Dotted key -> the file that last set it. Needed so a conflict can name
    // the lower file, and so a user can ask where a value came from.
    origins: BTreeMap<String, PathBuf>,
    sources: Vec<PathBuf>,
    // The project file's path, whether or not it exists, so a setting can be
    // refused when that file is where it came from.
    project: Option<PathBuf>,
    // Generic merging replaces arrays; permission provenance must survive that replacement.
    permission_layers: Vec<(PathBuf, bool, Value)>,
}

// Written by hand: a derived `Debug` would print `table`, and the table holds
// whatever the user keeps in their configuration, API keys included. `Config`
// ends up in `{:?}` and in assertion failures, so it must be safe there.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field(
                "keys",
                &self
                    .origins
                    .keys()
                    .map(|key| diagnostic_key(key))
                    .collect::<Vec<_>>(),
            )
            .field("sources", &self.sources)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// Load and merge the file layers, inserting `BEE_PROFILE` after global configuration.
    ///
    /// The global and project files are optional: a missing one is skipped.
    /// The `BEE_CONFIG` file was asked for by name, so a missing one is an
    /// error, as is any file that exists but cannot be read or parsed.
    ///
    /// # Errors
    ///
    /// - [`Error::ConfigSelectedMissing`] if `BEE_CONFIG` names a missing file.
    /// - [`Error::ConfigRead`] if a file exists but cannot be read.
    /// - [`Error::ConfigParse`] if a file is not valid TOML.
    /// - [`Error::ConfigConflict`] if two layers give one key different types.
    pub fn load(environment: &Environment, project_dir: &Path) -> Result<Self> {
        Self::load_with_global(environment, project_dir, None)
    }

    fn load_with_global(
        environment: &Environment,
        project_dir: &Path,
        replacement: Option<&Table>,
    ) -> Result<Self> {
        // (path, required, project), lowest precedence first. Order is the whole
        // precedence rule, so it lives in one visible place.
        let mut layers: Vec<(PathBuf, bool, bool)> = Vec::new();
        if let Some(global) = environment.global_config_path() {
            layers.push((global, false, false));
        }
        if let Some(selected) = environment.selected_config_path() {
            layers.push((selected.to_path_buf(), true, false));
        }
        layers.push((project_config_path(project_dir), false, true));

        let mut config = Self {
            project: Some(project_config_path(project_dir)),
            ..Self::default()
        };
        if environment.global_config_path().is_none() {
            config.merge_profile_environment(environment)?;
        }
        for (index, (path, required, project)) in layers.into_iter().enumerate() {
            let table = if index == 0
                && environment.global_config_path().is_some()
                && replacement.is_some()
            {
                replacement.cloned()
            } else {
                read_layer(&path, required)?
            };
            if let Some(table) = table {
                if let Some(value) = table.get("permissions") {
                    // Trust follows the selected layer, even when BEE_CONFIG names the project file.
                    config
                        .permission_layers
                        .push((path.clone(), project, value.clone()));
                }
                config.sources.push(path.clone());
                merge(&mut config.table, table, "", &path, &mut config.origins)?;
            }
            if index == 0 && environment.global_config_path().is_some() {
                config.merge_profile_environment(environment)?;
            }
        }
        Ok(config)
    }

    fn merge_profile_environment(&mut self, environment: &Environment) -> Result<()> {
        // Use the ordinary merge rules so later files can override the selector
        // without introducing a second precedence or type-conflict mechanism.
        if let Some(profile) = environment.variable(BEE_PROFILE_VAR) {
            let mut table = Table::new();
            table.insert("profile".into(), Value::String(profile));
            merge(
                &mut self.table,
                table,
                "",
                Path::new("<environment BEE_PROFILE>"),
                &mut self.origins,
            )?;
        }
        Ok(())
    }

    /// The merged table, for the consumer to deserialize from.
    #[must_use]
    pub fn table(&self) -> &Table {
        &self.table
    }

    pub(crate) fn permission_layers(&self) -> impl Iterator<Item = (&Path, bool, &Value)> {
        self.permission_layers
            .iter()
            .map(|(path, project, value)| (path.as_path(), *project, value))
    }

    /// The value at a dotted key such as `provider.name`.
    ///
    /// A key that itself contains a dot cannot be addressed this way; read it
    /// from [`Config::table`].
    #[must_use]
    pub fn get(&self, dotted_key: &str) -> Option<&Value> {
        let mut parts = dotted_key.split('.');
        let mut value = self.table.get(parts.next()?)?;
        for part in parts {
            value = value.as_table()?.get(part)?;
        }
        Some(value)
    }

    /// The file that set the value at a dotted key, after merging.
    #[must_use]
    pub fn origin(&self, dotted_key: &str) -> Option<&Path> {
        self.origins.get(dotted_key).map(PathBuf::as_path)
    }

    /// A key at or under `dotted_prefix` that the project file set, if any.
    ///
    /// For settings the project file may not make: it can come from a
    /// repository the user only cloned. The key is returned, never a value.
    #[must_use]
    pub fn set_by_project(&self, dotted_prefix: &str) -> Option<String> {
        let project = self.project.as_deref()?;
        let nested = format!("{dotted_prefix}.");
        self.origins
            .iter()
            .find(|(key, path)| {
                path.as_path() == project && (*key == dotted_prefix || key.starts_with(&nested))
            })
            .map(|(key, _)| key.clone())
    }

    /// The files that were actually read, lowest precedence first.
    #[must_use]
    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }
}

pub mod persist;

/// Treat an empty path as absent, as shells do for an empty variable.
fn non_empty(path: Option<PathBuf>) -> Option<PathBuf> {
    path.filter(|path| !path.as_os_str().is_empty())
}

/// Read and parse one layer. `None` means an optional file is absent.
fn read_layer(path: &Path, required: bool) -> Result<Option<Table>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return if required {
                Err(Error::ConfigSelectedMissing {
                    path: path.display().to_string(),
                })
            } else {
                Ok(None)
            };
        }
        // Any other failure (permissions, a directory, invalid UTF-8) is a file
        // the user put there that Bee cannot honour. Skipping it would silently
        // run with settings they believe are in force.
        Err(source) => {
            return Err(Error::ConfigRead {
                path: path.display().to_string(),
                source,
            });
        }
    };

    text.parse::<Table>().map(Some).map_err(|error| {
        // Only the message and a position are kept, never `error`'s `Display`,
        // which quotes the offending source line.
        let (line, column) = error
            .span()
            .map_or((1, 1), |span| line_and_column(&text, span.start));
        Error::ConfigParse {
            path: path.display().to_string(),
            line,
            column,
            reason: error.message().to_owned(),
        }
    })
}

/// 1-based line and column of a byte offset.
fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    // A span is a byte offset and may land mid-way through a multi-byte
    // character only if the parser is wrong; clamp so a slice cannot panic.
    let mut end = offset.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let before = &text[..end];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |s| s.chars().count()) + 1;
    (line, column)
}

/// Merge `higher` over `base`.
///
/// Tables merge key by key, anything else is replaced by the higher layer, and
/// a key holding different types in the two layers is a conflict. Arrays are
/// replaced whole, never concatenated, so a project can drop an entry the
/// global file lists.
fn merge(
    base: &mut Table,
    higher: Table,
    prefix: &str,
    higher_path: &Path,
    origins: &mut BTreeMap<String, PathBuf>,
) -> Result<()> {
    for (key, value) in higher {
        let dotted = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };

        match base.get_mut(&key) {
            None => {
                record_origins(&dotted, &value, higher_path, origins);
                base.insert(key, value);
            }
            Some(existing) if existing.type_str() != value.type_str() => {
                return Err(Error::ConfigConflict {
                    // The origin is always present for a key already merged.
                    lower_path: origins
                        .get(&dotted)
                        .map_or_else(String::new, |p| p.display().to_string()),
                    lower_type: existing.type_str(),
                    higher_path: higher_path.display().to_string(),
                    higher_type: value.type_str(),
                    key: diagnostic_key(&dotted).to_owned(),
                });
            }
            Some(Value::Table(existing)) => {
                let Value::Table(value) = value else {
                    unreachable!("the types were just checked to be equal");
                };
                // A table is "set by" the last file that contributed to it.
                origins.insert(dotted.clone(), higher_path.to_path_buf());
                merge(existing, value, &dotted, higher_path, origins)?;
            }
            Some(existing) => {
                // Same non-table type: the higher layer wins. Arrays land here
                // too, which is what makes them replace instead of append.
                *existing = value;
                origins.insert(dotted, higher_path.to_path_buf());
            }
        }
    }
    Ok(())
}

// Model IDs are configuration values expressed as keys; exposing their origin
// paths would bypass the normal protection against printing configuration values.
fn diagnostic_key(key: &str) -> &str {
    if key.starts_with("providers.") || key.starts_with("presets.") {
        for field in [
            ".model_protocols",
            ".model_metadata",
            ".model_context_windows",
        ] {
            if let Some((parent, _)) = key.split_once(&format!("{field}.")) {
                return &key[..parent.len() + field.len()];
            }
        }
    }
    key
}

/// Record `path` as the origin of `value` and of every key nested in it.
fn record_origins(
    dotted: &str,
    value: &Value,
    path: &Path,
    origins: &mut BTreeMap<String, PathBuf>,
) {
    origins.insert(dotted.to_owned(), path.to_path_buf());
    if let Value::Table(table) = value {
        for (key, nested) in table {
            record_origins(&format!("{dotted}.{key}"), nested, path, origins);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn table(text: &str) -> Table {
        text.parse().expect("the test fixture is valid TOML")
    }

    #[test]
    fn a_line_and_column_are_one_based() {
        assert_eq!(line_and_column("ab\ncd", 0), (1, 1));
        assert_eq!(line_and_column("ab\ncd", 4), (2, 2));
    }

    #[test]
    fn an_offset_past_the_end_does_not_panic() {
        assert_eq!(line_and_column("ab", 99), (1, 3));
    }

    #[test]
    fn merging_records_the_origin_of_nested_keys() {
        let mut base = Table::new();
        let mut origins = BTreeMap::new();
        merge(
            &mut base,
            table("[a]\nb = 1"),
            "",
            Path::new("low.toml"),
            &mut origins,
        )
        .unwrap();
        merge(
            &mut base,
            table("[a]\nc = 2"),
            "",
            Path::new("high.toml"),
            &mut origins,
        )
        .unwrap();

        assert_eq!(origins["a.b"], Path::new("low.toml"));
        assert_eq!(origins["a.c"], Path::new("high.toml"));
    }
}
