//! Named delegate definitions parsed from Bee's existing merged configuration.

use std::collections::BTreeMap;

use super::{Error, Result};

/// Validated local stdio launch settings. Debug deliberately hides process settings.
#[derive(Clone)]
pub struct Definition {
    pub(super) command: String,
    pub(super) args: Vec<String>,
    pub(super) env: BTreeMap<String, String>,
}

impl std::fmt::Debug for Definition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Definition([redacted])")
    }
}

/// Named agents; constructing or selecting a definition never starts a process.
#[derive(Clone, Default)]
pub struct Agents(BTreeMap<String, Definition>);

impl std::fmt::Debug for Agents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Agents([redacted])")
    }
}

impl Agents {
    /// Parse every delegate using the same layered configuration as other Bee settings.
    ///
    /// # Errors
    /// Returns a value-redacted configuration diagnostic for invalid definitions.
    pub fn from_config(config: &crate::config::Config) -> Result<Self> {
        let failure = |key: &str, reason| Error::Setting {
            path: config.origin(key).map_or_else(
                || "<configuration>".into(),
                |path| path.display().to_string(),
            ),
            reason,
        };
        let Some(value) = config.get("agents") else {
            return Ok(Self::default());
        };
        let table = value
            .as_table()
            .ok_or_else(|| failure("agents", "must be a table"))?;
        let mut agents = BTreeMap::new();
        for (name, value) in table {
            let key = format!("agents.{name}");
            if name.trim().is_empty() {
                return Err(failure("agents", "agent names must be nonempty"));
            }
            let table = value
                .as_table()
                .ok_or_else(|| failure(&key, "each definition must be a table"))?;
            for field in table.keys() {
                if !matches!(field.as_str(), "command" | "args" | "env" | "transport") {
                    return Err(failure(
                        &key,
                        "contains an unknown field; use command, args, env or transport",
                    ));
                }
            }
            let command = table
                .get("command")
                .and_then(toml::Value::as_str)
                .filter(|s| !s.trim().is_empty() && !s.contains('\0'))
                .ok_or_else(|| {
                    failure(
                        &format!("{key}.command"),
                        "command must be a nonempty executable string without NUL",
                    )
                })?;
            if let Some(transport) = table.get("transport")
                && transport.as_str() != Some("stdio")
            {
                return Err(failure(
                    &format!("{key}.transport"),
                    "transport must be stdio",
                ));
            }
            let mut args = Vec::new();
            if let Some(value) = table.get("args") {
                let invalid = || {
                    failure(
                        &format!("{key}.args"),
                        "args must be an array of strings without NUL",
                    )
                };
                for value in value.as_array().ok_or_else(invalid)? {
                    args.push(
                        value
                            .as_str()
                            .filter(|s| !s.contains('\0'))
                            .ok_or_else(invalid)?
                            .to_owned(),
                    );
                }
            }
            let mut env = BTreeMap::new();
            if let Some(value) = table.get("env") {
                let invalid = || {
                    failure(
                        &format!("{key}.env"),
                        "env must map nonempty names without equals or NUL to strings without NUL",
                    )
                };
                for (name, value) in value.as_table().ok_or_else(invalid)? {
                    if name.is_empty() || name.contains(['=', '\0']) {
                        return Err(invalid());
                    }
                    let value = value
                        .as_str()
                        .filter(|s| !s.contains('\0'))
                        .ok_or_else(invalid)?;
                    env.insert(name.clone(), value.to_owned());
                }
            }
            agents.insert(
                name.clone(),
                Definition {
                    command: command.into(),
                    args,
                    env,
                },
            );
        }
        Ok(Self(agents))
    }

    /// Select a definition explicitly; there is no first-agent default.
    ///
    /// # Errors
    /// Returns [`Error::UnknownAgent`] when the requested name is absent.
    pub fn get(&self, name: &str) -> Result<&Definition> {
        self.0.get(name).ok_or(Error::UnknownAgent)
    }

    /// Enumerate available names without exposing executable settings.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}
