//! Human-facing model discovery, with all output confined to the binary edge.

use std::io::{self, Write};

use bee::account::{Account, Accounts as Providers};
use bee::config::{Config, Environment};
use bee::models::{EndpointListing, Error, Service, Settings, Snapshot};
use futures::{StreamExt, stream};
use serde_json::{Value, json};

use crate::cli::ModelsCommand;

/// Execute model commands with the same configuration and cache as ACP.
pub fn run(command: ModelsCommand) -> bee::Result<()> {
    let environment = Environment::from_process();
    let cwd = std::env::current_dir().map_err(|source| bee::Error::Io {
        path: ".".into(),
        source,
    })?;
    let config = Config::load(&environment, &cwd)?;
    let service = Service::production(Settings::from_config(&config)?, &environment);
    let providers = Providers::from_config(&config)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| bee::Error::Io {
            path: "<async runtime>".into(),
            source,
        })?;
    runtime.block_on(execute(
        command,
        &service,
        &providers,
        &environment,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    ))
}

/// Injected service and writers keep command behavior hermetic in tests.
pub async fn execute(
    command: ModelsCommand,
    service: &Service,
    providers: &Providers,
    environment: &Environment,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> bee::Result<()> {
    let json_output = match &command {
        ModelsCommand::Providers { json }
        | ModelsCommand::List { json, .. }
        | ModelsCommand::Refresh { json, .. } => *json,
    };
    // Explicit endpoint enumeration does not need a public-catalog request.
    if matches!(
        command,
        ModelsCommand::List { endpoint: true, .. } | ModelsCommand::Refresh { .. }
    ) {
        service.load_cache().await;
    } else {
        service.prepare().await;
    }
    let mut snapshot = service.snapshot();
    let mut refresh_error = None;
    let data = match command {
        ModelsCommand::Providers { .. } => {
            if snapshot.fetched_at.is_none() {
                return Err(Error::Unavailable.into());
            }
            json!({"providers": snapshot.providers.iter().map(|(id, provider)| json!({"id":id,"name":provider.name,"model_count":provider.models.len()})).collect::<Vec<_>>()})
        }
        ModelsCommand::Refresh { profile, .. } => {
            if let Some(name) = profile {
                let account = bee::auth::profile(providers, &name)?;
                refresh_error = service.list_endpoint(account, environment).await.err();
                snapshot = account_snapshot(service, account, environment);
                json!({"refreshed":refresh_error.is_none(),"account":name,"models":available_models(&snapshot, account),"endpoint":endpoint_status(snapshot.endpoints.get(&name))})
            } else {
                refresh_error = service.refresh().await.err();
                snapshot = service.snapshot();
                json!({"refreshed":refresh_error.is_none(),"provider_count":snapshot.providers.len()})
            }
        }
        ModelsCommand::List {
            profile,
            catalog,
            endpoint,
            ..
        } => {
            if let Some(id) = catalog {
                let provider = snapshot.providers.get(&id).ok_or(Error::Unavailable)?;
                json!({"catalog":id,"provenance":{"source":"models.dev","confidence":"declared","endpoint_verified":false},"models":provider.models})
            } else {
                let accounts = match profile.as_deref() {
                    Some(name) => vec![bee::auth::profile(providers, name)?],
                    None => providers
                        .names()
                        .filter_map(|name| providers.get(name))
                        .collect(),
                };
                // Limit network fan-out while retaining every account's independent result.
                let results = stream::iter(
                    accounts
                        .iter()
                        .copied()
                        .filter(|account| endpoint || account.discovery()),
                )
                .map(|account| async move {
                    (account, service.list_endpoint(account, environment).await)
                })
                .buffer_unordered(4)
                .collect::<Vec<_>>()
                .await;
                for (account, result) in results {
                    if let Err(error) = result {
                        let current = account_snapshot(service, account, environment);
                        let usable = current
                            .endpoints
                            .get(account.name())
                            .is_some_and(|listing| {
                                listing.fetched_at.is_some()
                                    && listing.error != Some(Error::Unauthorized)
                            });
                        // Stale same-account evidence is usable for ordinary listing, never forced refresh.
                        if endpoint || !usable {
                            refresh_error.get_or_insert(error);
                        }
                        writeln!(err, "Account `{}` discovery: {error}", account.name())
                            .map_err(write_error)?;
                    }
                }
                let mut models = Vec::new();
                let mut statuses = Vec::new();
                let mut selected_endpoint = Value::Null;
                for account in accounts {
                    // Guard each projection independently; a neighbouring account's evidence grants nothing.
                    let current = account_snapshot(service, account, environment);
                    models.extend(available_models(&current, account));
                    let endpoint = endpoint_status(current.endpoints.get(account.name()));
                    if profile.as_deref() == Some(account.name()) {
                        selected_endpoint = endpoint.clone();
                    }
                    statuses.push(json!({"id":account.name(),"display_name":account_label(account),"discovery":account.discovery(),"status":current.availability_status(account),"endpoint":endpoint}));
                }
                let mut data = json!({"accounts":statuses,"models":models});
                if let Some(name) = profile {
                    data["account"] = json!(name);
                    data["endpoint"] = selected_endpoint;
                }
                data
            }
        }
    };
    let status = json!({"available":snapshot.fetched_at.is_some(),"stale":snapshot.stale,"fetched_at":snapshot.fetched_at,"error":snapshot.error,"cache_error":snapshot.cache_error});
    let output = json!({"catalog":status,"result":data});
    if json_output {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&output).expect("output serializes")
        )
        .map_err(write_error)?;
    } else {
        let freshness = if snapshot.fetched_at.is_none() {
            "unavailable"
        } else if snapshot.stale {
            "stale (cached fallback)"
        } else {
            "fresh"
        };
        writeln!(
            err,
            "Models.dev: {freshness}; catalog declarations are not endpoint verification."
        )
        .map_err(write_error)?;
        if let Some(endpoint) = data.get("endpoint").filter(|value| !value.is_null()) {
            writeln!(err, "Connection catalogue: {}; last success: {}; stale: {}; discovery error: {}. Listed models are not verified inference access.", text(&endpoint["provenance"]), endpoint["fetched_at"], endpoint["stale"], endpoint["error"]).map_err(write_error)?;
        }
        if let Some(accounts) = data["accounts"].as_array() {
            for account in accounts {
                writeln!(
                    err,
                    "Account `{}` ({}): {}",
                    text(&account["id"]),
                    text(&account["display_name"]),
                    text(&account["status"])
                )
                .map_err(write_error)?;
            }
        }
        if let Some(providers) = data["providers"].as_array() {
            for provider in providers {
                writeln!(
                    out,
                    "{}\t{}\t{} models",
                    text(&provider["id"]),
                    text(&provider["name"]),
                    provider["model_count"]
                )
                .map_err(write_error)?;
            }
        } else if let Some(models) = data["models"].as_array() {
            for model in models {
                let name = model["display_name"]
                    .as_str()
                    .or_else(|| model["facts"]["display_name"]["value"].as_str())
                    .unwrap_or_else(|| text(&model["id"]));
                writeln!(
                    out,
                    "{}\t{}\t{}",
                    text(&model["id"]),
                    name,
                    model["sources"]
                )
                .map_err(write_error)?;
            }
        } else if let Some(models) = data["models"].as_object() {
            for (id, model) in models {
                writeln!(
                    out,
                    "{id}\t{}",
                    model["metadata"]["display_name"].as_str().unwrap_or(id)
                )
                .map_err(write_error)?;
            }
        } else {
            writeln!(out, "{data}").map_err(write_error)?;
        }
    }
    // A forced refresh must have an observable failure exit status even if cached
    // data remains usable; routine offline configured-model queries still succeed.
    if let Some(error) = refresh_error {
        return Err(error.into());
    }
    Ok(())
}

fn account_snapshot(service: &Service, account: &Account, environment: &Environment) -> Snapshot {
    let credential = (|| {
        let store = if matches!(
            account.credential(),
            Some(bee::provider::CredentialSource::Store { .. })
        ) {
            bee::auth::FileStore::from_environment(environment)?
        } else {
            // Environment and unauthenticated accounts do not require a private storage root.
            bee::auth::FileStore::new("")
        };
        bee::auth::resolve(account, environment, &store, bee::auth::unix_now())
    })();
    match credential {
        Ok(credential) => service.account_snapshot(account, credential.as_ref()),
        Err(_) => {
            // Keep public enrichment for declared models, but never private evidence after auth loss.
            let mut public = service.snapshot();
            public.endpoints.clear();
            public
        }
    }
}

fn account_label(account: &Account) -> &str {
    account
        .display_name()
        .or_else(|| {
            bee::provider::registry::definition(account.provider())
                .map(|definition| definition.label)
        })
        .unwrap_or(account.name())
}

fn available_models(snapshot: &Snapshot, account: &Account) -> Vec<Value> {
    snapshot
        .suggestions(account, None)
        .into_iter()
        .map(|info| {
            let mut model = serde_json::to_value(info).expect("model serializes");
            let raw = text(&model["id"]).to_owned();
            let display = model["facts"]["display_name"]["value"]
                .as_str()
                .unwrap_or(&raw)
                .to_owned();
            model["id"] = json!(format!("{}/{raw}", account.name()));
            model["raw_id"] = json!(raw);
            model["account"] = json!(account.name());
            model["account_display_name"] = json!(account_label(account));
            model["display_name"] = json!(format!("{} / {display}", account_label(account)));
            model
        })
        .collect()
}

fn endpoint_status(listing: Option<&EndpointListing>) -> Value {
    listing.map_or(Value::Null, |listing| {
        json!({
            "provenance": listing.provenance,
            "scope": listing.scope,
            "fetched_at": listing.fetched_at,
            "attempted_at": listing.attempted_at,
            "stale": listing.stale,
            "error": listing.error,
            // A timestamp summary cannot reveal model IDs excluded by the current account filter.
            "inference_succeeded_at": listing.inference_succeeded_at.values().max(),
        })
    })
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}
fn write_error(source: io::Error) -> bee::Error {
    bee::Error::Io {
        path: "<terminal>".into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct NativeHttp(AtomicUsize);
    impl bee::models::Http for NativeHttp {
        fn get<'a>(
            &'a self,
            url: &'a str,
            _: reqwest::header::HeaderMap,
            _: usize,
        ) -> futures::future::BoxFuture<'a, Result<Vec<u8>, Error>> {
            assert_eq!(url, "https://openrouter.ai/api/v1/models/user");
            let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                if first {
                    Ok(br#"{"data":[{"id":"native-model","name":"Native name"}]}"#.to_vec())
                } else {
                    Err(Error::Transport)
                }
            })
        }
    }

    #[tokio::test]
    async fn native_list_is_automatic_and_explicit_refresh_reports_stale_data_before_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(
            &path,
            "[accounts.account]\nprovider='openrouter'\ncredential={env='KEY'}",
        )
        .unwrap();
        let env =
            Environment::new(None, None, Some(path)).with_variable("KEY", "credential-canary");
        let providers = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
        let service = Service::new(
            Settings {
                enabled: false,
                ..Default::default()
            },
            Arc::new(NativeHttp(AtomicUsize::new(0))),
            Arc::new(bee::models::FileCache(None)),
            Arc::new(bee::models::SystemClock),
        );
        let (mut out, mut err) = (Vec::new(), Vec::new());
        execute(
            ModelsCommand::List {
                profile: Some("account".into()),
                catalog: None,
                endpoint: false,
                json: true,
            },
            &service,
            &providers,
            &env,
            &mut out,
            &mut err,
        )
        .await
        .unwrap();
        let initial: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(initial["result"]["endpoint"]["provenance"], "native_api");
        assert_eq!(initial["result"]["models"][0]["id"], "account/native-model");
        out.clear();
        execute(
            ModelsCommand::List {
                profile: Some("account".into()),
                catalog: None,
                endpoint: false,
                json: true,
            },
            &service,
            &providers,
            &env,
            &mut out,
            &mut err,
        )
        .await
        .unwrap();
        let ordinary: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            ordinary["result"]["models"][0]["id"],
            "account/native-model"
        );
        assert_eq!(ordinary["result"]["accounts"][0]["status"], "stale");
        assert!(String::from_utf8_lossy(&err).contains("Account `account` discovery:"));
        out.clear();
        err.clear();
        assert!(
            execute(
                ModelsCommand::Refresh {
                    profile: Some("account".into()),
                    json: true
                },
                &service,
                &providers,
                &env,
                &mut out,
                &mut err
            )
            .await
            .is_err()
        );
        let stale: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(stale["result"]["account"], "account");
        assert!(stale["result"].get("profile").is_none());
        assert_eq!(stale["result"]["models"][0]["id"], "account/native-model");
        assert!(stale["result"]["endpoint"].get("models").is_none());
        assert_eq!(stale["result"]["endpoint"]["stale"], true);
        assert_eq!(stale["result"]["endpoint"]["error"], "transport");
        assert!(
            !String::from_utf8(out)
                .unwrap()
                .contains("credential-canary")
        );
        assert!(err.is_empty());
    }

    struct PartialHttp;
    impl bee::models::Http for PartialHttp {
        fn get<'a>(
            &'a self,
            url: &'a str,
            _: reqwest::header::HeaderMap,
            _: usize,
        ) -> futures::future::BoxFuture<'a, Result<Vec<u8>, Error>> {
            Box::pin(async move {
                if url.contains("failed.example") {
                    Err(Error::Transport)
                } else {
                    Ok(br#"{"data":[{"id":"remote-only"},{"id":"hidden-model-canary","name":"Hidden display canary"}]}"#.to_vec())
                }
            })
        }
    }

    #[tokio::test]
    async fn aggregate_discovery_returns_successful_accounts_before_reporting_an_unusable_failure()
    {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(&path, "[accounts.good]\nprovider='custom'\ndiscovery=true\nmodels=['remote-only']\nprotocols={openai-chat={base_url='https://good.example/v1'}}\n[accounts.bad]\nprovider='custom'\ndiscovery=true\nprotocols={openai-chat={base_url='https://failed.example/v1'}}\n").unwrap();
        let env = Environment::new(None, None, Some(path));
        let providers = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
        let service = Service::new(
            Settings {
                enabled: false,
                ..Default::default()
            },
            Arc::new(PartialHttp),
            Arc::new(bee::models::FileCache(None)),
            Arc::new(bee::models::SystemClock),
        );
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let result = execute(
            ModelsCommand::List {
                profile: None,
                catalog: None,
                endpoint: false,
                json: true,
            },
            &service,
            &providers,
            &env,
            &mut out,
            &mut err,
        )
        .await;
        assert!(result.is_err());
        let output: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(output["result"]["models"].as_array().unwrap().len(), 1);
        assert_eq!(output["result"]["models"][0]["id"], "good/remote-only");
        assert_eq!(output["result"]["accounts"].as_array().unwrap().len(), 2);
        assert!(!String::from_utf8_lossy(&out).contains("hidden-model-canary"));
        assert!(String::from_utf8_lossy(&err).contains("Account `bad`"));
    }

    #[tokio::test]
    async fn forced_endpoint_metadata_does_not_expand_discovery_disabled_declarations() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(&path, "[accounts.local]\nprovider='custom'\ndiscovery=false\nmodels=['declared']\nprotocols={openai-chat={base_url='https://good.example/v1'}}\n").unwrap();
        let env = Environment::new(None, None, Some(path));
        let providers = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
        let service = Service::new(
            Settings {
                enabled: false,
                ..Default::default()
            },
            Arc::new(PartialHttp),
            Arc::new(bee::models::FileCache(None)),
            Arc::new(bee::models::SystemClock),
        );
        let (mut out, mut err) = (Vec::new(), Vec::new());
        execute(
            ModelsCommand::List {
                profile: None,
                catalog: None,
                endpoint: true,
                json: true,
            },
            &service,
            &providers,
            &env,
            &mut out,
            &mut err,
        )
        .await
        .unwrap();
        let output: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(output["result"]["models"].as_array().unwrap().len(), 1);
        assert_eq!(output["result"]["models"][0]["id"], "local/declared");
        let serialized = String::from_utf8(out).unwrap();
        assert!(!serialized.contains("remote-only"));
        assert!(!serialized.contains("hidden-model-canary"));
        assert!(!serialized.contains("Hidden display canary"));
    }

    #[tokio::test]
    async fn filtered_inventory_is_never_exported_by_aggregate_targeted_or_refresh_output() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(&path, "[accounts.filtered]\nprovider='custom'\ndiscovery=true\nmodels=['remote-only']\nprotocols={openai-chat={base_url='https://good.example/v1'}}\n").unwrap();
        let env = Environment::new(None, None, Some(path));
        let accounts = Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap();
        let service = Service::new(
            Settings {
                enabled: false,
                ..Default::default()
            },
            Arc::new(PartialHttp),
            Arc::new(bee::models::FileCache(None)),
            Arc::new(bee::models::SystemClock),
        );
        let account = accounts.get("filtered").unwrap();
        service.list_endpoint(account, &env).await.unwrap();
        // Old successful-inference maps must not disclose a now-filtered raw model ID either.
        service.record_inference_success(account, None, "hidden-model-canary");
        for command in [
            ModelsCommand::List {
                profile: None,
                catalog: None,
                endpoint: false,
                json: true,
            },
            ModelsCommand::List {
                profile: Some("filtered".into()),
                catalog: None,
                endpoint: true,
                json: true,
            },
            ModelsCommand::Refresh {
                profile: Some("filtered".into()),
                json: true,
            },
        ] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            execute(command, &service, &accounts, &env, &mut out, &mut err)
                .await
                .unwrap();
            let output: Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(output["result"]["models"].as_array().unwrap().len(), 1);
            assert_eq!(output["result"]["models"][0]["id"], "filtered/remote-only");
            let serialized = String::from_utf8(out).unwrap();
            for hidden in [
                "hidden-model-canary",
                "Hidden display canary",
                "\"entries\"",
                "\"display_names\"",
            ] {
                assert!(
                    !serialized.contains(hidden),
                    "unexpected private inventory: {serialized}"
                );
            }
        }
    }

    #[tokio::test]
    async fn current_credentials_guard_cli_projection_and_missing_credentials_clear_private_evidence()
     {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(&path, "[accounts.work]\nprovider='custom'\ndiscovery=true\ncredential={env='KEY'}\nprotocols={openai-chat={base_url='https://good.example/v1'}}\n").unwrap();
        let env = Environment::new(None, None, Some(path));
        let original = env.clone().with_variable("KEY", "original-secret");
        let replacement = env.clone().with_variable("KEY", "replacement-secret");
        let accounts =
            Providers::from_config(&Config::load(&original, root.path()).unwrap()).unwrap();
        let account = accounts.get("work").unwrap();
        let service = Service::new(
            Settings {
                enabled: false,
                ..Default::default()
            },
            Arc::new(PartialHttp),
            Arc::new(bee::models::FileCache(None)),
            Arc::new(bee::models::SystemClock),
        );
        service.list_endpoint(account, &original).await.unwrap();
        assert!(
            !account_snapshot(&service, account, &original)
                .available_ids(account)
                .is_empty()
        );
        for current in [&env, &replacement] {
            let guarded = account_snapshot(&service, account, current);
            assert!(guarded.endpoints.is_empty());
            assert!(guarded.available_ids(account).is_empty());
        }
    }
}
