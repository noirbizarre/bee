//! Optional discovery must enrich configured models without becoming a dependency.

mod support;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Client, Error as AcpError};
use bee::config::{Config, Environment};
use bee::models::{self, Cache, Clock, Error, Http, Service, Settings};
use bee::provider::{Profile, Protocol};
use futures::future::BoxFuture;
use serde_json::{Value, json};

fn catalog(npm: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"new-provider": {
        "name":"A dynamically discovered provider", "npm":"@ai-sdk/openai-compatible",
        "api":"https://must-not-be-used.example", "env":["MUST_NOT_BE_READ"],
        "models": {
            "known": {"name":"Friendly model", "family":"not-a-protocol", "tool_call":true,"structured_output":true,
                "modalities":{"input":["text","image"],"output":["text"]},
                "limit":{"context":12000,"input":10000,"output":2000},"provider":{"npm":npm,"api":"https://wrong.example"}},
            "unknown-capabilities": {"name":"No capability claims"}
        }
    }})).unwrap()
}

#[derive(Default)]
struct MemoryCache {
    bytes: Mutex<Option<Vec<u8>>>,
    fail_write: AtomicBool,
}
impl Cache for MemoryCache {
    fn load(&self) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.bytes.lock().unwrap().clone())
    }
    fn store(&self, bytes: &[u8]) -> Result<(), Error> {
        if self.fail_write.load(Ordering::SeqCst) {
            return Err(Error::Cache);
        }
        *self.bytes.lock().unwrap() = Some(bytes.to_vec());
        Ok(())
    }
}
struct Time(AtomicU64);
impl Clock for Time {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
type Request = (String, reqwest::header::HeaderMap, usize);
struct FakeHttp {
    responses: Mutex<VecDeque<Result<Vec<u8>, Error>>>,
    requests: Mutex<Vec<Request>>,
}
impl FakeHttp {
    fn new(responses: Vec<Result<Vec<u8>, Error>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }
}
impl Http for FakeHttp {
    fn get<'a>(
        &'a self,
        url: &'a str,
        headers: reqwest::header::HeaderMap,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>, Error>> {
        self.requests
            .lock()
            .unwrap()
            .push((url.into(), headers, limit));
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected discovery request");
        Box::pin(async move { response })
    }
}
fn service(
    settings: Settings,
    http: Arc<dyn Http>,
    cache: Arc<MemoryCache>,
    time: Arc<Time>,
) -> Arc<Service> {
    Arc::new(Service::new(settings, http, cache, time))
}
fn profile(extra: &str) -> Profile {
    support::configured_providers(&format!(r#"
        [providers.test]
        model = "known"
        catalog_provider = "new-provider"
        prefer = ["openai-chat"]
        protocols = {{ openai-chat = {{base_url="http://configured.example/gateway/v1"}}, openai-responses = {{}}, anthropic-messages = {{}} }}
        {extra}
    "#)).get("test").unwrap().clone()
}
fn defaults() -> (Arc<MemoryCache>, Arc<Time>) {
    (
        Arc::new(MemoryCache::default()),
        Arc::new(Time(AtomicU64::new(1000))),
    )
}

struct CopilotAccount(AtomicUsize);
impl bee::http::LoginHttp for CopilotAccount {
    fn post_json<'a>(
        &'a self,
        _: &'a str,
        _: String,
    ) -> BoxFuture<'a, Result<bee::http::HttpResponse, bee::http::Unreachable>> {
        Box::pin(async { panic!("model discovery must not start OAuth") })
    }
    fn get_authenticated<'a>(
        &'a self,
        url: &'a str,
        headers: reqwest::header::HeaderMap,
    ) -> BoxFuture<'a, Result<bee::http::HttpResponse, bee::http::Unreachable>> {
        assert_eq!(url, "https://api.github.com/copilot_internal/user");
        assert_eq!(headers["x-github-api-version"], "2025-04-01");
        assert!(headers["authorization"].is_sensitive());
        let status = self.0.load(Ordering::SeqCst) as u16;
        Box::pin(async move {
            Ok(bee::http::HttpResponse { status, body: json!({"chat_enabled":true,"endpoints":{"api":"https://api.business.githubcopilot.com"}}).to_string() })
        })
    }
}

#[tokio::test]
async fn copilot_native_discovery_binds_routes_and_limits_to_the_selected_account() {
    let profile =
        support::configured_providers("[providers.work]\npreset='github'\ncredential={env='KEY'}")
            .get("work")
            .unwrap()
            .clone();
    let http = Arc::new(FakeHttp::new(vec![Ok(json!({"data":[
        {"id":"chat-only","name":"Chat model","model_picker_enabled":true,"supported_endpoints":["/chat/completions"],"capabilities":{"limits":{"max_context_window_tokens":32000,"max_prompt_tokens":28000,"max_output_tokens":4000},"supports":{"tool_calls":true,"streaming":true}}},
        {"id":"messages-only","name":"Messages model","supported_endpoints":["/v1/messages"]},
        {"id":"responses-only","supported_endpoints":["/responses"]},
        {"id":"disabled","policy":{"state":"disabled"}},
        {"id":"not-tools","capabilities":{"supports":{"tool_calls":false}}},
        {"id":"unsupported","supported_endpoints":["/embeddings"]}
    ]}).to_string().into_bytes()), Err(Error::Transport), Err(Error::Transport)]));
    let (cache, clock) = defaults();
    let service = Service::new(Settings::default(), http.clone(), cache.clone(), clock)
        .with_login_http(Arc::new(CopilotAccount(AtomicUsize::new(200))));
    let env = Environment::default().with_variable("KEY", "first-account-secret");
    let listing = service.list_endpoint(&profile, &env).await.unwrap();
    assert_eq!(
        listing.models,
        ["chat-only", "messages-only", "responses-only"]
    );
    assert_eq!(
        listing.entries["chat-only"].metadata.input_limit,
        Some(28000)
    );
    let snapshot = service.snapshot();
    let selection = snapshot.resolve(&profile, "chat-only");
    assert_eq!(selection.protocol, Protocol::OpenAiChat);
    assert_eq!(selection.protocol_fact.source, "native_api");
    assert_eq!(selection.window, Some(32000));
    assert_eq!(
        snapshot.resolve(&profile, "messages-only").protocol,
        Protocol::AnthropicMessages
    );
    assert_eq!(
        snapshot.resolve(&profile, "responses-only").protocol,
        Protocol::OpenAiResponses
    );
    let requests = http.requests.lock().unwrap().clone();
    assert_eq!(
        requests[0].0,
        "https://api.business.githubcopilot.com/models"
    );
    assert_eq!(
        requests[0].1["authorization"],
        "Bearer first-account-secret"
    );
    assert_eq!(requests[0].1["x-github-api-version"], "2026-08-01");
    assert!(!requests[0].1.contains_key("x-api-key"));
    assert!(!requests[0].1.contains_key("x-initiator"));
    drop(requests);
    assert!(service.list_endpoint(&profile, &env).await.is_err());
    assert_eq!(service.snapshot().endpoints["work"].models, listing.models);
    let changed = Environment::default().with_variable("KEY", "replacement-account-secret");
    assert!(service.list_endpoint(&profile, &changed).await.is_err());
    assert!(service.snapshot().endpoints["work"].models.is_empty());
    assert_eq!(
        selection.window,
        Some(32000),
        "turn decisions are immutable"
    );
    assert!(
        cache.bytes.lock().unwrap().is_none(),
        "private model facts never enter public cache"
    );
}

#[tokio::test]
async fn copilot_entitlement_rejection_clears_a_previous_private_listing() {
    let profile =
        support::configured_providers("[providers.work]\npreset='github'\ncredential={env='KEY'}")
            .get("work")
            .unwrap()
            .clone();
    let http = Arc::new(FakeHttp::new(vec![Ok(
        br#"{"data":[{"id":"visible","supported_endpoints":["/responses"]}]}"#.to_vec(),
    )]));
    let account = Arc::new(CopilotAccount(AtomicUsize::new(200)));
    let (cache, clock) = defaults();
    let service =
        Service::new(Settings::default(), http, cache, clock).with_login_http(account.clone());
    let env = Environment::default().with_variable("KEY", "account-secret");
    service.list_endpoint(&profile, &env).await.unwrap();
    account.0.store(403, Ordering::SeqCst);
    assert_eq!(
        service.list_endpoint(&profile, &env).await.err().unwrap(),
        Error::Unauthorized
    );
    assert!(service.snapshot().endpoints["work"].models.is_empty());
}

#[tokio::test]
async fn openrouter_native_pages_preserve_metadata_and_last_good_account_catalogue() {
    let profile = support::configured_providers(
        "[providers.test]\npreset='openrouter'\ncredential={env='KEY'}",
    )
    .get("test")
    .unwrap()
    .clone();
    let env = Environment::default().with_variable("KEY", "private-key");
    let (cache, clock) = defaults();
    let http = Arc::new(FakeHttp::new(vec![
        Ok(json!({"data":[{"id":"z-model","name":"Native label","context_length":12000,"architecture":{"input_modalities":["text","image"],"output_modalities":["text"]},"supported_parameters":["tools"],"top_provider":{"max_completion_tokens":2000}}],"links":{"next":"/api/v1/models/user?offset=1"}}).to_string().into_bytes()),
        Ok(br#"{"data":[{"id":"a-model"}],"links":{"next":null}}"#.to_vec()),
        Err(Error::Transport),
        Err(Error::Transport),
    ]));
    let service = service(
        Settings {
            enabled: false,
            ..Default::default()
        },
        http.clone(),
        cache.clone(),
        clock.clone(),
    );
    let listing = service.list_endpoint(&profile, &env).await.unwrap();
    assert_eq!(listing.models, ["z-model", "a-model"]);
    assert_eq!(listing.provenance, "native_api");
    assert_eq!(listing.scope, "user_preferences_filtered");
    assert!(listing.inference_succeeded_at.is_empty());
    let snapshot = service.snapshot();
    assert_eq!(snapshot.native_default(&profile), Some("z-model"));
    let info = snapshot.info(&profile, "z-model");
    assert_eq!(info.facts["tool_call"].value, true);
    assert_eq!(info.facts["structured_output"].value, false);
    assert_eq!(info.facts["context_window"].value, 12000);
    assert_eq!(info.facts["context_window"].source, "native_api");
    assert!(
        !snapshot
            .info(&profile, "a-model")
            .facts
            .contains_key("tool_call")
    );
    clock.0.store(1400, Ordering::SeqCst);
    assert_eq!(
        service.list_endpoint(&profile, &env).await.err(),
        Some(Error::Transport)
    );
    let snapshot = service.snapshot();
    let retained = &snapshot.endpoints["test"];
    assert_eq!(retained.models, ["z-model", "a-model"]);
    assert_eq!(retained.fetched_at, Some(1000));
    assert_eq!(retained.attempted_at, 1400);
    assert!(retained.stale);
    assert!(snapshot.info(&profile, "z-model").facts["context_window"].stale);
    assert!(
        snapshot
            .suggestions(&profile, None)
            .iter()
            .any(|model| model.id == "z-model")
    );
    assert_eq!(
        service
            .list_endpoint(&profile, &env.with_variable("KEY", "other-account"))
            .await
            .err(),
        Some(Error::Transport)
    );
    assert!(service.snapshot().endpoints["test"].models.is_empty());
    assert!(cache.bytes.lock().unwrap().is_none());
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests[0].0, "https://openrouter.ai/api/v1/models/user");
    assert_eq!(
        requests[1].0,
        "https://openrouter.ai/api/v1/models/user?offset=1"
    );
    assert_eq!(requests[1].1["authorization"], "Bearer private-key");
}

#[tokio::test]
async fn provider_pagination_cannot_send_credentials_to_another_origin_or_path() {
    for next in [
        "https://attacker.example/models/user",
        "/api/v1/auth/keys",
        "?offset=0#fragment",
        "https://user@openrouter.ai/api/v1/models/user",
    ] {
        let profile = support::configured_providers(
            "[providers.test]\npreset='openrouter'\ncredential={env='KEY'}",
        )
        .get("test")
        .unwrap()
        .clone();
        let http = Arc::new(FakeHttp::new(vec![Ok(
            json!({"data":[{"id":"partial"}],"links":{"next":next}})
                .to_string()
                .into_bytes(),
        )]));
        let (cache, clock) = defaults();
        let service = service(Settings::default(), http.clone(), cache, clock);
        assert_eq!(
            service
                .list_endpoint(
                    &profile,
                    &Environment::default().with_variable("KEY", "credential-canary")
                )
                .await
                .err(),
            Some(Error::Malformed)
        );
        assert_eq!(http.requests.lock().unwrap().len(), 1);
        assert!(service.snapshot().endpoints["test"].models.is_empty());
    }
}

#[tokio::test]
async fn lost_authorization_removes_account_models_instead_of_offering_stale_access() {
    let (cache, clock) = defaults();
    let http = Arc::new(FakeHttp::new(vec![
        Ok(br#"{"data":[{"id":"private"}]}"#.to_vec()),
        Err(Error::Unauthorized),
    ]));
    let service = service(Settings::default(), http, cache, clock);
    let profile = support::profile("openai-chat", "https://example.test/v1", "");
    service
        .list_endpoint(&profile, &Environment::default())
        .await
        .unwrap();
    assert_eq!(
        service
            .list_endpoint(&profile, &Environment::default())
            .await
            .err(),
        Some(Error::Unauthorized)
    );
    assert!(
        !service
            .snapshot()
            .suggestions(&profile, None)
            .iter()
            .any(|model| model.id == "private")
    );
}

#[tokio::test]
async fn successful_refresh_persists_only_the_public_catalog_and_resolves_declared_facts() {
    let (cache, clock) = defaults();
    let http = Arc::new(FakeHttp::new(vec![Ok(catalog("@ai-sdk/anthropic"))]));
    let service = service(Settings::default(), http.clone(), cache.clone(), clock);
    service.refresh().await.unwrap();
    let snapshot = service.snapshot();
    assert!(snapshot.providers.contains_key("new-provider"));
    assert!(!snapshot.stale);
    let info = snapshot.info(&profile(""), "known");
    assert_eq!(info.facts["tool_call"].value, true);
    assert_eq!(info.facts["tool_call"].source, "models.dev");
    assert_eq!(info.facts["tool_call"].confidence, "declared");
    assert!(
        !snapshot
            .info(&profile(""), "unknown-capabilities")
            .facts
            .contains_key("tool_call")
    );
    let selection = snapshot.resolve(&profile(""), "known");
    assert_eq!(selection.protocol, Protocol::OpenAiChat);
    assert_eq!(selection.window, None);
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests[0].0, models::CATALOG_URL);
    assert!(requests[0].1.is_empty());
    let envelope: Value =
        serde_json::from_slice(cache.bytes.lock().unwrap().as_ref().unwrap()).unwrap();
    assert_eq!(envelope["version"], 1);
    assert!(envelope.get("endpoints").is_none());
}

#[rstest::rstest]
#[case(Error::Transport)]
#[case(Error::Rejected)]
#[case(Error::Malformed)]
#[tokio::test]
async fn refresh_failure_keeps_stale_cache_usable_for_opted_in_routing_and_budgets(
    #[case] error: Error,
) {
    let (cache, clock) = defaults();
    let http = Arc::new(FakeHttp::new(vec![
        Ok(catalog("@ai-sdk/anthropic")),
        if error == Error::Malformed {
            Ok(b"invalid json".to_vec())
        } else {
            Err(error)
        },
    ]));
    let first = service(
        Settings::default(),
        http.clone(),
        cache.clone(),
        clock.clone(),
    );
    first.refresh().await.unwrap();
    let saved = cache.bytes.lock().unwrap().clone();
    clock.0.store(100_000, Ordering::SeqCst);
    let second = service(Settings::default(), http, cache.clone(), clock);
    second.prepare().await;
    let snapshot = second.snapshot();
    assert_eq!(snapshot.error, Some(error));
    assert!(snapshot.stale);
    assert_eq!(*cache.bytes.lock().unwrap(), saved);
    let decision = snapshot.resolve(
        &profile("catalog_protocol_hints=true\ncatalog_context_window=true"),
        "known",
    );
    assert_eq!(decision.protocol, Protocol::AnthropicMessages);
    assert_eq!(decision.window, Some(12000));
    assert!(decision.protocol_fact.stale);
    assert!(decision.window_fact.unwrap().stale);
}

#[tokio::test]
async fn failed_cache_writes_do_not_discard_valid_downloaded_data() {
    let (cache, clock) = defaults();
    cache.fail_write.store(true, Ordering::SeqCst);
    let service = service(
        Settings::default(),
        Arc::new(FakeHttp::new(vec![Ok(catalog("unknown"))])),
        cache,
        clock,
    );
    service.refresh().await.unwrap();
    assert!(service.snapshot().fetched_at.is_some());
    assert_eq!(service.snapshot().cache_error, Some(Error::Cache));
}

#[tokio::test]
async fn unavailable_catalog_and_unknown_ids_keep_the_offline_resolver_and_explicit_context() {
    let (cache, clock) = defaults();
    *cache.bytes.lock().unwrap() = Some(b"broken cache".to_vec());
    let service = service(
        Settings::default(),
        Arc::new(FakeHttp::new(vec![Err(Error::Transport)])),
        cache,
        clock,
    );
    service.prepare().await;
    let profile =
        profile("catalog_protocol_hints=true\ncatalog_context_window=true\ncontext_window=9000");
    for model in ["known", "ft:custom/model", "local:latest"] {
        let selection = service.snapshot().resolve(&profile, model);
        assert_eq!(selection.protocol, Protocol::OpenAiChat);
        assert_eq!(selection.window, Some(9000));
    }
}

#[tokio::test]
async fn fresh_cache_and_disabled_refresh_never_make_a_catalog_request() {
    let (cache, clock) = defaults();
    let http = Arc::new(FakeHttp::new(vec![Ok(catalog("@ai-sdk/anthropic"))]));
    service(
        Settings::default(),
        http.clone(),
        cache.clone(),
        clock.clone(),
    )
    .refresh()
    .await
    .unwrap();
    service(
        Settings::default(),
        http.clone(),
        cache.clone(),
        clock.clone(),
    )
    .prepare()
    .await;
    clock.0.store(100_000, Ordering::SeqCst);
    let cache_only = service(
        Settings {
            refresh: false,
            ..Settings::default()
        },
        http.clone(),
        cache.clone(),
        clock.clone(),
    );
    cache_only.prepare().await;
    assert!(cache_only.snapshot().stale);
    let disabled = service(
        Settings {
            enabled: false,
            ..Settings::default()
        },
        http.clone(),
        cache,
        clock,
    );
    disabled.prepare().await;
    assert!(disabled.snapshot().fetched_at.is_none());
    assert_eq!(disabled.refresh().await, Err(Error::Unavailable));
    assert_eq!(http.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn explicit_model_and_profile_fields_override_catalog_metadata_independently() {
    let (cache, clock) = defaults();
    let service = service(
        Settings::default(),
        Arc::new(FakeHttp::new(vec![Ok(catalog("@ai-sdk/anthropic"))])),
        cache,
        clock,
    );
    service.refresh().await.unwrap();
    let profile = profile(
        r#"
        catalog_protocol_hints=true
        catalog_context_window=true
        context_window=8000
        model_context_windows={known=7000}
        model_protocols={known="openai-responses"}
        metadata={tool_call=false,output_limit=300}
        model_metadata={known={display_name="My model",structured_output=false,output_limit=200}}
    "#,
    );
    let snapshot = service.snapshot();
    let info = snapshot.info(&profile, "known");
    assert_eq!(info.facts["display_name"].value, "My model");
    assert_eq!(info.facts["tool_call"].value, false);
    assert_eq!(info.facts["structured_output"].value, false);
    assert_eq!(info.facts["output_limit"].value, 200);
    assert_eq!(info.facts["input_limit"].source, "models.dev");
    assert_eq!(info.facts["output_limit"].source, "configuration");
    let decision = snapshot.resolve(&profile, "known");
    assert_eq!(decision.protocol, Protocol::OpenAiResponses);
    assert_eq!(decision.window, Some(7000));
    assert_eq!(
        snapshot.resolve(&profile, "unknown-capabilities").window,
        Some(8000)
    );
}

#[rstest::rstest]
#[case("@ai-sdk/openai", Protocol::OpenAiResponses)]
#[case("@ai-sdk/openai-compatible", Protocol::OpenAiChat)]
#[case("@ai-sdk/anthropic", Protocol::AnthropicMessages)]
#[case("@ai-sdk/google", Protocol::OpenAiChat)]
#[case("unrecognized", Protocol::OpenAiChat)]
#[tokio::test]
async fn only_documented_npm_signals_can_hint_a_configured_protocol(
    #[case] npm: &str,
    #[case] expected: Protocol,
) {
    let (cache, clock) = defaults();
    let service = service(
        Settings::default(),
        Arc::new(FakeHttp::new(vec![Ok(catalog(npm))])),
        cache,
        clock,
    );
    service.refresh().await.unwrap();
    let snapshot = service.snapshot();
    assert_eq!(
        snapshot
            .resolve(&profile("catalog_protocol_hints=true"), "known")
            .protocol,
        expected
    );
    // Model without npm falls back to the provider npm, not its family/name.
    assert_eq!(
        snapshot
            .resolve(
                &profile("catalog_protocol_hints=true"),
                "unknown-capabilities"
            )
            .protocol,
        Protocol::OpenAiChat
    );
    let single = support::profile(
        "openai-chat",
        "http://local.example",
        "catalog_provider='new-provider'\ncatalog_protocol_hints=true",
    );
    assert_eq!(
        snapshot.resolve(&single, "known").protocol,
        Protocol::OpenAiChat
    );
}

#[test]
fn malformed_optional_limits_are_unknown_without_losing_other_model_facts() {
    let mut value: Value = serde_json::from_slice(&catalog("unknown")).unwrap();
    value["new-provider"]["models"]["known"]["limit"] =
        json!({"context":0,"input":-1,"output":u64::MAX});
    let parsed = models::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
    let entry = &parsed["new-provider"].models["known"];
    assert!(entry.context_window.is_none());
    assert!(entry.metadata.input_limit.is_none());
    assert!(entry.metadata.output_limit.is_none());
    assert_eq!(entry.metadata.tool_call, Some(true));
}

#[tokio::test]
async fn google_ai_studio_discovers_exact_models_without_public_cache_or_implicit_selection() {
    let profiles = support::configured_providers(
        "provider='studio'\n[providers.studio]\npreset='google-aistudio'\n[providers.other]\npreset='google-aistudio'",
    );
    let profile = profiles.get("studio").unwrap();
    let (cache, clock) = defaults();
    let http = Arc::new(FakeHttp::new(vec![
        Ok(br#"{"data":[{"id":"gemini-z"},{"id":"gemini-a"},{"id":"gemini-z"}]}"#.to_vec()),
        Err(Error::Transport),
    ]));
    let service = service(Settings::default(), http.clone(), cache.clone(), clock);
    let env = Environment::default().with_variable("GEMINI_API_KEY", "studio-secret");
    let listing = service.list_endpoint(profile, &env).await.unwrap();
    assert_eq!(listing.models, ["gemini-a", "gemini-z"]);
    assert!(!listing.native);
    assert!(profile.default_model().is_none());
    let snapshot = service.snapshot();
    assert!(
        snapshot
            .suggestions(profiles.get("other").unwrap(), None)
            .is_empty()
    );
    assert!(cache.bytes.lock().unwrap().is_none());
    {
        let requests = http.requests.lock().unwrap();
        assert_eq!(
            requests[0].0,
            "https://generativelanguage.googleapis.com/v1beta/openai/models"
        );
        assert_eq!(requests[0].1["authorization"], "Bearer studio-secret");
        assert!(requests[0].1["authorization"].is_sensitive());
    }
    assert!(service.list_endpoint(profile, &env).await.is_err());
    // Optional discovery failure cannot alter an explicitly selected route.
    assert_eq!(
        service
            .snapshot()
            .resolve(profile, "gemini-explicit")
            .protocol,
        Protocol::OpenAiChat
    );
}

#[tokio::test]
async fn endpoint_lists_preserve_base_paths_authentication_and_account_isolation() {
    let (cache, clock) = defaults();
    let http = Arc::new(FakeHttp::new(vec![
        Ok(br#"{"data":[{"id":"private/ft"},{"id":"known"},{"id":"known"}]}"#.to_vec()),
        Err(Error::Rejected),
    ]));
    let service = service(Settings::default(), http.clone(), cache.clone(), clock);
    let profile = support::profile(
        "openai-chat",
        "http://configured.example/gateway/v1",
        "credential={env='TEST_KEY'}\nheaders={x-team='secret-team'}",
    );
    let env = Environment::default().with_variable("TEST_KEY", "account-one");
    service.list_endpoint(&profile, &env).await.unwrap();
    let snapshot = service.snapshot();
    assert_eq!(snapshot.endpoints["test"].models, ["known", "private/ft"]);
    assert!(
        !snapshot
            .info(&profile, "private/ft")
            .facts
            .contains_key("tool_call")
    );
    {
        let request = &http.requests.lock().unwrap()[0];
        assert_eq!(request.0, "http://configured.example/gateway/v1/models");
        assert_eq!(request.1["authorization"], "Bearer account-one");
        assert_eq!(request.1["x-team"], "secret-team");
    }
    assert!(cache.bytes.lock().unwrap().is_none());
    assert_eq!(
        service
            .list_endpoint(&profile, &env.with_variable("TEST_KEY", "account-two"))
            .await
            .err(),
        Some(Error::Rejected)
    );
    assert!(service.snapshot().endpoints["test"].models.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn chatgpt_models_keep_visible_server_order_and_names_out_of_the_public_cache() {
    use bee::auth::{Audience, Credential, CredentialStore, FileStore, Secret};
    let root = tempfile::tempdir().unwrap();
    let env = Environment::new(None, Some(root.path().into()), None);
    let store = FileStore::from_environment(&env).unwrap();
    // Let the store create its private directory/file before installing an OAuth fixture.
    store
        .put(
            Audience::Provider("test"),
            &Credential::new(Secret::new("fixture")),
        )
        .unwrap();
    std::fs::write(store.path(),json!({"version":2,"providers":{"test":{"secret":"account-access-secret","expires_at":bee::auth::unix_now()+3600,"oauth":{"issuer":"https://auth.openai.com","client_id":"issued-client","subject":"account","refresh_token":"refresh-secret","id_token":"id-secret","scopes":["chatgpt.tokens.use.direct"]}}}}).to_string()).unwrap();
    let profile = support::configured_providers("[providers.test]\npreset='chatgpt'")
        .get("test")
        .unwrap()
        .clone();
    let (cache, clock) = defaults();
    let http = Arc::new(FakeHttp::new(vec![Ok(json!({"models":[
        {"slug":"z-model","display_name":"Preferred model","visibility":"list"},
        {"slug":"hidden-model","display_name":"Hidden","visibility":"hide"},
        {"slug":"a-model","display_name":"Other model","visibility":"list"}
    ]})
    .to_string()
    .into_bytes())]));
    let service = service(Settings::default(), http.clone(), cache.clone(), clock);
    let listing = service.list_endpoint(&profile, &env).await.unwrap();
    assert_eq!(listing.models, ["z-model", "a-model"]);
    let snapshot = service.snapshot();
    assert_eq!(
        snapshot.info(&profile, "a-model").facts["display_name"].value,
        "Other model"
    );
    assert!(
        snapshot
            .info(&profile, "z-model")
            .sources
            .contains(&"endpoint")
    );
    assert!(snapshot.info(&profile, "hidden-model").sources.is_empty());
    assert!(cache.bytes.lock().unwrap().is_none());
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests[0].0, "https://api.openai.com/v1/models");
    assert_eq!(
        requests[0].1["authorization"],
        "Bearer account-access-secret"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn failed_renewal_retains_only_the_same_stored_accounts_catalogue() {
    use bee::auth::{Audience, Credential, CredentialStore, FileStore, Secret};
    struct Offline;
    impl bee::http::LoginHttp for Offline {
        fn post_json<'a>(
            &'a self,
            _: &'a str,
            _: String,
        ) -> BoxFuture<'a, Result<bee::http::HttpResponse, bee::http::Unreachable>> {
            Box::pin(async { Err(bee::http::Unreachable) })
        }
    }
    for replacement in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let env = Environment::new(None, Some(root.path().into()), None);
        let store = FileStore::from_environment(&env).unwrap();
        store
            .put(
                Audience::Provider("test"),
                &Credential::new(Secret::new("fixture")),
            )
            .unwrap();
        let mut document = json!({"version":2,"providers":{"test":{"secret":"access-secret","expires_at":bee::auth::unix_now()+3600,"oauth":{"issuer":"https://auth.openai.com","client_id":"client","subject":"first-account","refresh_token":"refresh-secret","id_token":"id-secret","scopes":["chatgpt.tokens.use.direct"]}}}});
        std::fs::write(store.path(), document.to_string()).unwrap();
        let profile = support::configured_providers("[providers.test]\npreset='chatgpt'")
            .get("test")
            .unwrap()
            .clone();
        let (cache, clock) = defaults();
        let service = Service::new(Settings::default(), Arc::new(FakeHttp::new(vec![Ok(br#"{"models":[{"slug":"private-model","display_name":"Private name","visibility":"list"}]}"#.to_vec())])), cache, clock).with_login_http(Arc::new(Offline));
        service.list_endpoint(&profile, &env).await.unwrap();
        document["providers"]["test"]["expires_at"] = json!(1);
        if replacement {
            document["providers"]["test"]["oauth"]["subject"] = json!("replacement-account");
        }
        std::fs::write(store.path(), document.to_string()).unwrap();
        assert_eq!(
            service.list_endpoint(&profile, &env).await.err(),
            Some(Error::Credentials)
        );
        let snapshot = service.snapshot();
        assert_eq!(snapshot.endpoints["test"].models.is_empty(), replacement);
        assert_eq!(
            snapshot.endpoints["test"].display_names.is_empty(),
            replacement
        );
        assert_eq!(snapshot.native_default(&profile).is_none(), replacement);
    }
}

#[test]
fn account_display_names_override_public_labels_but_not_explicit_configuration() {
    let profile = profile("");
    let mut snapshot = models::Snapshot {
        providers: models::parse(&catalog("unknown")).unwrap().into(),
        ..Default::default()
    };
    snapshot.endpoints.insert(
        profile.name().into(),
        models::EndpointListing {
            models: vec!["known".into()],
            display_names: [("known".into(), "Account label".into())].into(),
            fetched_at: Some(123),
            error: None,
            ..Default::default()
        },
    );
    let info = snapshot.info(&profile, "known");
    assert_eq!(info.facts["display_name"].value, "Account label");
    assert_eq!(info.facts["display_name"].source, "endpoint");
    assert_eq!(info.facts["display_name"].fetched_at, Some(123));
    let explicit = support::configured_providers("[providers.test]\nprotocols={openai-chat={}}\ncatalog_provider='new-provider'\nmodel_metadata={known={display_name='Configured label'}}").get("test").unwrap().clone();
    assert_eq!(
        snapshot.info(&explicit, "known").facts["display_name"].value,
        "Configured label"
    );
}

#[tokio::test]
async fn endpoint_model_ids_do_not_enter_another_profiles_suggestions_or_the_public_cache() {
    let (cache, clock) = defaults();
    let http = Arc::new(FakeHttp::new(vec![Ok(
        br#"{"data":[{"id":"account-only"}]}"#.to_vec(),
    )]));
    let service = service(Settings::default(), http, cache.clone(), clock);
    let first = support::profile("openai-chat", "http://same.example/v1", "");
    service
        .list_endpoint(&first, &Environment::default())
        .await
        .unwrap();
    let second = support::configured_providers(
        "[providers.other]\nprotocols={openai-chat={base_url='http://same.example/v1'}}",
    )
    .get("other")
    .unwrap()
    .clone();
    assert!(service.snapshot().suggestions(&second, None).is_empty());
    assert!(cache.bytes.lock().unwrap().is_none());
}

#[tokio::test]
async fn unsupported_or_incomplete_endpoint_listings_do_not_claim_complete_visibility() {
    let http = FakeHttp::new(vec![Ok(
        br#"{"data":[{"id":"first"}],"has_more":true}"#.to_vec()
    )]);
    assert_eq!(
        models::endpoint_models(&http, &profile(""), &Environment::default()).await,
        Err(Error::Unsupported)
    );
    let anthropic = support::profile("anthropic-messages", "http://local.example", "");
    assert_eq!(
        models::endpoint_models(&http, &anthropic, &Environment::default()).await,
        Err(Error::Unsupported)
    );
    let missing = profile("credential={env='MISSING'}");
    assert_eq!(
        models::endpoint_models(&http, &missing, &Environment::default()).await,
        Err(Error::Unauthorized)
    );
    assert_eq!(http.requests.lock().unwrap().len(), 1);
}

#[test]
fn xdg_cache_roots_are_absolute_and_missing_home_allows_memory_only_operation() {
    assert_eq!(Environment::default().model_cache_file(), None);
    // Windows requires a drive-qualified absolute root and uses native separators.
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let cache = root.path().join("cache");
    let env = Environment::new(None, Some(home.clone()), None)
        .with_xdg_cache_home(Some("relative".into()));
    assert_eq!(
        env.model_cache_file(),
        Some(home.join(".cache").join("bee").join("models.dev.json"))
    );
    assert_eq!(
        env.with_xdg_cache_home(Some(cache.clone()))
            .model_cache_file(),
        Some(cache.join("bee").join("models.dev.json"))
    );
}

#[test]
fn file_cache_publication_is_complete_and_missing_cache_is_ordinary() {
    let root = tempfile::tempdir().unwrap();
    let cache = models::FileCache(Some(root.path().join("cache/bee/catalog.json")));
    assert!(cache.load().unwrap().is_none());
    cache.store(b"first validated document").unwrap();
    cache.store(b"replacement validated document").unwrap();
    assert_eq!(
        cache.load().unwrap().unwrap(),
        b"replacement validated document"
    );
    let files = std::fs::read_dir(root.path().join("cache/bee"))
        .unwrap()
        .count();
    assert_eq!(files, 1, "temporary files must not survive publication");
}

#[tokio::test]
async fn maximum_endpoint_listings_project_without_quadratic_membership_scans() {
    let projection = tokio::task::spawn_blocking(|| {
        let profile = support::profile("openai-chat", "http://local.example", "");
        let mut snapshot = models::Snapshot::default();
        snapshot.endpoints.insert(
            profile.name().to_owned(),
            models::EndpointListing {
                display_names: Default::default(),
                models: (0..100_000)
                    .map(|index| format!("account/model-{index:06}"))
                    .collect(),
                fetched_at: Some(1000),
                error: None,
                ..Default::default()
            },
        );
        let infos = snapshot.suggestions(&profile, Some("selected/custom"));
        assert_eq!(infos.len(), 100_002);
        assert_eq!(infos[1].id, "selected/custom");
        assert_eq!(
            infos
                .iter()
                .filter(|info| info.sources.contains(&"endpoint"))
                .count(),
            100_000
        );
        assert_eq!(infos.last().unwrap().id, "account/model-099999");
    });
    // Generous for debug builds on CI, but a full linear scan per ID performs
    // billions of comparisons and stalls the same projection on ACP's reactor.
    tokio::time::timeout(Duration::from_secs(30), projection)
        .await
        .expect("large-list projection remains responsive")
        .unwrap();
}

#[tokio::test]
async fn production_discovery_gets_json_and_rejects_redirects_without_forwarding_credentials() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let source = TcpListener::bind("127.0.0.1:0").unwrap();
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let url = format!("http://{}/models", source.local_addr().unwrap());
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        target.local_addr().unwrap()
    );
    let thread = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for response in ["HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}".to_owned(),redirect] {
            let (mut socket,_)=source.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut bytes=Vec::new();let mut byte=[0];
            while !bytes.ends_with(b"\r\n\r\n") {socket.read_exact(&mut byte).unwrap();bytes.push(byte[0]);}
            requests.push(String::from_utf8(bytes).unwrap());
            socket.write_all(response.as_bytes()).unwrap();
        }
        requests
    });
    let http = bee::http::DiscoveryHttp;
    let bytes = http
        .get(&url, reqwest::header::HeaderMap::new(), 1024)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"data":[]})
    );
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("authorization", "Bearer private-key".parse().unwrap());
    assert_eq!(http.get(&url, headers, 1024).await, Err(Error::Rejected));
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let requests = thread.join().unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.starts_with("GET /models "))
    );
    assert!(requests[1].contains("authorization: Bearer private-key"));
}

#[tokio::test]
async fn missing_npm_and_an_unconfigured_hint_use_prefer_without_inventing_endpoints() {
    let (cache, clock) = defaults();
    let value = json!({"new-provider":{"name":"Dynamic","models":{"known":{"family":"anthropic","limit":{"context":999999999999u64}}}}});
    let service = service(
        Settings::default(),
        Arc::new(FakeHttp::new(vec![Ok(serde_json::to_vec(&value).unwrap())])),
        cache,
        clock,
    );
    service.refresh().await.unwrap();
    let decision = service.snapshot().resolve(
        &profile("catalog_protocol_hints=true\ncatalog_context_window=true"),
        "known",
    );
    assert_eq!(decision.protocol, Protocol::OpenAiChat);
    assert_eq!(decision.window, None);
    assert_eq!(decision.protocol_fact.source, "prefer");
}

#[test]
fn catalog_settings_validate_types_and_merge_independently() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("xdg/bee/config.toml");
    let project = bee::config::project_config_path(root.path());
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&global, "[catalog]\nrefresh=false\nrefresh_interval_secs=30\n[providers.test]\npreset='openai'\nmetadata={tool_call=true}\nmodel_metadata={known={output_limit=300}}\n").unwrap();
    std::fs::write(&project, "[catalog]\nenabled=true\n[providers.test]\nmodel_metadata={known={tool_call=false},other={structured_output=true}}\n").unwrap();
    let config = Config::load(
        &Environment::new(Some(root.path().join("xdg")), None, None),
        root.path(),
    )
    .unwrap();
    let settings = Settings::from_config(&config).unwrap();
    assert!(settings.enabled);
    assert!(!settings.refresh);
    assert_eq!(settings.refresh_interval, Duration::from_secs(30));
    let providers = bee::provider::Providers::from_config(&config).unwrap();
    let profile = providers.get("test").unwrap();
    assert_eq!(profile.catalog_provider(), Some("openai"));
    assert_eq!(
        profile.model_metadata("known").unwrap().tool_call,
        Some(false)
    );
    assert_eq!(
        profile.model_metadata("known").unwrap().output_limit,
        Some(300)
    );
    assert_eq!(
        profile.model_metadata("other").unwrap().structured_output,
        Some(true)
    );
    std::fs::write(
        &project,
        "[providers.test]\nmodel_metadata={'sk-private-value'={tool_call='wrong'}}\n",
    )
    .unwrap();
    std::fs::write(
        &global,
        "[providers.test]\nmodel_metadata={'sk-private-value'={tool_call=true}}\n",
    )
    .unwrap();
    let error = Config::load(
        &Environment::new(Some(root.path().join("xdg")), None, None),
        root.path(),
    )
    .unwrap_err();
    assert!(!format!("{error:?} {error}").contains("sk-private-value"));
}

#[tokio::test]
async fn corrupt_future_and_unsupported_cache_envelopes_do_not_supply_hints() {
    for value in [
        json!({"version":999,"fetched_at":1,"catalog":{}}),
        json!({"version":1,"fetched_at":2000,"catalog":{}}),
        json!({"version":1,"fetched_at":1,"catalog":[]}),
    ] {
        let (cache, clock) = defaults();
        *cache.bytes.lock().unwrap() = Some(serde_json::to_vec(&value).unwrap());
        let service = service(
            Settings {
                refresh: false,
                ..Settings::default()
            },
            Arc::new(FakeHttp::new(vec![])),
            cache,
            clock,
        );
        service.prepare().await;
        assert!(service.snapshot().fetched_at.is_none());
    }
}

#[tokio::test]
async fn endpoint_invalidation_prevents_an_in_flight_old_account_response_from_publishing() {
    struct Gated {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl Http for Gated {
        fn get<'a>(
            &'a self,
            _: &'a str,
            _: reqwest::header::HeaderMap,
            _: usize,
        ) -> BoxFuture<'a, Result<Vec<u8>, Error>> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(br#"{"data":[{"id":"old-account-only"}]}"#.to_vec())
            })
        }
    }
    let http = Arc::new(Gated {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let (cache, clock) = defaults();
    let service = service(Settings::default(), http.clone(), cache, clock);
    let profile = support::profile("openai-chat", "http://local.example", "");
    let env = Environment::default();
    let (listing, ()) = tokio::join!(service.list_endpoint(&profile, &env), async {
        http.entered.notified().await;
        service.invalidate_endpoint(profile.name());
        http.release.notify_one();
    });
    assert_eq!(listing.err(), Some(Error::Credentials));
    assert!(!service.snapshot().endpoints.contains_key(profile.name()));
}

#[tokio::test]
async fn concurrent_native_refreshes_share_only_the_same_accounts_request() {
    struct Gated {
        calls: AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl Http for Gated {
        fn get<'a>(
            &'a self,
            _: &'a str,
            headers: reqwest::header::HeaderMap,
            _: usize,
        ) -> BoxFuture<'a, Result<Vec<u8>, Error>> {
            Box::pin(async move {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                let id = if headers["authorization"] == "Bearer account-one" {
                    "one"
                } else {
                    "two"
                };
                Ok(json!({"data":[{"id":id}]}).to_string().into_bytes())
            })
        }
    }
    for replacement in [false, true] {
        let http = Arc::new(Gated {
            calls: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let (cache, clock) = defaults();
        let service = service(Settings::default(), http.clone(), cache, clock);
        let profile = support::configured_providers(
            "[providers.test]\npreset='openrouter'\ncredential={env='KEY'}",
        )
        .get("test")
        .unwrap()
        .clone();
        let one = Environment::default().with_variable("KEY", "account-one");
        let two = Environment::default().with_variable(
            "KEY",
            if replacement {
                "account-two"
            } else {
                "account-one"
            },
        );
        let (first, second, ()) = tokio::join!(
            service.list_endpoint(&profile, &one),
            service.list_endpoint(&profile, &two),
            async {
                http.entered.notified().await;
                http.release.notify_one();
            }
        );
        assert_eq!(first.unwrap().models, ["one"]);
        assert_eq!(
            second.unwrap().models,
            [if replacement { "two" } else { "one" }]
        );
        assert_eq!(
            http.calls.load(Ordering::SeqCst),
            if replacement { 2 } else { 1 }
        );
    }
}

#[tokio::test]
async fn concurrent_refresh_callers_share_one_request_but_endpoint_notifications_do_not_skip_it() {
    struct Gated {
        calls: AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl Http for Gated {
        fn get<'a>(
            &'a self,
            _: &'a str,
            _: reqwest::header::HeaderMap,
            _: usize,
        ) -> BoxFuture<'a, Result<Vec<u8>, Error>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.notify_one();
                self.release.notified().await;
                Ok(catalog("@ai-sdk/openai"))
            })
        }
    }
    let http = Arc::new(Gated {
        calls: AtomicUsize::new(0),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let (cache, clock) = defaults();
    let service = service(Settings::default(), http.clone(), cache, clock);
    let (one, two, ()) = tokio::join!(service.refresh(), service.refresh(), async {
        http.entered.notified().await;
        service.invalidate_endpoint("unrelated");
        http.release.notify_one();
    });
    one.unwrap();
    two.unwrap();
    assert_eq!(http.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn background_options_preserve_the_current_custom_id_and_never_update_closed_sessions() {
    let mut updated: Value = serde_json::from_slice(&catalog("@ai-sdk/openai-compatible")).unwrap();
    updated["new-provider"]["models"]["known"]["name"] = json!("Updated display name");
    let (cache, clock) = defaults();
    let service = service(
        Settings {
            refresh: false,
            ..Settings::default()
        },
        Arc::new(FakeHttp::new(vec![
            Ok(catalog("@ai-sdk/openai-compatible")),
            Ok(serde_json::to_vec(&updated).unwrap()),
        ])),
        cache,
        clock,
    );
    service.refresh().await.unwrap();
    let profiles = support::configured_providers(
        "[providers.test]\nmodel='known'\ncatalog_provider='new-provider'\nprotocols={openai-chat={}}\n",
    );
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_discovery(service.clone());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(AcpError::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        client.connect_with(
            bee::acp::agent_with_runtime(profiles, runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let open = connection
                    .send_request(v2::NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?
                    .session_id;
                let closed = connection
                    .send_request(v2::NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::SetSessionConfigOptionRequest::new(
                        open.clone(),
                        "model",
                        "custom/private",
                    ))
                    .block_task()
                    .await?;
                connection
                    .send_request(v2::CloseSessionRequest::new(closed))
                    .block_task()
                    .await?;
                while updates.try_recv().is_ok() {}
                service.refresh().await.unwrap();
                loop {
                    let update = updates.recv().await.unwrap();
                    assert_eq!(update.session_id, open);
                    if let v2::SessionUpdate::ConfigOptionUpdate(options) = update.update {
                        if !serde_json::to_string(&options)
                            .unwrap()
                            .contains("Updated display name")
                        {
                            continue;
                        }
                        let model = options
                            .config_options
                            .iter()
                            .find(|option| option.config_id.to_string() == "model")
                            .unwrap();
                        let v2::SessionConfigKind::Select(select) = &model.kind else {
                            panic!("model selector")
                        };
                        assert_eq!(select.current_value.to_string(), "custom/private");
                        assert!(
                            serde_json::to_string(&options)
                                .unwrap()
                                .contains("custom/private")
                        );
                        break;
                    }
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn catalog_models_never_become_a_profiles_implicit_default() {
    let (cache, clock) = defaults();
    let service = service(
        Settings {
            refresh: false,
            ..Settings::default()
        },
        Arc::new(FakeHttp::new(vec![Ok(catalog("@ai-sdk/openai"))])),
        cache,
        clock,
    );
    service.refresh().await.unwrap();
    let profiles = support::configured_providers(
        "[providers.test]\ncatalog_provider='new-provider'\nprotocols={openai-chat={}}\n",
    );
    assert!(profiles.get("test").unwrap().default_model().is_none());
    assert!(
        !service
            .snapshot()
            .suggestions(profiles.get("test").unwrap(), None)
            .is_empty()
    );
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_discovery(service);
    tokio::time::timeout(
        Duration::from_secs(5),
        Client.v2().connect_with(
            bee::acp::agent_with_runtime(profiles, runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?;
                assert!(
                    session
                        .config_options
                        .iter()
                        .all(|option| option.config_id.to_string() != "model")
                );
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn native_initial_selection_is_sticky_and_survives_a_durable_restart() {
    let root = tempfile::tempdir().unwrap();
    let env = Environment::default().with_variable("KEY", "fixture-key");
    // Explicit refreshes make the ordering change deterministic without a background request.
    let profiles = support::configured_providers(
        "[providers.test]\npreset='openrouter'\ncredential={env='KEY'}\ndiscover_models=false",
    );
    let (cache, clock) = defaults();
    let service = service(
        Settings {
            enabled: false,
            ..Default::default()
        },
        Arc::new(FakeHttp::new(vec![
            Ok(br#"{"data":[{"id":"z-native"},{"id":"a-native"}]}"#.to_vec()),
            Ok(br#"{"data":[{"id":"a-native"}]}"#.to_vec()),
        ])),
        cache,
        clock,
    );
    service
        .list_endpoint(profiles.get("test").unwrap(), &env)
        .await
        .unwrap();
    let saved = Arc::new(Mutex::new(None));
    for restart in [false, true] {
        let runtime = bee::acp::Runtime::new(
            bee::agent::Limits::default(),
            bee::tool::Registry::default(),
        )
        .with_discovery(service.clone())
        .with_history(bee::history::Store::open(root.path().join("state")).unwrap());
        let saved = saved.clone();
        let service = service.clone();
        let profiles_for_request = profiles.clone();
        let env = env.clone();
        let cwd = root.path().to_path_buf();
        tokio::time::timeout(
            Duration::from_secs(5),
            Client.v2().connect_with(
                bee::acp::agent_with_runtime(profiles.clone(), runtime),
                async move |connection| {
                    connection
                        .send_request(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("test", "0"),
                        ))
                        .block_task()
                        .await?;
                    let (id, options) = if restart {
                        let id: v2::SessionId = saved.lock().unwrap().clone().unwrap();
                        let response = connection
                            .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd))
                            .block_task()
                            .await?;
                        (id, response.config_options)
                    } else {
                        let response = connection
                            .send_request(v2::NewSessionRequest::new(cwd))
                            .block_task()
                            .await?;
                        *saved.lock().unwrap() = Some(response.session_id.clone());
                        (response.session_id, response.config_options)
                    };
                    let selected = options
                        .iter()
                        .find(|option| option.config_id.to_string() == "model")
                        .unwrap();
                    let v2::SessionConfigKind::Select(select) = &selected.kind else {
                        panic!("model selector");
                    };
                    assert_eq!(select.current_value.to_string(), "z-native");
                    if !restart {
                        service
                            .list_endpoint(profiles_for_request.get("test").unwrap(), &env)
                            .await
                            .unwrap();
                    }
                    // Resume after refresh/restart must retain the selected opaque ID even when absent.
                    connection
                        .send_request(v2::CloseSessionRequest::new(id))
                        .block_task()
                        .await?;
                    Ok(())
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
    }
}

#[test]
fn metadata_validation_and_merge_conflicts_never_echo_model_key_values() {
    let root = tempfile::tempdir().unwrap();
    let file = bee::config::project_config_path(root.path());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    for metadata in [
        "{tool_call='secret'}",
        "{output_limit=0}",
        "{secret='value'}",
    ] {
        std::fs::write(&file, format!("[providers.test]\nprotocols={{openai-chat={{}}}}\nmodel_metadata={{'sk-private-value'={metadata}}}")).unwrap();
        let config = Config::load(&Environment::default(), root.path()).unwrap();
        let error = bee::provider::Providers::from_config(&config).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("sk-private-value"));
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
}

struct PendingHttp {
    entered: AtomicUsize,
    dropped: Arc<AtomicBool>,
}
struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl Http for PendingHttp {
    fn get<'a>(
        &'a self,
        _: &'a str,
        _: reqwest::header::HeaderMap,
        _: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>, Error>> {
        Box::pin(async move {
            let _guard = Dropped(self.dropped.clone());
            self.entered.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn pending_discovery_does_not_block_inference_and_disconnect_drops_its_request() {
    let dropped = Arc::new(AtomicBool::new(false));
    let http = Arc::new(PendingHttp {
        entered: AtomicUsize::new(0),
        dropped: dropped.clone(),
    });
    let (cache, clock) = defaults();
    let service = service(Settings::default(), http.clone(), cache, clock);
    let server = support::Server::start(vec![support::Response::json(support::chat_text(
        "offline catalog still works",
    ))]);
    let profiles = support::providers(
        "openai-chat",
        &server.url,
        "catalog_provider='new-provider'\ncatalog_protocol_hints=true\ncatalog_context_window=true",
    );
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_discovery(service);
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(AcpError::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        client.connect_with(
            bee::acp::agent_with_runtime(profiles, runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                while http.entered.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
                let session = connection
                    .send_request(v2::NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["question".into()]))
                    .block_task()
                    .await?;
                loop {
                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                        updates.recv().await.unwrap().update
                    {
                        assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                        break;
                    }
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    assert!(
        server
            .request()
            .head
            .starts_with("POST /prefix/chat/completions ")
    );
}

#[tokio::test]
async fn refreshed_protocol_hints_change_only_the_next_turn_and_make_native_history_portable() {
    let (cache, clock) = defaults();
    let http = Arc::new(FakeHttp::new(vec![
        Ok(catalog("@ai-sdk/openai")),
        Ok(catalog("@ai-sdk/anthropic")),
        Ok(catalog("@ai-sdk/openai")),
    ]));
    let service = service(
        Settings {
            refresh: false,
            ..Settings::default()
        },
        http,
        cache,
        clock,
    );
    service.refresh().await.unwrap();
    let server = support::Server::start(vec![
        support::Response::json(
            json!({"status":"completed","output":[{"id":"reason","type":"reasoning","summary":[],"encrypted_content":"private-native"},{"id":"m1","type":"message","role":"assistant","content":[{"type":"output_text","text":"first"}]}]}),
        ),
        support::Response::json(
            json!({"type":"message","role":"assistant","content":[{"type":"text","text":"second"}],"stop_reason":"end_turn"}),
        ),
        support::Response::json(
            json!({"status":"completed","output":[{"id":"m3","type":"message","role":"assistant","content":[{"type":"output_text","text":"third"}]}]}),
        ),
    ]);
    let profiles = support::configured_providers(&format!(
        r#"[providers.test]
        model="known"
        catalog_provider="new-provider"
        catalog_protocol_hints=true
        catalog_context_window=true
        prefer=["openai-chat"]
        protocols={{openai-chat={{base_url="{url}"}},openai-responses={{base_url="{url}"}},anthropic-messages={{base_url="{url}"}}}}
    "#,
        url = server.url
    ));
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_discovery(service.clone());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(AcpError::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        client.connect_with(
            bee::acp::agent_with_runtime(profiles, runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?;
                let options = serde_json::to_value(&session.config_options).unwrap();
                assert!(options.to_string().contains("Friendly model"));
                for index in 0..3 {
                    connection
                        .send_request(v2::PromptRequest::new(
                            session.session_id.clone(),
                            vec![format!("question {index}").into()],
                        ))
                        .block_task()
                        .await?;
                    loop {
                        match updates.recv().await.unwrap().update {
                            v2::SessionUpdate::UsageUpdate(usage) => {
                                assert_eq!(usage.size, 12000);
                                assert!(
                                    serde_json::to_string(&usage)
                                        .unwrap()
                                        .contains("models.dev")
                                );
                            }
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
                                assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                                break;
                            }
                            _ => {}
                        }
                    }
                    if index < 2 {
                        service.refresh().await.unwrap();
                    }
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let first = server.request();
    assert!(first.head.starts_with("POST /prefix/responses "));
    let second = server.request();
    assert!(second.head.starts_with("POST /prefix/v1/messages "));
    assert!(!second.body.to_string().contains("private-native"));
    let third = server.request();
    assert!(third.head.starts_with("POST /prefix/responses "));
    assert!(!third.body.to_string().contains("private-native"));
    assert!(third.body.to_string().contains("first"));
    assert!(third.body.to_string().contains("second"));
}

#[tokio::test]
async fn refresh_during_a_tool_loop_cannot_change_its_route_budget_or_output_reservation() {
    let (cache, clock) = defaults();
    let mut replacement: Value = serde_json::from_slice(&catalog("@ai-sdk/anthropic")).unwrap();
    replacement["new-provider"]["models"]["known"]["limit"]["context"] = json!(20000);
    let service = service(
        Settings {
            refresh: false,
            ..Settings::default()
        },
        Arc::new(FakeHttp::new(vec![
            Ok(catalog("@ai-sdk/openai-compatible")),
            Ok(serde_json::to_vec(&replacement).unwrap()),
        ])),
        cache,
        clock,
    );
    service.refresh().await.unwrap();
    let (first, release) = support::Response::json(json!({"choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"read-1","type":"function","function":{"name":"read","arguments":"{\"path\":\"file.txt\"}"}}]},"finish_reason":"tool_calls"}]})).gated();
    let server = support::Server::start(vec![
        first,
        support::Response::json(support::chat_text("first answer")),
        support::Response::json(
            json!({"type":"message","role":"assistant","content":[{"type":"text","text":"second answer"}],"stop_reason":"end_turn"}),
        ),
    ]);
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "tool result").unwrap();
    let profiles = support::configured_providers(&format!(
        r#"[providers.test]
        model="known"
        catalog_provider="new-provider"
        catalog_protocol_hints=true
        catalog_context_window=true
        max_output_tokens=123
        prefer=["openai-chat"]
        protocols={{openai-chat={{base_url="{url}"}},anthropic-messages={{base_url="{url}"}}}}
    "#,
        url = server.url
    ));
    let mut tools = bee::tool::Registry::default();
    tools.register(bee::tool::read::Read).unwrap();
    let runtime = bee::acp::Runtime::new(bee::agent::Limits::default(), tools)
        .with_discovery(service.clone());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(AcpError::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let server_ref = &server;
    tokio::time::timeout(
        Duration::from_secs(5),
        client.connect_with(
            bee::acp::agent_with_runtime(profiles, runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path().to_path_buf()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(
                        session.clone(),
                        vec!["first".into()],
                    ))
                    .block_task()
                    .await?;
                let first = server_ref.request_async().await;
                assert!(first.head.starts_with("POST /prefix/chat/completions "));
                assert_eq!(first.body["max_tokens"], 123);
                service.refresh().await.unwrap();
                release.send(()).unwrap();
                let mut window = None;
                loop {
                    match updates.recv().await.unwrap().update {
                        v2::SessionUpdate::UsageUpdate(usage) => {
                            window = Some(usage.size);
                        }
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
                            assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                            break;
                        }
                        _ => {}
                    }
                }
                assert_eq!(window, Some(12000));
                let followup = server_ref.request_async().await;
                assert!(followup.head.starts_with("POST /prefix/chat/completions "));
                assert_eq!(followup.body["max_tokens"], 123);
                assert!(followup.body.to_string().contains("tool result"));
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["next".into()]))
                    .block_task()
                    .await?;
                loop {
                    match updates.recv().await.unwrap().update {
                        v2::SessionUpdate::UsageUpdate(usage) => {
                            window = Some(usage.size);
                        }
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
                            assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                            break;
                        }
                        _ => {}
                    }
                }
                assert_eq!(window, Some(20000));
                let next = server_ref.request_async().await;
                assert!(next.head.starts_with("POST /prefix/v1/messages "));
                assert_eq!(next.body["max_tokens"], 123);
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

#[rstest::rstest]
#[case::failed(0)]
#[case::refused(1)]
#[case::cancelled(2)]
#[tokio::test]
async fn unsuccessful_protocol_changes_leave_committed_native_history_and_route_intact(
    #[case] kind: u8,
) {
    let (cache, clock) = defaults();
    let service = service(
        Settings {
            refresh: false,
            ..Settings::default()
        },
        Arc::new(FakeHttp::new(vec![
            Ok(catalog("@ai-sdk/openai")),
            Ok(catalog("@ai-sdk/anthropic")),
            Ok(catalog("@ai-sdk/openai")),
        ])),
        cache,
        clock,
    );
    service.refresh().await.unwrap();
    let mut unsuccessful = support::Response::json(if kind == 1 {
        json!({"type":"message","role":"assistant","content":[{"type":"text","text":"refused"}],"stop_reason":"refusal"})
    } else {
        json!({"error":"failure"})
    });
    if kind == 0 {
        unsuccessful.status = 503;
    }
    let (unsuccessful, release) = if kind == 2 {
        let (response, release) = unsuccessful.gated();
        (response, Some(release))
    } else {
        (unsuccessful, None)
    };
    let server = support::Server::start(vec![
        support::Response::json(
            json!({"status":"completed","output":[{"id":"reason","type":"reasoning","summary":[],"encrypted_content":"retained-native"},{"id":"m1","type":"message","role":"assistant","content":[{"type":"output_text","text":"retained answer"}]}]}),
        ),
        unsuccessful,
        support::Response::json(
            json!({"status":"completed","output":[{"id":"m3","type":"message","role":"assistant","content":[{"type":"output_text","text":"recovered"}]}]}),
        ),
    ]);
    let profiles = support::configured_providers(&format!(
        r#"[providers.test]
        model="known"
        catalog_provider="new-provider"
        catalog_protocol_hints=true
        prefer=["openai-responses"]
        protocols={{openai-responses={{base_url="{url}"}},anthropic-messages={{base_url="{url}"}}}}
    "#,
        url = server.url
    ));
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_discovery(service.clone());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(AcpError::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let server_ref = &server;
    tokio::time::timeout(
        Duration::from_secs(5),
        client.connect_with(
            bee::acp::agent_with_runtime(profiles, runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?
                    .session_id;
                for index in 0..3 {
                    connection
                        .send_request(v2::PromptRequest::new(
                            session.clone(),
                            vec![format!("question {index}").into()],
                        ))
                        .block_task()
                        .await?;
                    let request = server_ref.request_async().await;
                    if index == 2 {
                        assert!(request.body.to_string().contains("retained-native"));
                        assert!(!request.body.to_string().contains("question 1"));
                    }
                    if index == 1 && kind == 2 {
                        connection.send_notification(v2::CancelSessionNotification::new(
                            session.clone(),
                        ))?;
                    }
                    loop {
                        if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                            updates.recv().await.unwrap().update
                        {
                            if index == 1 && kind == 0 {
                                assert!(matches!(idle.stop_reason, Some(v2::StopReason::Error(_))));
                                break;
                            }
                            let expected = if index == 1 {
                                match kind {
                                    1 => v2::StopReason::Refusal,
                                    _ => v2::StopReason::Cancelled,
                                }
                            } else {
                                v2::StopReason::EndTurn
                            };
                            assert_eq!(idle.stop_reason, Some(expected));
                            break;
                        }
                    }
                    if index == 1
                        && let Some(release) = &release
                    {
                        release.send(()).unwrap();
                    }
                    if index < 2 {
                        service.refresh().await.unwrap();
                    }
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}
