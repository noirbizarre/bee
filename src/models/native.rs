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
    profile: &Profile,
    credential: Option<&Credential>,
) -> Result<EndpointListing, Error> {
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
