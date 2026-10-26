//! Small provider-specific catalogue adapters, independent of inference routing.

use super::*;
use crate::auth::{Credential, CredentialStore};

fn auth_error(error: crate::Error) -> Error {
    match error {
        crate::Error::AuthNotLoggedIn { .. }
        | crate::Error::AuthExpired { .. }
        | crate::Error::AuthMissingEnv { .. } => Error::Unauthorized,
        _ => Error::Credentials,
    }
}

/// Implemented native integrations, not inference protocols or a provider allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Integration {
    /// User preference/privacy-filtered native API.
    OpenRouter,
    /// Official subscription-backed OAuth integration, separate from API keys.
    ChatGpt,
    /// GitHub user-token authenticated Copilot model catalogue.
    Copilot,
}

pub(super) fn same_account(a: Option<&Credential>, b: Option<&Credential>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.same_discovery_account(b),
        (None, None) => true,
        _ => false,
    }
}

pub(super) async fn current_credential(
    profile: &Profile,
    env: &Environment,
) -> Result<Option<Credential>, Error> {
    let profile = profile.clone();
    let env = env.clone();
    tokio::task::spawn_blocking(move || {
        let store = if matches!(
            profile.credential(),
            Some(crate::provider::CredentialSource::Store { .. })
        ) {
            crate::auth::FileStore::from_environment(&env).map_err(|_| Error::Credentials)?
        } else {
            crate::auth::FileStore::new("")
        };
        crate::auth::resolve(&profile, &env, &store, crate::auth::unix_now()).map_err(auth_error)
    })
    .await
    .map_err(|_| Error::Credentials)?
}

pub(super) async fn credential(
    profile: &Profile,
    env: &Environment,
    http: &dyn crate::http::LoginHttp,
    observe: impl FnOnce(Option<&Credential>) + Send,
) -> Result<Option<Credential>, Error> {
    if profile
        .login_spec()
        .is_some_and(crate::preset::LoginSpec::is_chatgpt)
    {
        let store =
            crate::auth::FileStore::from_environment(env).map_err(|_| Error::Credentials)?;
        let read_store = store.clone();
        let name = profile.name().to_owned();
        // Revoked required grants invalidate visibility even when renewal cannot proceed.
        let stored = tokio::task::spawn_blocking(move || {
            read_store.get(crate::auth::Audience::Provider(&name))
        })
        .await
        .map_err(|_| Error::Credentials)?
        .map_err(auth_error)?;
        // Identity is known before renewal: an outage for a replacement account
        // must never borrow the previous account's last-good catalogue.
        observe(stored.as_ref());
        if stored.as_ref().and_then(Credential::chatgpt_plan_enabled) != Some(true) {
            return Err(Error::Unauthorized);
        }
        return crate::auth::chatgpt::resolve(&store, profile.name(), http)
            .await
            .map(Some)
            .map_err(auth_error);
    }
    let credential = current_credential(profile, env).await?;
    observe(credential.as_ref());
    Ok(credential)
}

pub(super) fn empty_listing(profile: &Profile) -> EndpointListing {
    let chatgpt = profile
        .login_spec()
        .is_some_and(crate::preset::LoginSpec::is_chatgpt);
    let openrouter = profile.discovery() == Some(Integration::OpenRouter);
    EndpointListing {
        native: profile.discovery().is_some() || chatgpt,
        provenance: if chatgpt {
            "official_integration"
        } else if openrouter {
            "native_api"
        } else {
            "endpoint"
        },
        scope: if openrouter {
            "user_preferences_filtered"
        } else {
            "connection_visible"
        },
        ..Default::default()
    }
}

pub(super) async fn discover(
    http: &dyn Http,
    login_http: &dyn crate::http::LoginHttp,
    profile: &Profile,
    credential: Option<&Credential>,
) -> Result<EndpointListing, Error> {
    if profile.discovery() == Some(Integration::Copilot) {
        let credential = credential.ok_or(Error::Unauthorized)?;
        let target = crate::auth::copilot::target(credential.secret(), login_http)
            .await
            .map_err(|error| {
                if crate::auth::copilot::unauthorized(&error) {
                    Error::Unauthorized
                } else {
                    Error::Credentials
                }
            })?;
        let headers = crate::llm::request_headers(
            profile,
            Protocol::OpenAiChat,
            Some(credential.secret()),
            None,
            false,
        )
        .map_err(|_| Error::Credentials)?;
        let bytes = http
            .get(&format!("{target}/models"), headers, MAX_LIST_BYTES)
            .await?;
        if bytes.len() > MAX_LIST_BYTES {
            return Err(Error::Limit);
        }
        return copilot_listing(profile, &bytes);
    }
    let chatgpt = profile
        .login_spec()
        .is_some_and(crate::preset::LoginSpec::is_chatgpt);
    let openrouter = profile.discovery() == Some(Integration::OpenRouter);
    let mut listing = empty_listing(profile);
    let mut urls = BTreeSet::new();
    for endpoint in profile.endpoints().iter().filter(|endpoint| {
        matches!(
            endpoint.protocol(),
            Protocol::OpenAiChat | Protocol::OpenAiResponses
        )
    }) {
        let url = format!(
            "{}/models{}",
            endpoint.base_url(),
            if openrouter { "/user" } else { "" }
        );
        if !urls.insert(url.clone()) {
            continue;
        }
        let base = reqwest::Url::parse(&url).map_err(|_| Error::Malformed)?;
        let headers = crate::llm::request_headers(
            profile,
            endpoint.protocol(),
            credential.map(Credential::secret),
            None,
            false,
        )
        .map_err(|_| Error::Credentials)?;
        let mut next = Some(base.clone());
        let mut visited = BTreeSet::new();
        let mut total = 0usize;
        while let Some(url) = next.take() {
            // A provider-controlled pagination link must never redirect credentials.
            if url.origin() != base.origin()
                || url.path() != base.path()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
                || !visited.insert(url.to_string())
                || visited.len() > 100
            {
                return Err(Error::Malformed);
            }
            let bytes = http
                .get(url.as_str(), headers.clone(), MAX_LIST_BYTES)
                .await?;
            total = total.saturating_add(bytes.len());
            if bytes.len() > MAX_LIST_BYTES || total > MAX_CATALOG_BYTES {
                return Err(Error::Limit);
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Malformed)?;
            if value["has_more"] == true || value.get("next").is_some_and(|v| !v.is_null()) {
                return Err(Error::Unsupported);
            }
            if let Some(link) = value["links"].get("next").filter(|v| !v.is_null()) {
                if !openrouter {
                    return Err(Error::Unsupported);
                }
                next = Some(
                    url.join(link.as_str().ok_or(Error::Malformed)?)
                        .map_err(|_| Error::Malformed)?,
                );
            }
            let data = value[if chatgpt { "models" } else { "data" }]
                .as_array()
                .ok_or(Error::Malformed)?;
            if listing.models.len() + data.len() > 100_000 {
                return Err(Error::Limit);
            }
            for item in data {
                if chatgpt && item["visibility"] != "list" {
                    continue;
                }
                let id = item[if chatgpt { "slug" } else { "id" }]
                    .as_str()
                    .ok_or(Error::Malformed)?;
                crate::provider::check_model_id(id).map_err(|_| Error::Malformed)?;
                if listing.entries.contains_key(id) {
                    continue;
                }
                let mut entry = Entry::default();
                entry.metadata.display_name = item[if chatgpt { "display_name" } else { "name" }]
                    .as_str()
                    .map(str::to_owned);
                if chatgpt && entry.metadata.display_name.is_none() {
                    return Err(Error::Malformed);
                }
                if openrouter {
                    let modalities = |key| {
                        item["architecture"][key].as_array().and_then(|values| {
                            values
                                .iter()
                                .map(|v| v.as_str().map(str::to_owned))
                                .collect()
                        })
                    };
                    entry.context_window = positive(&item["context_length"]);
                    entry.metadata.input_modalities = modalities("input_modalities");
                    entry.metadata.output_modalities = modalities("output_modalities");
                    entry.metadata.output_limit =
                        positive(&item["top_provider"]["max_completion_tokens"]);
                    if let Some(parameters) = item["supported_parameters"].as_array() {
                        entry.metadata.tool_call = Some(parameters.iter().any(|p| p == "tools"));
                        entry.metadata.structured_output =
                            Some(parameters.iter().any(|p| p == "structured_outputs"));
                    }
                }
                if !entry.metadata.valid() {
                    return Err(Error::Malformed);
                }
                // Missing labels must not overwrite richer public metadata with a bare ID.
                if let Some(name) = &entry.metadata.display_name {
                    listing.display_names.insert(id.into(), name.clone());
                }
                listing.models.push(id.into());
                listing.entries.insert(id.into(), entry);
            }
        }
    }
    if urls.is_empty() {
        return Err(Error::Unsupported);
    }
    if !listing.native {
        listing.models.sort();
    }
    Ok(listing)
}

fn copilot_listing(profile: &Profile, bytes: &[u8]) -> Result<EndpointListing, Error> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| Error::Malformed)?;
    let data = value["data"].as_array().ok_or(Error::Malformed)?;
    if data.len() > 100_000 {
        return Err(Error::Limit);
    }
    let mut listing = empty_listing(profile);
    listing.provenance = "native_api";
    listing.scope = "account_visible";
    for item in data {
        // Picker/policy visibility limits automatic choices, not the routing facts needed by explicit selections.
        let automatic = item["model_picker_enabled"] != false
            && item["policy"]["state"] != "disabled"
            && item["capabilities"]["supports"]["tool_calls"] != false
            && item["capabilities"]["supports"]["streaming"] != false;
        let id = item["id"].as_str().ok_or(Error::Malformed)?;
        crate::provider::check_model_id(id).map_err(|_| Error::Malformed)?;
        if listing.entries.contains_key(id) {
            continue;
        }
        let protocols = item
            .get("supported_endpoints")
            .map(|value| {
                value
                    .as_array()
                    .ok_or(Error::Malformed)?
                    .iter()
                    .map(|endpoint| {
                        Ok(match endpoint.as_str().ok_or(Error::Malformed)? {
                            "/chat/completions" => Some(Protocol::OpenAiChat),
                            "/responses" => Some(Protocol::OpenAiResponses),
                            "/v1/messages" => Some(Protocol::AnthropicMessages),
                            _ => None,
                        })
                    })
                    .collect::<Result<Vec<_>, Error>>()
                    .map(|items| items.into_iter().flatten().collect::<Vec<_>>())
            })
            .transpose()?;
        let mut entry = Entry {
            protocols,
            ..Entry::default()
        };
        entry.metadata.display_name = item["name"].as_str().map(str::to_owned);
        let limits = &item["capabilities"]["limits"];
        entry.context_window = positive(&limits["max_context_window_tokens"]);
        entry.metadata.output_limit = positive(&limits["max_output_tokens"]);
        entry.metadata.input_limit = positive(&limits["max_prompt_tokens"]);
        let supports = &item["capabilities"]["supports"];
        entry.metadata.tool_call = supports["tool_calls"].as_bool();
        entry.metadata.structured_output = supports["structured_outputs"].as_bool();
        entry.metadata.input_modalities = supports["vision"].as_bool().map(|vision| {
            if vision {
                vec!["text".into(), "image".into()]
            } else {
                vec!["text".into()]
            }
        });
        if !entry.metadata.valid() {
            return Err(Error::Malformed);
        }
        if let Some(name) = &entry.metadata.display_name {
            listing.display_names.insert(id.into(), name.clone());
        }
        // An explicitly unsupported route remains evidence for manual-selection errors, not an automatic choice.
        if automatic
            && entry.protocols.as_ref().is_none_or(|protocols| {
                profile
                    .endpoints()
                    .iter()
                    .any(|endpoint| protocols.contains(&endpoint.protocol()))
            })
        {
            listing.models.push(id.into());
        }
        listing.entries.insert(id.into(), entry);
    }
    Ok(listing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copilot_non_picker_models_retain_routes_and_context_for_explicit_selection() {
        let profile =
            crate::test_support::configured_providers("[providers.work]\npreset='github'")
                .get("work")
                .unwrap()
                .clone();
        let listing = copilot_listing(&profile, br#"{"data":[
            {"id":"hidden-model","model_picker_enabled":false,"supported_endpoints":["/chat/completions"],"capabilities":{"limits":{"max_context_window_tokens":32000}}},
            {"id":"picker-model","model_picker_enabled":true,"supported_endpoints":["/responses"]}
        ]}"#).unwrap();
        assert_eq!(listing.models, ["picker-model"]);
        let mut snapshot = Snapshot::default();
        snapshot.endpoints.insert("work".into(), listing);
        let selection = snapshot.resolve(&profile, "hidden-model");
        assert_eq!(selection.protocol, Protocol::OpenAiChat);
        assert_eq!(selection.window, Some(32000));
        assert!(
            snapshot
                .check_route(&profile, "hidden-model", Protocol::OpenAiResponses)
                .is_err()
        );
    }

    #[test]
    fn copilot_explicit_routes_override_native_facts_but_cannot_violate_them() {
        let profile = crate::test_support::configured_providers("[providers.work]\npreset='github'\nmodel_protocols={multi='openai-chat',conflict='openai-responses'}\ncontext_window=64000").get("work").unwrap().clone();
        let listing = copilot_listing(&profile, br#"{"data":[
            {"id":"multi","supported_endpoints":["/responses","/chat/completions"],"capabilities":{"limits":{"max_context_window_tokens":32000}}},
            {"id":"conflict","supported_endpoints":["/chat/completions"]},
            {"id":"future-only","supported_endpoints":["/unknown"]},
            {"id":"prompt-only","capabilities":{"limits":{"max_prompt_tokens":8000}}}
        ]}"#).unwrap();
        let mut snapshot = Snapshot::default();
        snapshot.endpoints.insert("work".into(), listing);
        let selection = snapshot.resolve(&profile, "multi");
        assert_eq!(selection.protocol, Protocol::OpenAiChat);
        assert_eq!(selection.protocol_fact.source, "configuration");
        assert_eq!(selection.window, Some(64000));
        assert!(
            snapshot
                .check_route(&profile, "multi", selection.protocol)
                .is_ok()
        );
        assert!(
            snapshot
                .check_route(
                    &profile,
                    "conflict",
                    snapshot.resolve(&profile, "conflict").protocol
                )
                .is_err()
        );
        assert!(
            snapshot
                .check_route(&profile, "future-only", Protocol::OpenAiResponses)
                .is_err()
        );
        assert!(
            snapshot.endpoints["work"].entries["prompt-only"]
                .context_window
                .is_none()
        );
    }
}
