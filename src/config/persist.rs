//! Narrow, comment-preserving user configuration transactions for explicit login.

use std::io::Write;
use std::path::Path;

use fs2::FileExt;
use toml_edit::{DocumentMut, Item, Table, TableLike, value};

use super::{Config, Environment};
use crate::account::Accounts;
use crate::{Error, Result};

/// A preflight result retains no credential and exposes no configuration values.
pub struct LoginAccount {
    name: String,
    providers: Accounts,
    import_env: Option<String>,
    import_fields: std::collections::BTreeMap<String, String>,
    // Incomplete routing settings cannot be represented by an executable Account.
    original: Option<toml::Value>,
    original_origins: std::collections::BTreeMap<String, std::path::PathBuf>,
    additions: toml::Table,
}

/// An incomplete login target has no executable account or credential authority.
pub struct LoginDraft {
    name: String,
    definition: &'static crate::provider::registry::Definition,
    settings: toml::Table,
    original: Option<toml::Value>,
    original_origins: std::collections::BTreeMap<String, std::path::PathBuf>,
}

impl LoginDraft {
    /// The frozen login target; no first-account fallback is permitted.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Safe descriptors, never configured field values.
    pub fn missing_fields(&self) -> Vec<&'static crate::provider::registry::Field> {
        self.definition
            .fields
            .iter()
            .filter(|field| {
                field.owner == crate::provider::registry::FieldOwner::Instance
                    && field.required
                    && !self.settings.contains_key(field.name)
            })
            .collect()
    }

    /// Validate one missing ordinary setting without exposing the supplied value.
    pub fn validate_input(&self, field: &str, input: &str) -> Result<()> {
        if !self
            .missing_fields()
            .iter()
            .any(|candidate| candidate.name == field)
        {
            return Err(failure(
                Path::new("<login>"),
                "only absent required instance fields may be collected",
            ));
        }
        let mut settings = self.settings.clone();
        settings.insert(field.into(), toml::Value::String(input.into()));
        self.definition
            .inspect_fields(
                crate::provider::registry::FieldOwner::Instance,
                &settings,
                false,
            )
            .map_err(|reason| failure(Path::new("<login>"), reason))
    }

    /// Freeze a fully validated proposal without publishing settings or credentials.
    pub fn complete(
        self,
        env: &Environment,
        cwd: &Path,
        additions: toml::Table,
    ) -> Result<LoginAccount> {
        for (field, value) in &additions {
            self.validate_input(
                field,
                value
                    .as_str()
                    .ok_or_else(|| failure(Path::new("<login>"), "instance fields must be text"))?,
            )?;
        }
        let config = Config::load(env, cwd)?;
        if target(&config, &self.name) != self.original.as_ref()
            || target_origins(&config, &self.name) != self.original_origins
        {
            return Err(failure(
                Path::new("<login>"),
                "account authority changed during login; repeat login",
            ));
        }
        let plan = prepare_inputs(env, cwd, &self.name, additions)?;
        // Preflight reloads layers; it must not silently accept a target changed between reads.
        if plan.original != self.original || plan.original_origins != self.original_origins {
            return Err(failure(
                Path::new("<login>"),
                "account authority changed during login; repeat login",
            ));
        }
        Ok(plan)
    }
}

fn target<'a>(config: &'a Config, name: &str) -> Option<&'a toml::Value> {
    config.table().get("accounts")?.as_table()?.get(name)
}

fn target_origins(
    config: &Config,
    name: &str,
) -> std::collections::BTreeMap<String, std::path::PathBuf> {
    fn visit(
        config: &Config,
        key: String,
        value: &toml::Value,
        origins: &mut std::collections::BTreeMap<String, std::path::PathBuf>,
    ) {
        if let Some(path) = config.origins.get(&key) {
            origins.insert(key.clone(), path.clone());
        }
        if let Some(fields) = value.as_table() {
            for (field, value) in fields {
                visit(config, format!("{key}.{field}"), value, origins);
            }
        }
    }
    let mut origins = std::collections::BTreeMap::new();
    if let Some(account) = target(config, name) {
        // Traverse the actual target: prefix matching confuses `work` with the separate `work.personal` ID.
        // Provenance still matters when a higher layer repeats the same routing or credential value.
        visit(config, format!("accounts.{name}"), account, &mut origins);
    }
    origins
}

/// Only login may defer missing settings, and only for its selected target.
pub fn begin_login(env: &Environment, cwd: &Path, name: Option<&str>) -> Result<LoginDraft> {
    let config = Config::load(env, cwd)?;
    let names = crate::profile::login_names(&config)?;
    let name = match name {
        Some(name) => name,
        None if names.len() == 1 => &names[0],
        _ => {
            return Err(failure(
                Path::new("<login>"),
                "specify an account ID; omitted login/logout requires exactly one active account",
            ));
        }
    };
    let original = target(&config, name).cloned();
    let provider = match &original {
        Some(value) => value
            .get("provider")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| {
                failure(
                    Path::new("<login>"),
                    "account requires a registered provider implementation",
                )
            })?,
        None => name,
    };
    let definition = crate::provider::registry::definition(provider).ok_or_else(|| {
        failure(
            Path::new("<login>"),
            "unknown account or provider; configure a named account first",
        )
    })?;
    let settings = original
        .as_ref()
        .and_then(|value| value.get("settings"))
        .map(|value| {
            value
                .as_table()
                .cloned()
                .ok_or_else(|| failure(Path::new("<login>"), "provider settings must be a table"))
        })
        .transpose()?
        .unwrap_or_default();
    definition
        .inspect_fields(
            crate::provider::registry::FieldOwner::Instance,
            &settings,
            false,
        )
        .map_err(|reason| failure(Path::new("<login>"), reason))?;
    let mut other = config.clone();
    if let Some(accounts) = other
        .table
        .get_mut("accounts")
        .and_then(toml::Value::as_table_mut)
    {
        accounts.remove(name);
    }
    Accounts::connections(&other)?;
    Ok(LoginDraft {
        name: name.into(),
        definition,
        settings,
        original,
        original_origins: target_origins(&config, name),
    })
}

fn supplemented(mut config: Config, name: &str, additions: &toml::Table) -> Config {
    if !additions.is_empty() {
        let accounts = config
            .table
            .entry("accounts")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .expect("validated accounts");
        let account = accounts
            .entry(name)
            .or_insert_with(|| {
                toml::Value::Table(toml::Table::from_iter([(
                    "provider".into(),
                    toml::Value::String(name.into()),
                )]))
            })
            .as_table_mut()
            .expect("validated account");
        let settings = account
            .entry("settings")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .expect("validated settings");
        settings.extend(additions.clone());
    }
    config
}

impl LoginAccount {
    /// Reject authority edits while authentication was pending, preserving unrelated edits.
    pub fn check(&self, env: &Environment, cwd: &Path) -> Result<()> {
        let config = Config::load(env, cwd)?;
        if target(&config, &self.name) != self.original.as_ref()
            || target_origins(&config, &self.name) != self.original_origins
        {
            return Err(failure(
                &env.global_config_path()
                    .unwrap_or_else(|| "<user configuration>".into()),
                "account authority changed during login; review the account and repeat login",
            ));
        }
        Ok(())
    }
    /// Declared environment source before the proposed switch to stored authority.
    pub fn import_env(&self) -> Option<&str> {
        self.import_env.as_deref()
    }
    /// Whether the source came from provider defaults rather than explicit account configuration.
    pub fn inherits_environment(&self) -> bool {
        // Compare provenance, not the variable name: an explicit binding to the default name is still authoritative.
        self.import_env.is_some()
            && self
                .original
                .as_ref()
                .and_then(|account| account.get("credential"))
                .is_none()
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
            .for_provider(instance.provider());
        credential.validate_for(instance)?;
        Ok(Some(credential))
    }
    /// Configured accounts after the proposed user-only change.
    pub fn accounts(&self) -> &Accounts {
        &self.providers
    }

    /// Re-plan under a stable lock because browser login can outlive another editor.
    pub fn commit(&self, env: &Environment, cwd: &Path) -> Result<()> {
        // Reauthentication may only change credentials: managed/read-only configuration needs no writer lock.
        self.check(env, cwd)?;
        let (document, effective) = proposed(env, cwd, &self.name, &self.additions)?;
        if document.is_none() {
            if effective.get(&self.name) != self.providers.get(&self.name) {
                return Err(failure(
                    Path::new("<effective configuration>"),
                    "account definition changed during login; review the account and repeat login",
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
        let (document, effective) = proposed(env, cwd, &self.name, &self.additions)?;
        // Browser/device authorization may outlive an edit to the target or source.
        if effective.get(&self.name) != self.providers.get(&self.name) {
            return Err(failure(
                &path,
                "account definition changed during login; credentials may have been stored; review the account and repeat login",
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
pub fn prepare(env: &Environment, cwd: &Path, name: &str) -> Result<LoginAccount> {
    prepare_inputs(env, cwd, name, toml::Table::new())
}

fn prepare_inputs(
    env: &Environment,
    cwd: &Path,
    name: &str,
    additions: toml::Table,
) -> Result<LoginAccount> {
    let config = Config::load(env, cwd)?;
    let original = target(&config, name).cloned();
    let original_origins = target_origins(&config, name);
    let config = supplemented(config, name, &additions);
    let connections = Accounts::connections(&config)?;
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
    let (_, providers) = proposed(env, cwd, name, &additions)?;
    let import_fields = connections
        .get(name)
        .map(|instance| instance.credential_env_fields().clone())
        .unwrap_or_default();
    Ok(LoginAccount {
        name: name.into(),
        providers,
        import_env,
        import_fields,
        original,
        original_origins,
        additions,
    })
}

fn table<'a>(parent: &'a mut dyn TableLike, key: &str) -> &'a mut dyn TableLike {
    parent
        .entry(key)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .expect("validated table")
}

fn proposed(
    env: &Environment,
    cwd: &Path,
    name: &str,
    additions: &toml::Table,
) -> Result<(Option<DocumentMut>, Accounts)> {
    let config = Config::load(env, cwd)?;
    let was_configured = target(&config, name).is_some();
    let config = supplemented(config, name, additions);
    // Authentication addresses configured identities even when profile membership excludes them.
    let providers = crate::auth::configured_accounts(&config)?;
    if additions.is_empty()
        && providers.get(name).is_some_and(|instance| {
            !matches!(
                instance.credential(),
                Some(crate::provider::CredentialSource::Env(_))
            )
        })
    {
        return Ok((None, providers));
    }
    let configured = was_configured;
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
        table(table(document.as_table_mut(), "accounts"), name).insert("provider", value(name));
    }
    for (field, input) in additions {
        table(
            table(table(document.as_table_mut(), "accounts"), name),
            "settings",
        )
        .insert(
            field,
            value(input.as_str().expect("validated instance field")),
        );
    }
    let connections = Accounts::connections(&config)?;
    let switch_source = connections.get(name).is_some_and(|instance| {
        matches!(
            instance.credential(),
            Some(crate::provider::CredentialSource::Env(_))
        )
    }) || (!configured
        && crate::provider::registry::definition(name)
            .is_some_and(|definition| !definition.methods.is_empty()));
    if switch_source {
        let connection = table(table(document.as_table_mut(), "accounts"), name);
        let credential = table(connection, "credential");
        credential.remove("env");
        credential.remove("fields");
        credential.insert("store", value(true));
    }
    let active = Accounts::from_config(&config)?;
    let group_name = active.profile_name();
    let key = format!("profiles.{group_name}.accounts");
    // Existing-account login must never enable an account or expand a profile's scope.
    if !configured && config.get(&key).is_some() {
        if config.origin(&key).is_some_and(|origin| origin != path) {
            return Err(failure(
                config.origin(&key).expect("known origin"),
                "active-profile membership is overridden here; add the account to that membership array",
            ));
        }
        let group = table(table(document.as_table_mut(), "profiles"), group_name);
        if let Some(members) = group.get_mut("accounts").and_then(Item::as_array_mut) {
            // Append in place so existing membership order, formatting and comments survive login.
            members.push(name);
        } else {
            // Higher-layer arrays were rejected; retain effective membership if the user layer omits it.
            let mut members = toml_edit::Array::new();
            for member in active.names() {
                members.push(member);
            }
            members.push(name);
            group.insert("accounts", value(members));
        }
    }
    let replacement = document
        .to_string()
        .parse::<toml::Table>()
        .map_err(|_| failure(&path, "cannot validate updated configuration"))?;
    let effective = Config::load_with_global(env, cwd, Some(&replacement))?;
    let updated = crate::auth::configured_accounts(&effective)?;
    if additions.iter().any(|(field, input)| {
        updated
            .get(name)
            .and_then(|account| account.settings().get(field))
            .map(String::as_str)
            != input.as_str()
    }) {
        return Err(failure(
            &path,
            "higher-priority configuration prevents collected account settings; configure those settings in the effective layer",
        ));
    }
    if !configured && Accounts::from_config(&effective)?.get(name).is_none() {
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
