//! Synthetic ADC and official-endpoint wire contracts; no Google account is used.

use super::*;
use crate::test_support::{Response, Server, configured_providers};
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicUsize, Ordering},
};

const ACCESS: &str = "fixture-cloud-access-secret";
const REFRESH: &str = "fixture-cloud-refresh-secret";
const KEY: &str = include_str!("../../../tests/fixtures/oidc-test-key.pem");

fn profile(extra: &str) -> Profile {
    configured_providers(&format!("[providers.work]\npreset='google-cloud'\nmodel='google/gemini-2.5-flash'\ngoogle_cloud={{project='resource-project',location='global'}}\n{extra}"))
        .get("work").unwrap().clone()
}

fn user() -> Value {
    json!({"type":"authorized_user","client_id":"fixture-client","client_secret":"fixture-client-secret","refresh_token":REFRESH,"quota_project_id":"adc-quota"})
}

pub(crate) struct Fixture {
    pub(crate) calls: AtomicUsize,
    email: StdMutex<String>,
    grants: StdMutex<Vec<Vec<(String, String)>>>,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            email: StdMutex::new("worker@example.iam.gserviceaccount.com".into()),
            grants: StdMutex::new(Vec::new()),
        }
    }
}

impl Http for Fixture {
    fn request(&self, request: Request) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            match request {
                Request::MetadataIdentity => return Ok(json!(*self.email.lock().unwrap())),
                Request::Token(pairs) => self.grants.lock().unwrap().push(
                    pairs
                        .into_iter()
                        .map(|(k, v)| (k, v.expose().to_owned()))
                        .collect(),
                ),
                Request::MetadataToken => {}
            }
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(
                json!({"access_token":format!("{ACCESS}-{index}"),"token_type":"Bearer","expires_in":3600}),
            )
        })
    }
}

struct Sandbox {
    root: tempfile::TempDir,
    env: Environment,
    path: PathBuf,
}

impl Sandbox {
    fn new(document: Value) -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("adc.json");
        std::fs::write(&path, document.to_string()).unwrap();
        let env = Environment::new(None, Some(root.path().into()), None)
            .with_variable("GOOGLE_APPLICATION_CREDENTIALS", path.to_str().unwrap());
        Self { root, env, path }
    }
}

#[tokio::test]
async fn refresh_rotates_access_tokens_without_changing_selection_identity_or_double_refreshing() {
    let sandbox = Sandbox::new(user());
    let profile = profile("");
    let http = Arc::new(Fixture::new());
    let session = Session::with_http(&profile, &sandbox.env, http.clone())
        .await
        .unwrap();
    let first = session.credential().await.unwrap();
    assert_eq!(session.quota_project(), Some("adc-quota"));
    let reference =
        crate::selection::Reference::new(&profile, "google/gemini-2.5-flash".into(), Some(&first));
    assert!(reference.matches(&profile, identity(&profile, &sandbox.env).unwrap().as_ref()));
    session
        .state
        .lock()
        .await
        .credential
        .as_mut()
        .unwrap()
        .expires_at = Some(1);
    let (a, b) = tokio::join!(session.credential(), session.credential());
    let second = a.unwrap();
    assert_eq!(second.secret(), b.unwrap().secret());
    assert_ne!(first.secret(), second.secret());
    assert!(reference.matches(&profile, Some(&second)));
    assert_eq!(http.calls.load(Ordering::SeqCst), 2);
    let grants = http.grants.lock().unwrap();
    assert_eq!(
        grants[0]
            .iter()
            .find(|(k, _)| k == "refresh_token")
            .unwrap()
            .1,
        REFRESH
    );
    let debug = format!("{first:?} {second:?}");
    assert!(!debug.contains(ACCESS));
    assert!(!debug.contains(REFRESH));
}

#[tokio::test]
async fn replacement_credentials_and_changed_targets_cannot_inherit_an_automatic_choice() {
    let sandbox = Sandbox::new(user());
    let profile = profile("");
    let http = Arc::new(Fixture::new());
    let session = Session::with_http(&profile, &sandbox.env, http.clone())
        .await
        .unwrap();
    let credential = session.credential().await.unwrap();
    let reference = crate::selection::Reference::new(
        &profile,
        "google/gemini-2.5-flash".into(),
        Some(&credential),
    );
    for settings in [
        "project='other-project'",
        "project='resource-project',location='us-central1'",
        "project='resource-project',quota_project='other-quota'",
    ] {
        let other = configured_providers(&format!(
            "[providers.work]\npreset='google-cloud'\ngoogle_cloud={{{settings}}}"
        ));
        assert!(!reference.matches(other.get("work").unwrap(), Some(&credential)));
        assert!(session.check(other.get("work").unwrap()).is_err());
    }
    let mut changed = user();
    changed["refresh_token"] = "replacement-secret".into();
    std::fs::write(&sandbox.path, changed.to_string()).unwrap();
    let error = session.credential().await.unwrap_err();
    assert!(error.to_string().contains("changed"));
    assert!(!format!("{error:?}").contains("replacement-secret"));
    assert_eq!(http.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn service_account_assertions_are_signed_and_scoped_for_the_fixed_google_token_audience() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use rsa::{pkcs1::DecodeRsaPrivateKey, traits::PublicKeyParts};
    let sandbox = Sandbox::new(
        json!({"type":"service_account","client_email":"worker@example.iam.gserviceaccount.com","private_key":KEY,"token_uri":TOKEN_URL}),
    );
    let http = Arc::new(Fixture::new());
    Session::with_http(&profile(""), &sandbox.env, http.clone())
        .await
        .unwrap();
    let grants = http.grants.lock().unwrap();
    let assertion = &grants[0].iter().find(|(k, _)| k == "assertion").unwrap().1;
    let private = rsa::RsaPrivateKey::from_pkcs1_pem(KEY).unwrap();
    let key = jsonwebtoken::DecodingKey::from_rsa_components(
        &URL_SAFE_NO_PAD.encode(private.n().to_bytes_be()),
        &URL_SAFE_NO_PAD.encode(private.e().to_bytes_be()),
    )
    .unwrap();
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.set_audience(&[TOKEN_URL]);
    validation.set_issuer(&["worker@example.iam.gserviceaccount.com"]);
    let claims = jsonwebtoken::decode::<Value>(assertion, &key, &validation)
        .unwrap()
        .claims;
    assert_eq!(claims["scope"], SCOPE);
    assert_eq!(
        claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
        3600
    );
}

#[tokio::test]
async fn metadata_account_identity_is_verified_before_each_step_without_using_token_bytes() {
    let root = tempfile::tempdir().unwrap();
    let env = Environment::new(None, Some(root.path().into()), None);
    let profile = profile("");
    assert!(identity(&profile, &env).unwrap().is_none());
    let http = Arc::new(Fixture::new());
    let session = Session::with_http(&profile, &env, http.clone())
        .await
        .unwrap();
    let credential = session.credential().await.unwrap();
    assert!(credential.cloud_identity.is_some());
    *http.email.lock().unwrap() = "other@example.iam.gserviceaccount.com".into();
    assert!(
        session
            .credential()
            .await
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
    assert_eq!(http.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn explicit_invalid_adc_never_falls_back_and_credential_errors_are_redacted() {
    for document in [
        json!({"type":"external_account","credential_source":{"executable":{"command":REFRESH}}}),
        json!({"type":"impersonated_service_account","service_account_impersonation_url":REFRESH}),
        json!({"type":"authorized_user","refresh_token":REFRESH}),
        {
            let mut d = user();
            d["token_uri"] = format!("https://evil.example/{REFRESH}").into();
            d
        },
        {
            let mut d = user();
            d["quota_project_id"] = format!("../{REFRESH}").into();
            d
        },
    ] {
        let sandbox = Sandbox::new(document);
        let http = Arc::new(Fixture::new());
        let error = Session::with_http(&profile(""), &sandbox.env, http.clone())
            .await
            .err()
            .unwrap();
        assert!(!format!("{error:?} {error}").contains(REFRESH));
        assert_eq!(http.calls.load(Ordering::SeqCst), 0);
    }
    let sandbox = Sandbox::new(user());
    std::fs::remove_file(&sandbox.path).unwrap();
    assert!(identity(&profile(""), &sandbox.env).is_err());
    std::fs::write(&sandbox.path, format!("{{{REFRESH}")).unwrap();
    assert!(!format!("{:?}", identity(&profile(""), &sandbox.env).unwrap_err()).contains(REFRESH));
    std::fs::write(&sandbox.path, vec![b' '; MAX_DOCUMENT + 1]).unwrap();
    assert!(identity(&profile(""), &sandbox.env).is_err());
}

#[tokio::test]
async fn explicit_adc_precedes_local_adc_and_explicit_quota_project_precedes_adc_quota() {
    let sandbox = Sandbox::new(user());
    let local = sandbox.root.path().join("gcloud");
    std::fs::create_dir(&local).unwrap();
    std::fs::write(
        local.join("application_default_credentials.json"),
        "invalid local JSON",
    )
    .unwrap();
    let env = sandbox
        .env
        .clone()
        .with_variable("CLOUDSDK_CONFIG", local.to_str().unwrap());
    let profile = configured_providers("[providers.work]\npreset='google-cloud'\ngoogle_cloud={project='resource-project',quota_project='explicit-quota'}").get("work").unwrap().clone();
    let session = Session::with_http(&profile, &env, Arc::new(Fixture::new()))
        .await
        .unwrap();
    assert_eq!(session.quota_project(), Some("explicit-quota"));
    let local_env = Environment::new(None, None, None)
        .with_variable("CLOUDSDK_CONFIG", local.to_str().unwrap());
    assert!(identity(&profile, &local_env).is_err());
    std::fs::write(
        local.join("application_default_credentials.json"),
        user().to_string(),
    )
    .unwrap();
    assert!(identity(&profile, &local_env).unwrap().is_some());
}

fn stream(delta: Value, finish: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]}),
        json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3}})
    )
}

#[tokio::test]
async fn cloud_signed_tools_replay_exact_arguments_and_renew_before_the_next_model_step() {
    let sandbox = Sandbox::new(user());
    let profile = profile("");
    let http = Arc::new(Fixture::new());
    let session = Session::with_http(&profile, &sandbox.env, http.clone())
        .await
        .unwrap();
    let server = Server::start(vec![
        Response::sse(stream(
            json!({"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"lookup","arguments":"{ \"key\" : \"value\" }"},"extra_content":{"google":{"thought_signature":"private-signature"}}}]}),
            "tool_calls",
        )),
        Response::sse(stream(json!({"content":"done"}), "stop")),
    ]);
    let model = crate::llm::Model::new(profile, "google/gemini-2.5-flash".into(), None)
        .unwrap()
        .with_google_cloud_session(session.clone())
        .with_instructions("system guidance".into())
        .with_test_http(crate::http::InferenceHttp::test_routed(server.url.clone()));
    let tools = [crate::llm::Tool {
        name: "lookup".into(),
        description: "look up a key".into(),
        parameters: json!({"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}),
    }];
    let mut messages = vec![crate::llm::Message::User("question".into())];
    let first = model.step(&messages, &tools, &mut |_| {}).await.unwrap();
    assert_eq!(first.stop, crate::llm::Stop::Tools);
    assert_eq!(first.usage.input, Some(12));
    messages.push(first.message);
    messages.push(crate::llm::Message::Tool {
        id: "call-1".into(),
        is_error: false,
        output: "result".into(),
    });
    // Round-trip durable representation before the continuation request.
    let messages: Vec<crate::llm::Message> =
        serde_json::from_value(serde_json::to_value(messages).unwrap()).unwrap();
    session
        .state
        .lock()
        .await
        .credential
        .as_mut()
        .unwrap()
        .expires_at = Some(1);
    let second = model.step(&messages, &tools, &mut |_| {}).await.unwrap();
    assert!(
        matches!(second.message, crate::llm::Message::Assistant { ref text, .. } if text == "done")
    );
    let first_request = server.request();
    let second_request = server.request();
    assert!(
        first_request
            .head
            .starts_with("POST /prefix/chat/completions")
    );
    assert!(
        first_request
            .head
            .contains(&format!("authorization: Bearer {ACCESS}-0"))
    );
    assert!(
        second_request
            .head
            .contains(&format!("authorization: Bearer {ACCESS}-1"))
    );
    assert!(
        second_request
            .head
            .contains("x-goog-user-project: adc-quota")
    );
    assert_eq!(first_request.body["model"], "google/gemini-2.5-flash");
    assert_eq!(
        first_request.body["messages"][0]["content"],
        "system guidance"
    );
    assert_eq!(first_request.body["max_tokens"], 4096);
    assert_eq!(
        second_request.body["messages"][2]["tool_calls"][0]["function"]["arguments"],
        "{ \"key\" : \"value\" }"
    );
    assert_eq!(
        second_request.body["messages"][2]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
        "private-signature"
    );
    server.assert_no_more_requests();
}

#[tokio::test]
async fn cloud_errors_do_not_retry_or_reveal_provider_messages() {
    let sandbox = Sandbox::new(user());
    let session = Session::with_http(&profile(""), &sandbox.env, Arc::new(Fixture::new()))
        .await
        .unwrap();
    for (status, error, expected) in [
        (
            401,
            json!({"status":"UNAUTHENTICATED"}),
            "rejected authentication",
        ),
        (
            403,
            json!({"status":"PERMISSION_DENIED"}),
            "permission denied",
        ),
        (
            403,
            json!({"status":"PERMISSION_DENIED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED"}]}),
            "enable aiplatform",
        ),
        (
            403,
            json!({"details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"BILLING_DISABLED"}]}),
            "enable billing",
        ),
        (404, json!({"status":"NOT_FOUND"}), "model or resource"),
        (429, json!({"status":"RESOURCE_EXHAUSTED"}), "rate limited"),
        (
            503,
            json!({"status":"UNAVAILABLE"}),
            "temporarily unavailable",
        ),
    ] {
        let mut error = error;
        error["message"] = REFRESH.into();
        let mut response = Response::json(json!({"error":error}));
        response.status = status;
        let server = Server::start(vec![response]);
        let model = crate::llm::Model::new(profile(""), "google/gemini-2.5-flash".into(), None)
            .unwrap()
            .with_google_cloud_session(session.clone())
            .with_test_http(crate::http::InferenceHttp::test_routed(server.url.clone()));
        let error = model
            .step(
                &[crate::llm::Message::User("question".into())],
                &[],
                &mut |_| {},
            )
            .await
            .err()
            .unwrap();
        let rendered = format!("{error} {error:?}");
        assert!(rendered.contains(expected), "{rendered}");
        assert!(!rendered.contains(REFRESH));
        server.request();
        server.assert_no_more_requests();
    }
}

#[tokio::test]
async fn embedded_cloud_inference_requires_a_prepared_matching_session() {
    let model = crate::llm::Model::new(
        profile(""),
        "google/gemini-2.5-flash".into(),
        Some(Secret::new(ACCESS)),
    )
    .unwrap();
    assert!(matches!(
        model.step(&[], &[], &mut |_| {}).await,
        Err(crate::llm::Error::GoogleCloud(_))
    ));
    let providers = configured_providers("[providers.work]\npreset='google-cloud'");
    let error = Session::with_http(
        providers.get("work").unwrap(),
        &Environment::new(None, None, None),
        Arc::new(Fixture::new()),
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("google_cloud.project"));
}
