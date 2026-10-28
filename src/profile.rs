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
    let selected = config
        .get("profile")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| invalid("profile", "requires a string selector"))
        })
        .transpose()?
        .unwrap_or("default");
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
    let mut groups: BTreeMap<String, Group> = config
        .get("profiles")
        .map(|value| {
            value.clone().try_into().map_err(|_| {
                invalid(
                    "profiles",
                    "contains invalid fields or types; profiles accept accounts and model",
                )
            })
        })
        .transpose()?
        .unwrap_or_default();
    groups.entry("default".into()).or_default();
    let baseline: Vec<_> = accounts
        .values()
        .filter(|account| account.enabled())
        .map(|account| account.name().to_owned())
        .collect();
    let mut resolved = BTreeMap::new();
    for (name, group) in groups {
        let key = format!("profiles.{name}");
        if name.trim().is_empty() || name.trim() != name || name.chars().any(char::is_control) {
            return Err(invalid(&key, "invalid profile name"));
        }
        let mut seen = BTreeSet::new();
        let mut scoped = BTreeMap::new();
        for id in group.accounts.as_ref().unwrap_or(&baseline) {
            if !seen.insert(id) {
                return Err(invalid(&key, "contains duplicate account references"));
            }
            let account = accounts
                .get(id)
                .ok_or_else(|| invalid(&key, "references an unknown account"))?;
            scoped.insert(id.clone(), account.clone());
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
        .remove(selected)
        .ok_or_else(|| invalid("profile", "selects an unknown profile"))
}
