//! Named, runtime-fixed scopes over authenticated connections.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::config::Config;
use crate::provider::{Profile, Providers, check_model_id};
use crate::{Error, Result};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Group {
    #[serde(default)]
    connections: Vec<String>,
    provider: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, Preferences>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    model: Option<String>,
    models: Option<Vec<String>>,
}

pub(crate) fn resolve(
    config: &Config,
    connections: BTreeMap<String, Profile>,
) -> Result<Providers> {
    let invalid = |key: &str, reason| Error::ProfileSetting {
        path: config
            .origin(key)
            .map_or_else(|| "<default>".into(), |p| p.display().to_string()),
        reason,
    };
    let selected = match config.get("profile") {
        None => "default",
        Some(value) => value
            .as_str()
            .ok_or_else(|| invalid("profile", "requires a string selector"))?,
    };
    let mut groups: BTreeMap<String, Group> = match config.get("profiles") {
        None => BTreeMap::new(),
        Some(value) => value.clone().try_into().map_err(|_| {
            invalid(
                "profiles",
                "contains an invalid declaration; check field names and types",
            )
        })?,
    };
    if groups.is_empty() {
        if selected != "default" {
            return Err(invalid(
                "profile",
                "selects an unknown profile; only the implicit default exists",
            ));
        }
        return Providers::legacy(config, connections);
    }
    // Once explicit groups exist, implicit membership would expose accounts
    // the user never opted into. The guaranteed default is therefore empty.
    groups.entry("default".into()).or_default();
    let mut resolved = BTreeMap::new();
    for (name, group) in groups {
        let key = format!("profiles.{name}");
        if name.trim().is_empty() || name.trim() != name || name.chars().any(char::is_control) {
            return Err(invalid(
                &key,
                "requires nonempty profile names without surrounding whitespace or control characters",
            ));
        }
        let mut members = BTreeSet::new();
        let mut scoped = BTreeMap::new();
        for member in group.connections {
            if !members.insert(member.clone()) {
                return Err(invalid(&key, "contains a duplicate connection reference"));
            }
            let connection = connections
                .get(&member)
                .ok_or_else(|| invalid(&key, "references an unknown connection"))?;
            scoped.insert(member, connection.clone());
        }
        for (member, preferences) in group.models {
            let connection = scoped.remove(&member).ok_or_else(|| {
                invalid(
                    &key,
                    "has model preferences for a connection outside its membership",
                )
            })?;
            for model in preferences
                .model
                .iter()
                .chain(preferences.models.iter().flatten())
            {
                check_model_id(model).map_err(|_| invalid(&key, "contains an invalid model ID"))?;
            }
            scoped.insert(
                member,
                connection.with_model_preferences(preferences.model, preferences.models),
            );
        }
        let default = match group.provider {
            Some(provider) if scoped.contains_key(&provider) => Some(provider),
            Some(_) => {
                return Err(invalid(
                    &key,
                    "selects a default connection outside its membership",
                ));
            }
            None if scoped.len() > 1 => {
                return Err(invalid(
                    &key,
                    "requires provider to choose among multiple connections",
                ));
            }
            None => scoped.keys().next().cloned(),
        };
        resolved.insert(
            name.clone(),
            Providers {
                profiles: scoped,
                default,
                profile_name: name,
            },
        );
    }
    resolved.remove(selected).ok_or_else(|| {
        invalid(
            "profile",
            "selects an unknown profile; define it under [profiles]",
        )
    })
}
