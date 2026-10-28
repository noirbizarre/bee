//! Narrow, comment-preserving user configuration transactions for explicit login.

use std::io::Write;
use std::path::Path;

use fs2::FileExt;
use toml_edit::{DocumentMut, Item, Table, TableLike, value};

use super::{Config, Environment};
use crate::provider::Providers;
use crate::{Error, Result};

/// A preflight result retains no credential and exposes no configuration values.
pub struct LoginConnection {
    name: String,
    providers: Providers,
    import_env: Option<String>,
    import_fields: std::collections::BTreeMap<String, String>,
    original: Option<crate::provider::Instance>,
}

impl LoginConnection {
    /// Reject authority edits while authentication was pending, preserving unrelated edits.
    pub fn check(&self, env: &Environment, cwd: &Path) -> Result<()> {
        let config = Config::load(env, cwd)?;
        if Providers::connections(&config)?.get(&self.name) != self.original.as_ref() {
            return Err(failure(
                &env.global_config_path()
                    .unwrap_or_else(|| "<user configuration>".into()),
                "provider authority changed during login; review the instance and repeat login",
            ));
        }
        Ok(())
    }
    /// Declared environment source before the proposed switch to stored authority.
    pub fn import_env(&self) -> Option<&str> {
        self.import_env.as_deref()
    }
    /// Acquire all declared secret fields as one generation before publication.
    pub fn import_credential(&self, env: &Environment) -> Result<Option<crate::auth::Credential>> {
        let Some(variable) = &self.import_env else {
            return Ok(None);
        };
        let read = |variable: &str| {
            env.variable(variable)
                .map(crate::auth::Secret::new)
                .ok_or_else(|| Error::AuthMissingEnv {
                    profile: self.name.clone(),
                    variable: variable.into(),
                })
        };
        let secret = read(variable)?;
        let fields = self
            .import_fields
            .iter()
            .map(|(field, variable)| Ok((field.clone(), read(variable)?)))
            .collect::<Result<_>>()?;
        let instance = self.providers.get(&self.name).expect("planned instance");
        let credential = crate::auth::Credential::api_key_fields(secret, fields)
            .for_provider(instance.provider_type());
        credential.validate_for(instance)?;
        Ok(Some(credential))
    }
    /// Effective connections after the proposed user-only change.
    pub fn providers(&self) -> &Providers {
        &self.providers
    }

    /// Re-plan under a stable lock because browser login can outlive another editor.
    pub fn commit(&self, env: &Environment, cwd: &Path) -> Result<()> {
        // Reauthentication may only change credentials: managed/read-only configuration needs no writer lock.
        self.check(env, cwd)?;
        let (document, effective) = proposed(env, cwd, &self.name)?;
        if document.is_none() {
            if effective.get(&self.name) != self.providers.get(&self.name) {
                return Err(failure(
                    Path::new("<effective configuration>"),
                    "provider definition changed during login; review the instance and repeat login",
                ));
            }
            return Ok(());
        }
        let path = env.global_config_path().ok_or_else(|| {
            failure(
                Path::new("<user configuration>"),
                "configure HOME or XDG_CONFIG_HOME",
            )
        })?;
        let parent = path.parent().expect("configuration parent");
        std::fs::create_dir_all(parent)
            .map_err(|_| failure(&path, "cannot create the user configuration directory"))?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("toml.lock"))
            .map_err(|_| failure(&path, "cannot open the configuration lock"))?;
        lock.lock_exclusive()
            .map_err(|_| failure(&path, "cannot lock user configuration"))?;
        self.check(env, cwd)?;
        let (document, effective) = proposed(env, cwd, &self.name)?;
        // Browser/device authorization may outlive an edit to the target or source.
        if effective.get(&self.name) != self.providers.get(&self.name) {
            return Err(failure(
                &path,
                "provider definition changed during login; credentials may have been stored; review the instance and repeat login",
            ));
        }
        let Some(document) = document else {
            return Ok(());
        };
        let mut file = tempfile::NamedTempFile::new_in(parent)
            .map_err(|_| failure(&path, "cannot stage user configuration"))?;
        // New files are private; replacement must not broaden an existing file's mode.
        if let Ok(metadata) = std::fs::metadata(&path) {
            file.as_file()
                .set_permissions(metadata.permissions())
                .map_err(|_| failure(&path, "cannot preserve configuration permissions"))?;
        }
        file.write_all(document.to_string().as_bytes())
            .map_err(|_| failure(&path, "cannot write user configuration"))?;
        file.as_file()
            .sync_all()
            .map_err(|_| failure(&path, "cannot sync user configuration"))?;
        file.persist(&path)
            .map_err(|_| failure(&path, "cannot publish user configuration"))?;
        #[cfg(unix)]
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| {
                failure(
                    &path,
                    "configuration was published but its directory could not be synced",
                )
            })?;
        Ok(())
    }
}

fn failure(path: &Path, reason: &'static str) -> Error {
    Error::ConfigWrite {
        path: path.display().to_string(),
        reason,
    }
}

/// Only explicit built-in login creates declarations; status and startup do not.
pub fn prepare(env: &Environment, cwd: &Path, name: &str) -> Result<LoginConnection> {
    let config = Config::load(env, cwd)?;
    let connections = Providers::connections(&config)?;
    let import_env = match connections
        .get(name)
        .and_then(|instance| instance.credential())
    {
        Some(crate::provider::CredentialSource::Env(variable)) => Some(variable.clone()),
        None if !connections.contains_key(name) => crate::provider::registry::definition(name)
            .and_then(|definition| {
                definition
                    .defaults()
                    .get("credential")?
                    .get("env")?
                    .as_str()
                    .map(str::to_owned)
            }),
        _ => None,
    };
    let (_, providers) = proposed(env, cwd, name)?;
    let import_fields = connections
        .get(name)
        .map(|instance| instance.credential_env_fields().clone())
        .unwrap_or_default();
    Ok(LoginConnection {
        name: name.into(),
        providers,
        import_env,
        import_fields,
        original: connections.get(name).cloned(),
    })
}

fn table<'a>(parent: &'a mut dyn TableLike, key: &str) -> &'a mut dyn TableLike {
    parent
        .entry(key)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .expect("validated table")
}

fn proposed(env: &Environment, cwd: &Path, name: &str) -> Result<(Option<DocumentMut>, Providers)> {
    let config = Config::load(env, cwd)?;
    let providers = Providers::from_config(&config)?;
    if providers.get(name).is_some_and(|instance| {
        !matches!(
            instance.credential(),
            Some(crate::provider::CredentialSource::Env(_))
        )
    }) {
        return Ok((None, providers));
    }
    let configured = config
        .table()
        .get("providers")
        .and_then(toml::Value::as_table)
        .is_some_and(|table| table.contains_key(name));
    if !configured
        && !crate::provider::registry::definition(name)
            .is_some_and(|definition| definition.id != "custom")
    {
        crate::auth::profile(&providers, name)?;
    }
    let path = env.global_config_path().ok_or_else(|| {
        failure(
            Path::new("<user configuration>"),
            "configure HOME or XDG_CONFIG_HOME",
        )
    })?;
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(failure(&path, "requires a regular user configuration file"));
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return Err(failure(&path, "cannot read user configuration")),
    };
    let mut document: DocumentMut = text.parse().map_err(|_| {
        failure(
            &path,
            "invalid TOML; repair user configuration before login",
        )
    })?;
    if !configured {
        table(table(document.as_table_mut(), "providers"), name).insert("type", value(name));
    }
    let connections = Providers::connections(&config)?;
    let switch_source = connections.get(name).is_some_and(|instance| {
        matches!(
            instance.credential(),
            Some(crate::provider::CredentialSource::Env(_))
        )
    }) || (!configured
        && crate::provider::registry::definition(name)
            .is_some_and(|definition| !definition.methods.is_empty()));
    if switch_source {
        let connection = table(table(document.as_table_mut(), "providers"), name);
        let credential = table(connection, "credential");
        credential.remove("env");
        credential.remove("fields");
        credential.insert("store", value(true));
    }
    let explicit = config
        .get("profiles")
        .and_then(toml::Value::as_table)
        .is_some_and(|groups| !groups.is_empty());
    if explicit && providers.get(name).is_none() {
        let group_name = providers.profile_name();
        let key = format!("profiles.{group_name}.connections");
        if config.origin(&key).is_some_and(|origin| origin != path) {
            return Err(failure(
                config.origin(&key).expect("known origin"),
                "active-profile membership is overridden here; add the connection to that membership array",
            ));
        }
        let group = table(table(document.as_table_mut(), "profiles"), group_name);
        // Copy membership only from the global layer; higher-layer arrays were rejected.
        let mut members = toml_edit::Array::new();
        for member in providers.names() {
            members.push(member);
        }
        members.push(name);
        group.insert("connections", value(members));
        if group.get("provider").is_none()
            && let Some(previous) = providers.default_name()
        {
            group.insert("provider", value(previous));
        }
    } else if config.get("provider").is_none()
        && let Some(previous) = providers.default_name()
    {
        document["provider"] = value(previous);
    }
    let replacement = document
        .to_string()
        .parse::<toml::Table>()
        .map_err(|_| failure(&path, "cannot validate updated configuration"))?;
    let effective = Config::load_with_global(env, cwd, Some(&replacement))?;
    let updated = Providers::from_config(&effective)?;
    if updated.get(name).is_none() {
        return Err(failure(
            &path,
            "higher-priority configuration prevents active-profile membership",
        ));
    }
    if switch_source
        && !matches!(
            updated.get(name).and_then(|instance| instance.credential()),
            Some(crate::provider::CredentialSource::Store { .. })
        )
    {
        return Err(failure(
            &path,
            "higher-priority configuration prevents stored credential authority; remove its environment binding before login",
        ));
    }
    Ok((Some(document), updated))
}
