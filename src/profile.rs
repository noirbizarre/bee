//! Profile scopes select accounts and one optional qualified model preference.

use crate::account::{Account, Accounts, ModelRef};
use crate::config::Config;
use crate::{Error, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Group {
    accounts: Option<Vec<String>>,
    model: Option<String>,
}

fn invalid(config: &Config, key: &str, reason: &'static str) -> Error {
    Error::ProfileSetting {
        path: config.origin(key).map_or_else(
            || "<configuration>".into(),
            |path| path.display().to_string(),
        ),
        reason,
    }
}

fn groups(config: &Config) -> Result<(String, BTreeMap<String, Group>)> {
    let selected = config
        .get("profile")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| invalid(config, "profile", "requires a string selector"))
        })
        .transpose()?
        .unwrap_or("default")
        .to_owned();
    let mut groups: BTreeMap<String, Group> = config
        .get("profiles")
        .map(|value| {
            value.clone().try_into().map_err(|_| {
                invalid(
                    config,
                    "profiles",
                    "contains invalid fields or types; profiles accept accounts and model",
                )
            })
        })
        .transpose()?
        .unwrap_or_default();
    groups.entry("default".into()).or_default();
    if !groups.contains_key(&selected) {
        return Err(invalid(config, "profile", "selects an unknown profile"));
    }
    Ok((selected, groups))
}

fn members<T>(
    config: &Config,
    name: &str,
    group: &Group,
    baseline: &[String],
    accounts: &BTreeMap<String, T>,
) -> Result<Vec<String>> {
    let key = format!("profiles.{name}");
    if name.trim().is_empty() || name.trim() != name || name.chars().any(char::is_control) {
        return Err(invalid(config, &key, "invalid profile name"));
    }
    let mut seen = BTreeSet::new();
    let names = group.accounts.as_deref().unwrap_or(baseline);
    for id in names {
        if !seen.insert(id) {
            return Err(invalid(
                config,
                &key,
                "contains duplicate account references",
            ));
        }
        if !accounts.contains_key(id) {
            return Err(invalid(config, &key, "references an unknown account"));
        }
    }
    Ok(names.to_vec())
}

/// Resolve only identities before login collects required routing settings.
pub(crate) fn login_names(config: &Config) -> Result<Vec<String>> {
    let invalid = || Error::ProfileSetting {
        path: "<login configuration>".into(),
        reason: "invalid account membership or profile selector; review accounts and profiles",
    };
    let accounts = config
        .table()
        .get("accounts")
        .map(|value| value.as_table().ok_or_else(invalid))
        .transpose()?
        .cloned()
        .unwrap_or_default();
    let mut baseline = Vec::new();
    for (name, account) in &accounts {
        let account = account.as_table().ok_or_else(invalid)?;
        let enabled = account
            .get("enabled")
            .map(|value| value.as_bool().ok_or_else(invalid))
            .transpose()?
            .unwrap_or(true);
        if enabled {
            baseline.push(name.clone());
        }
    }
    let (selected, groups) = groups(config)?;
    let accounts = accounts.into_iter().collect();
    let mut names = Vec::new();
    for (name, group) in &groups {
        let scoped = members(config, name, group, &baseline, &accounts)?;
        if name == &selected {
            names = scoped;
        }
    }
    Ok(names)
}

pub(crate) fn resolve(config: &Config, accounts: BTreeMap<String, Account>) -> Result<Accounts> {
    let invalid = |key: &str, reason| Error::ProfileSetting {
        path: config.origin(key).map_or_else(
            || "<configuration>".into(),
            |path| path.display().to_string(),
        ),
        reason,
    };
    if config.get("provider").is_some() || config.get("providers").is_some() {
        return Err(invalid(
            "provider",
            "unsupported setting; configure accounts and an optional account-qualified model",
        ));
    }
    let (selected, groups) = groups(config)?;
    let root_model = config
        .get("model")
        .map(|value| {
            let text = value
                .as_str()
                .ok_or_else(|| invalid("model", "requires an account-qualified model string"))?;
            ModelRef::parse(text).map_err(|reason| invalid("model", reason))
        })
        .transpose()?;
    let validate = |reference: &ModelRef, key: &str| -> Result<()> {
        let account = accounts
            .get(&reference.account)
            .ok_or_else(|| invalid(key, "model references an unknown account"))?;
        if account
            .model_filter()
            .is_some_and(|models| !models.contains(&reference.model))
            || (!account.discovery() && !account.models().contains(&reference.model))
        {
            return Err(invalid(
                key,
                "default model is not declared available by its account",
            ));
        }
        Ok(())
    };
    if let Some(reference) = &root_model {
        validate(reference, "model")?;
    }
    let baseline: Vec<_> = accounts
        .values()
        .filter(|account| account.enabled())
        .map(|account| account.name().to_owned())
        .collect();
    let mut resolved = BTreeMap::new();
    for (name, group) in groups {
        let key = format!("profiles.{name}");
        let mut scoped = BTreeMap::new();
        for id in members(config, &name, &group, &baseline, &accounts)? {
            scoped.insert(id.clone(), accounts[&id].clone());
        }
        let configured_model = if let Some(model) = group.model {
            let reference = ModelRef::parse(&model).map_err(|reason| invalid(&key, reason))?;
            validate(&reference, &key)?;
            if !scoped.contains_key(&reference.account) {
                return Err(invalid(
                    &key,
                    "default model references an account outside this profile",
                ));
            }
            Some(reference)
        } else {
            // A root preference is dormant when its account is excluded; it cannot enable an account.
            root_model
                .as_ref()
                .filter(|reference| scoped.contains_key(&reference.account))
                .cloned()
        };
        resolved.insert(
            name.clone(),
            Accounts {
                accounts: scoped,
                configured_model,
                profile_name: name,
            },
        );
    }
    resolved
        .remove(&selected)
        .ok_or_else(|| invalid("profile", "selects an unknown profile"))
}
