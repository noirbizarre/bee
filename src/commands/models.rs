//! Human-facing model discovery, with all output confined to the binary edge.

use std::io::{self, Write};

use bee::config::{Config, Environment};
use bee::models::{Error, Service, Settings};
use bee::provider::Providers;
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
                let profile = bee::auth::profile(providers, &name)?;
                refresh_error = service.list_endpoint(profile, environment).await.err();
                snapshot = service.snapshot();
                json!({"refreshed":refresh_error.is_none(),"profile":name,"endpoint":snapshot.endpoints.get(&name)})
            } else {
                refresh_error = service.refresh().await.err();
                snapshot = service.snapshot();
                json!({"refreshed":refresh_error.is_none(),"provider_count":snapshot.providers.len()})
            }
        }
        ModelsCommand::List {
            profile,
            catalog_provider,
            endpoint,
            ..
        } => {
            if let Some(id) = catalog_provider {
                let provider = snapshot.providers.get(&id).ok_or(Error::Unavailable)?;
                json!({"catalog_provider":id,"provenance":{"source":"models.dev","confidence":"declared","endpoint_verified":false},"models":provider.models})
            } else {
                let profile = bee::auth::profile(
                    providers,
                    profile
                        .as_deref()
                        .or_else(|| providers.default_name())
                        .unwrap_or("<default>"),
                )?;
                let endpoint_info = if endpoint || profile.discover_models() {
                    refresh_error = service.list_endpoint(profile, environment).await.err();
                    snapshot = service.snapshot();
                    snapshot.endpoints.get(profile.name()).cloned()
                } else {
                    None
                };
                let mut data = json!({"profile":profile.name(),"models":snapshot.suggestions(profile, profile.default_model())});
                if let Some(listing) = endpoint_info {
                    data["endpoint"] = serde_json::to_value(listing).expect("listing serializes");
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
                let name = model["facts"]["display_name"]["value"]
                    .as_str()
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
            "[providers.account]\ntype='openrouter'\ncredential={env='KEY'}",
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
                catalog_provider: None,
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
        assert_eq!(initial["result"]["models"][0]["id"], "native-model");
        out.clear();
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
        assert_eq!(stale["result"]["endpoint"]["models"][0], "native-model");
        assert_eq!(stale["result"]["endpoint"]["stale"], true);
        assert_eq!(stale["result"]["endpoint"]["error"], "transport");
        assert!(
            !String::from_utf8(out)
                .unwrap()
                .contains("credential-canary")
        );
        assert!(err.is_empty());
    }
}
