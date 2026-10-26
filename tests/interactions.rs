//! Production ACP user interactions, with deterministic providers and client policy.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v2;
use agent_client_protocol::{Agent, Client, Error, Responder, V2ConnectionTo};
use serde_json::{Value, json};
use support::{Response, Server};

fn tool_call(protocol: &str, name: &str, arguments: Value) -> Value {
    match protocol {
        "openai-chat" => {
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"call-1","type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]})
        }
        "openai-responses" => {
            json!({"status":"completed","output":[{"type":"function_call","id":"item-1","call_id":"call-1","name":name,"arguments":arguments.to_string()}]})
        }
        _ => {
            json!({"type":"message","role":"assistant","content":[{"type":"tool_use","id":"call-1","name":name,"input":arguments}],"stop_reason":"tool_use"})
        }
    }
}

fn answer(protocol: &str) -> Value {
    match protocol {
        "openai-chat" => support::chat_text("done"),
        "openai-responses" => {
            json!({"status":"completed","output":[{"type":"message","id":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]})
        }
        _ => {
            json!({"type":"message","role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"})
        }
    }
}

async fn initialize(connection: &V2ConnectionTo<Agent>, capabilities: Value) -> Result<(), Error> {
    let request: v2::InitializeRequest = serde_json::from_value(json!({
        "protocolVersion":2,"info":{"name":"interactions-test","version":"0"},"capabilities":capabilities,
    })).unwrap();
    connection.send_request(request).block_task().await?;
    Ok(())
}

async fn idle(
    updates: &mut tokio::sync::mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
) -> v2::IdleStateUpdate {
    loop {
        if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
            updates.recv().await.unwrap().update
        {
            return idle;
        }
    }
}

fn permission(outcome: &str) -> v2::RequestPermissionResponse {
    let value = match outcome {
        "cancel" => json!({"outcome":"cancelled"}),
        "unknown" => json!({"outcome":"future"}),
        option => json!({"outcome":"selected","optionId":option}),
    };
    serde_json::from_value(json!({"outcome":value})).unwrap()
}

#[rstest::rstest]
#[case("allow_once")]
#[case("reject_once")]
#[case("unoffered")]
#[case("cancel")]
#[case("unknown")]
#[case("error")]
#[case("invalid")]
#[tokio::test]
async fn only_an_offered_bash_approval_can_create_a_file(#[case] outcome: &'static str) {
    for protocol in ["openai-chat", "openai-responses", "anthropic-messages"] {
        let root = tempfile::tempdir().unwrap();
        let command = if outcome == "invalid" {
            "bad\0command"
        } else {
            "printf approved > sentinel"
        };
        let server = Server::start(vec![
            Response::json(tool_call(protocol, "bash", json!({"command":command}))),
            Response::json(answer(protocol)),
        ]);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let client = Client
            .v2()
            .on_receive_request(
                async move |request: v2::RequestPermissionRequest,
                            responder: Responder<v2::RequestPermissionResponse>,
                            _| {
                    observed
                        .lock()
                        .unwrap()
                        .push(serde_json::to_value(request).unwrap());
                    if outcome == "error" {
                        responder.respond_with_error(Error::internal_error().data("CLIENT-SECRET"))
                    } else {
                        responder.respond(permission(outcome))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _| {
                    tx.send(update).unwrap();
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            );
        tokio::time::timeout(
            Duration::from_secs(10),
            client.connect_with(
                bee::acp::agent_with_runtime(
                    support::providers(protocol, &server.url, ""),
                    bee::acp::Runtime::new(
                        bee::agent::Limits::default(),
                        bee::tool::builtins_with_bash(support::bash_settings()).unwrap(),
                    ),
                ),
                async |connection| {
                    initialize(&connection, json!({})).await?;
                    let session = connection
                        .send_request(v2::NewSessionRequest::new(root.path()))
                        .block_task()
                        .await?
                        .session_id;
                    connection
                        .send_request(v2::PromptRequest::new(
                            session.clone(),
                            vec!["execute".into()],
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(
                        idle(&mut updates).await.stop_reason,
                        Some(v2::StopReason::EndTurn)
                    );
                    assert_eq!(
                        root.path().join("sentinel").exists(),
                        outcome == "allow_once"
                    );
                    let requests = requests.lock().unwrap();
                    assert_eq!(requests.len(), usize::from(outcome != "invalid"));
                    if let Some(request) = requests.first() {
                        assert_eq!(request["sessionId"], session.to_string());
                        assert_eq!(request["subject"]["cwd"], root.path().to_str().unwrap());
                        assert_eq!(request["subject"]["toolCallId"], "tool-1-1");
                        assert_eq!(request["options"][0]["kind"], "allow_once");
                        assert_eq!(request["options"][1]["kind"], "reject_once");
                    }
                    Ok(())
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        server.request();
        let continuation = server.request().body.to_string();
        assert!(continuation.contains("call-1"));
        assert!(!continuation.contains("CLIENT-SECRET"));
        assert_eq!(
            continuation.contains("not approved"),
            outcome != "allow_once" && outcome != "invalid"
        );
    }
}

#[rstest::rstest]
#[case("accept")]
#[case("decline")]
#[case("cancel")]
#[case("future")]
#[case("missing")]
#[case("wrong")]
#[case("oversized")]
#[case("error")]
#[tokio::test]
async fn form_answers_are_validated_before_reaching_the_model(#[case] action: &'static str) {
    for protocol in ["openai-chat", "openai-responses", "anthropic-messages"] {
        let server = Server::start(vec![
            Response::json(tool_call(
                protocol,
                "ask_user",
                json!({"question":"Which color?"}),
            )),
            Response::json(answer(protocol)),
        ]);
        let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let client = Client
            .v2()
            .on_receive_request(
                async move |request: v2::CreateElicitationRequest,
                            responder: Responder<v2::CreateElicitationResponse>,
                            _| {
                    let request = serde_json::to_value(request).unwrap();
                    assert_eq!(request["mode"], "form");
                    assert_eq!(request["toolCallId"], "tool-1-1");
                    assert_eq!(request["requestedSchema"]["required"], json!(["answer"]));
                    if action == "error" {
                        return responder
                            .respond_with_error(Error::internal_error().data("CLIENT-SECRET"));
                    }
                    let response = match action {
                        "accept" => json!({"action":"accept","content":{"answer":"blue-choice"}}),
                        "missing" => json!({"action":"accept"}),
                        "wrong" => json!({"action":"accept","content":{"answer":42}}),
                        "oversized" => {
                            json!({"action":"accept","content":{"answer":"x".repeat(4097)}})
                        }
                        action => json!({"action":action}),
                    };
                    responder.respond(serde_json::from_value(response).unwrap())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _| {
                    tx.send(update).unwrap();
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            );
        tokio::time::timeout(
            Duration::from_secs(10),
            client.connect_with(
                bee::acp::agent_with_runtime(
                    support::providers(protocol, &server.url, ""),
                    bee::acp::Runtime::new(
                        bee::agent::Limits::default(),
                        bee::tool::Registry::default(),
                    ),
                ),
                async |connection| {
                    initialize(&connection, json!({"elicitation":{"form":{}}})).await?;
                    let session = connection
                        .send_request(v2::NewSessionRequest::new(std::env::temp_dir()))
                        .block_task()
                        .await?
                        .session_id;
                    connection
                        .send_request(v2::PromptRequest::new(session, vec!["ask".into()]))
                        .block_task()
                        .await?;
                    assert_eq!(
                        idle(&mut updates).await.stop_reason,
                        Some(v2::StopReason::EndTurn)
                    );
                    Ok(())
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let first = server.request().body.to_string();
        assert!(first.contains("ask_user"));
        let continuation = server.request().body.to_string();
        assert_eq!(continuation.contains("blue-choice"), action == "accept");
        assert!(!continuation.contains("CLIENT-SECRET"));
    }
}

#[rstest::rstest]
#[case(json!({}), false)]
#[case(json!({"elicitation":null}), false)]
#[case(json!({"elicitation":{}}), false)]
#[case(json!({"elicitation":{"url":{}}}), false)]
#[case(json!({"elicitation":{"form":{}}}), true)]
#[case(json!({"elicitation":{"form":{},"url":{}}}), true)]
#[tokio::test]
async fn form_tools_are_advertised_only_for_negotiated_form_support(
    #[case] capabilities: Value,
    #[case] form: bool,
) {
    let server = Server::start(vec![Response::json(support::chat_text("done"))]);
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).unwrap();
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_providers(support::providers("openai-chat", &server.url, "")),
            async |connection| {
                initialize(&connection, capabilities).await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["hello".into()]))
                    .block_task()
                    .await?;
                idle(&mut updates).await;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(server.request().body.to_string().contains("ask_user"), form);
}

struct Exchange {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    status: u16,
    gate: Option<Arc<tokio::sync::Notify>>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

impl bee::http::LoginHttp for Exchange {
    fn post_form<'a>(
        &'a self,
        url: &'a str,
        body: String,
    ) -> futures::future::BoxFuture<'a, Result<bee::http::HttpResponse, bee::http::Unreachable>>
    {
        Box::pin(async move {
            struct DropMarker(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for DropMarker {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _drop = DropMarker(self.dropped.clone());
            assert_eq!(url, "https://auth.openai.com/api/accounts/oauth/token");
            assert!(body.contains("grant_type=authorization_code"));
            assert!(body.contains("code=CALLBACK-SECRET"));
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            Err(bee::http::Unreachable)
        })
    }
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        body: String,
    ) -> futures::future::BoxFuture<'a, Result<bee::http::HttpResponse, bee::http::Unreachable>>
    {
        Box::pin(async move {
            struct DropMarker(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for DropMarker {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _drop = DropMarker(self.dropped.clone());
            assert_eq!(url, "https://openrouter.ai/api/v1/auth/keys");
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["code"], "CALLBACK-SECRET");
            assert!(!body["code_verifier"].as_str().unwrap().is_empty());
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            Ok(bee::http::HttpResponse {
                status: self.status,
                body: json!({"key":"STORED-SECRET"}).to_string(),
            })
        })
    }
}

#[cfg(unix)]
fn login_fixture(
    root: &std::path::Path,
    url: &str,
    extra: &str,
) -> (bee::provider::Providers, bee::config::Environment) {
    let path = root.join("config.toml");
    std::fs::write(&path, format!("[providers.test]\nmodel='test'\ncredential={{store=true,login='openrouter'}}\n{extra}\n[providers.test.protocols.openai-chat]\nbase_url='{url}'")).unwrap();
    let env = bee::config::Environment::new(None, Some(root.into()), Some(path));
    let config = bee::config::Config::load(&env, &root.join("no-project")).unwrap();
    (bee::provider::Providers::from_config(&config).unwrap(), env)
}

#[cfg(unix)]
fn callback(request: &Value) -> reqwest::Url {
    let url = reqwest::Url::parse(request["url"].as_str().unwrap()).unwrap();
    let params: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    let mut callback = reqwest::Url::parse(
        params
            .get("redirect_uri")
            .or_else(|| params.get("callback_url"))
            .unwrap(),
    )
    .unwrap();
    callback.set_host(Some("127.0.0.1")).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("state", &params["state"])
        .append_pair("code", "CALLBACK-SECRET");
    if params.contains_key("redirect_uri") {
        callback
            .query_pairs_mut()
            .append_pair("client_id", "issued-client");
    }
    callback
}

#[cfg(unix)]
#[rstest::rstest]
#[case("bad-state")]
#[case("no-code")]
#[case("denied")]
#[case("stateless-denial")]
#[case("store-failure")]
#[tokio::test]
async fn failed_external_login_does_not_report_completion_or_publish_a_credential(
    #[case] failure: &'static str,
) {
    let root = tempfile::tempdir().unwrap();
    let (profiles, env) = login_fixture(root.path(), "http://127.0.0.1:1", "");
    let store_path = env.credentials_path().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_auth(
        env,
        Arc::new(Exchange {
            calls: calls.clone(),
            status: 200,
            gate: None,
            dropped: Arc::default(),
        }),
    );
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let (elicited, mut elicitations) = tokio::sync::mpsc::unbounded_channel();
    let (complete, mut completions) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async move |request: v2::CreateElicitationRequest,
                        responder: Responder<v2::CreateElicitationResponse>,
                        _| {
                elicited
                    .send(serde_json::to_value(request).unwrap())
                    .unwrap();
                responder.respond(serde_json::from_value(json!({"action":"accept"})).unwrap())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |notification: v2::CompleteElicitationNotification, _| {
                complete.send(notification).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(profiles, runtime),
            async |connection| {
                initialize(&connection, json!({"elicitation":{"url":{}}})).await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["login".into()]))
                    .block_task()
                    .await?;
                let request = elicitations.recv().await.unwrap();
                let mut url = callback(&request);
                if failure == "store-failure" {
                    // A store becoming unusable during browser interaction cannot be replaced.
                    std::fs::create_dir_all(&store_path).unwrap();
                } else {
                    let state = url
                        .query_pairs()
                        .find(|(key, _)| key == "state")
                        .unwrap()
                        .1
                        .into_owned();
                    url.set_query(None);
                    if failure != "stateless-denial" {
                        url.query_pairs_mut().append_pair(
                            "state",
                            if failure == "bad-state" {
                                "wrong"
                            } else {
                                &state
                            },
                        );
                    }
                    if matches!(failure, "denied" | "stateless-denial") {
                        url.query_pairs_mut().append_pair("error", "access_denied");
                    }
                    if failure == "bad-state" {
                        url.query_pairs_mut().append_pair("code", "CALLBACK-SECRET");
                    }
                }
                reqwest::get(url).await.unwrap();
                assert!(matches!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::Error(_))
                ));
                assert!(completions.try_recv().is_err());
                assert!(!store_path.is_file());
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        usize::from(failure == "store-failure")
    );
}

#[cfg(unix)]
#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn concurrent_url_logins_keep_unique_ids_and_isolated_credential_commits(
    #[case] cancel_first: bool,
) {
    use bee::auth::CredentialStore;
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let server = Server::start(vec![
        Response::json(support::chat_text("done")),
        Response::json(support::chat_text("done")),
    ]);
    let (_, env) = login_fixture(root.path(), &server.url, "");
    let config_path = root.path().join("config.toml");
    let mut config = format!(
        "provider='test'\n{}",
        std::fs::read_to_string(&config_path).unwrap()
    );
    config.push_str(&format!("\n[providers.second]\nmodel='test'\ncredential={{store=true,login='openrouter'}}\nprotocols={{openai-chat={{base_url='{}'}}}}", server.url));
    std::fs::write(&config_path, config).unwrap();
    let profiles = bee::provider::Providers::from_config(
        &bee::config::Config::load(&env, &root.path().join("none")).unwrap(),
    )
    .unwrap();
    let store = bee::auth::FileStore::from_environment(&env).unwrap();
    store
        .put(
            bee::auth::Audience::Provider("unrelated"),
            &bee::auth::Credential::new(bee::auth::Secret::new("preserved")),
        )
        .unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_auth(
        env,
        Arc::new(Exchange {
            calls: calls.clone(),
            status: 200,
            gate: Some(gate.clone()),
            dropped: Arc::default(),
        }),
    );
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let (elicited, mut elicitations) = tokio::sync::mpsc::unbounded_channel();
    let (complete, mut completions) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async move |request: v2::CreateElicitationRequest,
                        responder: Responder<v2::CreateElicitationResponse>,
                        _| {
                elicited
                    .send(serde_json::to_value(request).unwrap())
                    .unwrap();
                responder.respond(serde_json::from_value(json!({"action":"accept"})).unwrap())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |notification: v2::CompleteElicitationNotification, _| {
                complete.send(notification).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(profiles, runtime),
            async |connection| {
                initialize(&connection, json!({"elicitation":{"url":{}}})).await?;
                let mut sessions = Vec::new();
                let mut ids = Vec::new();
                for (index, provider) in ["test", "second"].into_iter().enumerate() {
                    let session = connection
                        .send_request(v2::NewSessionRequest::new(root.path()))
                        .block_task()
                        .await?
                        .session_id;
                    connection
                        .send_request(v2::SetSessionConfigOptionRequest::new(
                            session.clone(),
                            "provider",
                            provider,
                        ))
                        .block_task()
                        .await?;
                    connection
                        .send_request(v2::PromptRequest::new(
                            session.clone(),
                            vec!["login".into()],
                        ))
                        .block_task()
                        .await?;
                    let request = elicitations.recv().await.unwrap();
                    assert_eq!(request["sessionId"], session.to_string());
                    ids.push(request["elicitationId"].as_str().unwrap().to_owned());
                    reqwest::get(callback(&request)).await.unwrap();
                    while calls.load(Ordering::SeqCst) <= index {
                        tokio::task::yield_now().await;
                    }
                    sessions.push(session);
                }
                assert_ne!(ids[0], ids[1]);
                if cancel_first {
                    connection.send_notification(v2::CancelSessionNotification::new(
                        sessions[0].clone(),
                    ))?;
                    assert_eq!(
                        idle(&mut updates).await.stop_reason,
                        Some(v2::StopReason::Cancelled)
                    );
                }
                gate.notify_waiters();
                for _ in 0..if cancel_first { 1 } else { 2 } {
                    assert_eq!(
                        idle(&mut updates).await.stop_reason,
                        Some(v2::StopReason::EndTurn)
                    );
                }
                let mut completed = Vec::new();
                while let Ok(notification) = completions.try_recv() {
                    completed.push(notification.elicitation_id.to_string());
                }
                completed.sort();
                let mut expected = if cancel_first {
                    vec![ids[1].clone()]
                } else {
                    ids
                };
                expected.sort();
                assert_eq!(completed, expected);
                assert_eq!(
                    store
                        .get(bee::auth::Audience::Provider("test"))
                        .unwrap()
                        .is_some(),
                    !cancel_first
                );
                assert!(
                    store
                        .get(bee::auth::Audience::Provider("second"))
                        .unwrap()
                        .is_some()
                );
                assert_eq!(
                    store
                        .get(bee::auth::Audience::Provider("unrelated"))
                        .unwrap()
                        .unwrap()
                        .secret()
                        .expose(),
                    "preserved"
                );
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

#[cfg(unix)]
#[rstest::rstest]
#[case("accept", 200, false)]
#[case("accept", 200, true)]
#[case("decline", 200, false)]
#[case("cancel", 200, false)]
#[case("future", 200, false)]
#[case("error", 200, false)]
#[case("accept", 403, false)]
#[tokio::test]
async fn url_acceptance_is_not_provider_login_completion(
    #[case] action: &'static str,
    #[case] status: u16,
    #[case] expired: bool,
) {
    use bee::auth::CredentialStore;
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let server = Server::start(vec![Response::json(support::chat_text("authenticated"))]);
    let (profiles, env) = login_fixture(root.path(), &server.url, "");
    let store = bee::auth::FileStore::from_environment(&env).unwrap();
    if expired {
        store
            .put(
                bee::auth::Audience::Provider("test"),
                &bee::auth::Credential::new(bee::auth::Secret::new("old")).expiring_at(1),
            )
            .unwrap();
    }
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_auth(
        env,
        Arc::new(Exchange {
            calls: calls.clone(),
            status,
            gate: None,
            dropped: Arc::default(),
        }),
    );
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let (elicited, mut elicitations) = tokio::sync::mpsc::unbounded_channel();
    let (complete, mut completions) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async move |request: v2::CreateElicitationRequest,
                        responder: Responder<v2::CreateElicitationResponse>,
                        _| {
                elicited
                    .send((serde_json::to_value(request).unwrap(), responder))
                    .unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |notification: v2::CompleteElicitationNotification, _| {
                complete.send(notification).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(profiles, runtime),
            async |connection| {
                initialize(&connection, json!({"elicitation":{"url":{}}})).await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(
                        session.clone(),
                        vec!["login".into()],
                    ))
                    .block_task()
                    .await?;
                let (request, responder) = elicitations.recv().await.unwrap();
                assert_eq!(request["mode"], "url");
                assert_eq!(request["sessionId"], session.to_string());
                assert!(request.get("toolCallId").is_none());
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                assert!(completions.try_recv().is_err());
                if action == "error" {
                    responder.respond_with_error(Error::internal_error().data("PEER-SECRET"))?;
                } else {
                    responder.respond(serde_json::from_value(json!({"action":action})).unwrap())?;
                }
                if action == "accept" {
                    // An ordered request proves acceptance was processed; it cannot complete login.
                    connection
                        .send_request(v2::ListSessionsRequest::new())
                        .block_task()
                        .await?;
                    assert_eq!(calls.load(Ordering::SeqCst), 0);
                    assert!(completions.try_recv().is_err());
                    let response = reqwest::get(callback(&request)).await.unwrap();
                    assert!(response.status().is_success());
                }
                let terminal = idle(&mut updates).await;
                let success = action == "accept" && status == 200;
                assert_eq!(
                    terminal.stop_reason == Some(v2::StopReason::EndTurn),
                    success
                );
                if success {
                    let notification = completions.recv().await.unwrap();
                    assert_eq!(
                        notification.elicitation_id.to_string(),
                        request["elicitationId"].as_str().unwrap()
                    );
                    assert_eq!(
                        store
                            .get(bee::auth::Audience::Provider("test"))
                            .unwrap()
                            .unwrap()
                            .secret()
                            .expose(),
                        "STORED-SECRET"
                    );
                } else {
                    assert!(completions.try_recv().is_err());
                    assert!(
                        store
                            .get(bee::auth::Audience::Provider("test"))
                            .unwrap()
                            .is_none()
                    );
                }
                let wire = serde_json::to_string(&terminal).unwrap();
                for secret in ["CALLBACK-SECRET", "STORED-SECRET", "PEER-SECRET"] {
                    assert!(!wire.contains(secret));
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        usize::from(action == "accept")
    );
    if action == "accept" && status == 200 {
        let request = server.request();
        assert!(request.head.contains("Bearer STORED-SECRET"));
        assert!(!request.body.to_string().contains("SECRET"));
    }
}

enum Pending {
    Permission(Responder<v2::RequestPermissionResponse>),
    Form(Responder<v2::CreateElicitationResponse>),
}

impl Pending {
    fn cancellation(&self) -> agent_client_protocol::RequestCancellation {
        match self {
            Self::Permission(r) => r.cancellation(),
            Self::Form(r) => r.cancellation(),
        }
    }
    fn accept(self) {
        // A response can race SDK cancellation; either result must be harmless.
        let _ = match self {
            Self::Permission(r) => r.respond(permission("allow_once")),
            Self::Form(r) => r.respond(
                serde_json::from_value(
                    json!({"action":"accept","content":{"answer":"late-answer"}}),
                )
                .unwrap(),
            ),
        };
    }
}

#[rstest::rstest]
#[case(false, false)]
#[case(false, true)]
#[case(true, false)]
#[case(true, true)]
#[tokio::test]
async fn stopping_pending_interactions_cleans_up_before_idle_and_isolates_sessions(
    #[case] form: bool,
    #[case] close: bool,
    #[values(false, true)] start: bool,
) {
    let root = tempfile::tempdir().unwrap();
    let other_root = tempfile::tempdir().unwrap();
    let name = if form { "ask_user" } else { "bash" };
    let arguments = if form {
        json!({"question":"answer?"})
    } else {
        json!({"command":"printf approved > sentinel"})
    };
    let server = Server::start(vec![
        Response::json(tool_call("openai-chat", name, arguments.clone())),
        Response::json(tool_call("openai-chat", name, arguments)),
        Response::json(support::chat_text("other")),
        Response::json(support::chat_text("replacement")),
    ]);
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let (pending, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let form_pending = pending.clone();
    let cancellation = Arc::new(Mutex::new(
        None::<agent_client_protocol::RequestCancellation>,
    ));
    let observed = cancellation.clone();
    let client = Client.v2().on_receive_request(
        async move |_: v2::RequestPermissionRequest, responder: Responder<v2::RequestPermissionResponse>, _| { pending.send(Pending::Permission(responder)).unwrap(); Ok(()) }, agent_client_protocol::on_receive_request!(),
    ).on_receive_request(
        async move |_: v2::CreateElicitationRequest, responder: Responder<v2::CreateElicitationResponse>, _| { form_pending.send(Pending::Form(responder)).unwrap(); Ok(()) }, agent_client_protocol::on_receive_request!(),
    ).on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            if matches!(&update.update, v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(i)) if i.stop_reason == Some(v2::StopReason::Cancelled)) {
                assert!(observed.lock().unwrap().as_ref().unwrap().is_cancelled());
            }
            tx.send(update).unwrap(); Ok(())
        }, agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &server.url, ""),
                bee::acp::Runtime::new(
                    bee::agent::Limits::default(),
                    bee::tool::builtins_with_bash(support::bash_settings()).unwrap(),
                ),
            ),
            async |connection| {
                initialize(&connection, json!({"elicitation":{"form":{}}})).await?;
                let first = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(
                        first.clone(),
                        vec!["discarded-question".into()],
                    ))
                    .block_task()
                    .await?;
                let held = requests.recv().await.unwrap();
                *cancellation.lock().unwrap() = Some(held.cancellation());
                // Drain initial events; resume must produce a requires_action snapshot.
                while updates.try_recv().is_ok() {}
                let mut request = v2::ResumeSessionRequest::new(first.clone(), root.path());
                if start {
                    request.replay_from = Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new()));
                }
                connection.send_request(request).block_task().await?;
                if start {
                    assert!(matches!(
                        updates.recv().await.unwrap().update,
                        v2::SessionUpdate::UserMessage(_)
                    ));
                }
                let mut snapshot = updates.recv().await.unwrap().update;
                if start {
                    assert!(matches!(snapshot, v2::SessionUpdate::ToolCallUpdate(_)));
                    snapshot = updates.recv().await.unwrap().update;
                }
                assert!(matches!(
                    snapshot,
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::RequiresAction(_))
                ));
                let other = connection
                    .send_request(v2::NewSessionRequest::new(other_root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(
                        other.clone(),
                        vec!["other-question".into()],
                    ))
                    .block_task()
                    .await?;
                let independent = requests.recv().await.unwrap();
                let independent_cancellation = independent.cancellation();
                if close {
                    connection
                        .send_request(v2::CloseSessionRequest::new(first.clone()))
                        .block_task()
                        .await?;
                } else {
                    connection
                        .send_notification(v2::CancelSessionNotification::new(first.clone()))?;
                }
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::Cancelled)
                );
                assert!(!independent_cancellation.is_cancelled());
                held.accept();
                independent.accept();
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::EndTurn)
                );
                assert!(!root.path().join("sentinel").exists());
                assert_eq!(other_root.path().join("sentinel").exists(), !form);
                if close {
                    connection
                        .send_request(v2::ResumeSessionRequest::new(first.clone(), root.path()))
                        .block_task()
                        .await?;
                    assert_eq!(idle(&mut updates).await.stop_reason, None);
                }
                connection
                    .send_request(v2::PromptRequest::new(
                        first,
                        vec!["replacement-question".into()],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::EndTurn)
                );
                assert!(updates.try_recv().is_err());
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    for _ in 0..3 {
        server.request();
    }
    let replacement = server.request().body.to_string();
    assert!(!replacement.contains("discarded-question"));
    assert!(!replacement.contains("late-answer"));
}

#[cfg(unix)]
#[rstest::rstest]
#[case::cancel_create("create", false, false)]
#[case::close_create("create", true, false)]
#[case::cancel_callback("callback", false, false)]
#[case::close_callback("callback", true, false)]
#[case::cancel_exchange("exchange", false, false)]
#[case::close_exchange("exchange", true, false)]
#[case::expire_create("create", false, true)]
#[case::expire_callback("callback", false, true)]
#[case::expire_exchange("exchange", false, true)]
#[case::chatgpt_cancel_create("chatgpt_create", false, false)]
#[case::chatgpt_close_callback("chatgpt_callback", true, false)]
#[case::chatgpt_cancel_exchange("chatgpt_exchange", false, false)]
#[case::chatgpt_close_exchange("chatgpt_exchange", true, false)]
#[case::chatgpt_expire_callback("chatgpt_callback", false, true)]
#[tokio::test]
async fn stopping_browser_login_drops_pending_work_before_terminal_idle(
    #[case] stage: &'static str,
    #[case] close: bool,
    #[case] expire: bool,
) {
    use std::sync::atomic::Ordering;
    let chatgpt = stage.starts_with("chatgpt_");
    let stage = stage.strip_prefix("chatgpt_").unwrap_or(stage);
    let root = tempfile::tempdir().unwrap();
    let (profiles, env) = login_fixture(root.path(), "http://127.0.0.1:1", "");
    let profiles = if chatgpt {
        support::configured_providers("[providers.test]\npreset='chatgpt'\nmodel='account-model'")
    } else {
        profiles
    };
    let store = env.credentials_path().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gate = Arc::new(tokio::sync::Notify::new());
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_auth(
        env,
        Arc::new(Exchange {
            calls: calls.clone(),
            status: 200,
            gate: Some(gate.clone()),
            dropped: dropped.clone(),
        }),
    );
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let (elicited, mut elicitations) = tokio::sync::mpsc::unbounded_channel();
    let cancellation = Arc::new(Mutex::new(
        None::<agent_client_protocol::RequestCancellation>,
    ));
    let observed = cancellation.clone();
    let observed_drop = dropped.clone();
    let first_terminal = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (complete, mut completions) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async move |request: v2::CreateElicitationRequest,
                        responder: Responder<v2::CreateElicitationResponse>,
                        _| {
                *cancellation.lock().unwrap() = Some(responder.cancellation());
                elicited
                    .send((serde_json::to_value(request).unwrap(), responder))
                    .unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                if matches!(
                    &update.update,
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                ) && first_terminal.swap(false, Ordering::SeqCst)
                {
                    if stage == "create" {
                        assert!(observed.lock().unwrap().as_ref().unwrap().is_cancelled());
                    }
                    if stage == "exchange" {
                        assert!(observed_drop.load(Ordering::SeqCst));
                    }
                }
                tx.send(update).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |notification: v2::CompleteElicitationNotification, _| {
                complete.send(notification).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    let exercise = client.connect_with(
        bee::acp::agent_with_runtime(profiles, runtime),
        async |connection| {
            initialize(&connection, json!({"elicitation":{"url":{}}})).await?;
            let session = connection
                .send_request(v2::NewSessionRequest::new(root.path()))
                .block_task()
                .await?
                .session_id;
            connection
                .send_request(v2::PromptRequest::new(
                    session.clone(),
                    vec!["login".into()],
                ))
                .block_task()
                .await?;
            let (request, responder) =
                tokio::time::timeout(Duration::from_secs(10), elicitations.recv())
                    .await
                    .unwrap()
                    .unwrap();
            let mut held = Some(responder);
            assert!(!request.to_string().contains("id_token"));
            assert!(!request.to_string().contains("STORED-SECRET"));
            if stage != "create" {
                held.take()
                    .unwrap()
                    .respond(serde_json::from_value(json!({"action":"accept"})).unwrap())?;
                connection
                    .send_request(v2::ListSessionsRequest::new())
                    .block_task()
                    .await?;
            }
            if stage == "exchange" {
                reqwest::get(callback(&request)).await.unwrap();
                while calls.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            }
            if expire {
                tokio::time::pause();
                tokio::time::advance(bee::login::DEFAULT_TIMEOUT + Duration::from_secs(1)).await;
                tokio::time::resume();
            } else if close {
                connection
                    .send_request(v2::CloseSessionRequest::new(session.clone()))
                    .block_task()
                    .await?;
            } else {
                connection
                    .send_notification(v2::CancelSessionNotification::new(session.clone()))?;
            }
            let terminal = tokio::time::timeout(Duration::from_secs(10), idle(&mut updates))
                .await
                .unwrap();
            if expire {
                assert!(matches!(
                    terminal.stop_reason,
                    Some(v2::StopReason::Error(_))
                ));
            } else {
                assert_eq!(terminal.stop_reason, Some(v2::StopReason::Cancelled));
            }
            if let Some(held) = held {
                let _ = held.respond(serde_json::from_value(json!({"action":"accept"})).unwrap());
            }
            gate.notify_waiters();
            // An ordered barrier catches duplicate terminal updates and late completions.
            connection
                .send_request(v2::ListSessionsRequest::new())
                .block_task()
                .await?;
            assert!(updates.try_recv().is_err());
            assert!(completions.try_recv().is_err());
            if chatgpt {
                use bee::auth::CredentialStore;
                assert!(
                    bee::auth::FileStore::new(&store)
                        .get(bee::auth::Audience::Provider("test"))
                        .unwrap()
                        .is_none()
                );
            } else {
                assert!(!store.exists());
            }
            assert!(reqwest::get(callback(&request)).await.is_err());
            if expire {
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["retry-login".into()]))
                    .block_task()
                    .await?;
                let (replacement, responder) =
                    tokio::time::timeout(Duration::from_secs(10), elicitations.recv())
                        .await
                        .unwrap()
                        .unwrap();
                assert_ne!(replacement["elicitationId"], request["elicitationId"]);
                responder.respond(serde_json::from_value(json!({"action":"decline"})).unwrap())?;
                assert!(matches!(
                    tokio::time::timeout(Duration::from_secs(10), idle(&mut updates))
                        .await
                        .unwrap()
                        .stop_reason,
                    Some(v2::StopReason::Error(_))
                ));
                assert!(completions.try_recv().is_err());
                if chatgpt {
                    use bee::auth::CredentialStore;
                    assert!(
                        bee::auth::FileStore::new(&store)
                            .get(bee::auth::Audience::Provider("test"))
                            .unwrap()
                            .is_none()
                    );
                } else {
                    assert!(!store.exists());
                }
            }
            Ok(())
        },
    );
    // A Tokio deadline enclosing the virtual-time jump would expire before the
    // workflow's own deadline could be observed. Bound its waits individually instead.
    if expire {
        exercise.await.unwrap();
    } else {
        tokio::time::timeout(Duration::from_secs(10), exercise)
            .await
            .unwrap()
            .unwrap();
    }
}

#[rstest::rstest]
#[case("unsupported")]
#[case("form-only")]
#[case("no-flow")]
#[case("env")]
#[case("malformed-store")]
#[tokio::test]
async fn unsupported_or_misconfigured_login_never_invokes_url_elicitation(
    #[case] kind: &'static str,
) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    let credential = match kind {
        "env" => "{env='ABSENT_KEY'}",
        "no-flow" => "{store=true}",
        _ => "{store=true,login='openrouter'}",
    };
    std::fs::write(&path, format!("[providers.test]\nmodel='test'\ncredential={credential}\nprotocols={{openai-chat={{base_url='http://127.0.0.1:1'}}}}")).unwrap();
    let env = bee::config::Environment::new(None, Some(root.path().into()), Some(path));
    let config = bee::config::Config::load(&env, &root.path().join("none")).unwrap();
    let profiles = bee::provider::Providers::from_config(&config).unwrap();
    if kind == "malformed-store" {
        let store = env.credentials_path().unwrap();
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(store, "STORE-SECRET").unwrap();
    }
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_auth(
        env,
        Arc::new(Exchange {
            calls: Arc::default(),
            status: 200,
            gate: None,
            dropped: Arc::default(),
        }),
    );
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async |_: v2::CreateElicitationRequest,
                   _: Responder<v2::CreateElicitationResponse>,
                   _| {
                panic!("unsupported URL login must not be invoked");
                #[allow(unreachable_code)]
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(profiles, runtime),
            async |connection| {
                let capabilities = match kind {
                    "unsupported" => json!({}),
                    "form-only" => json!({"elicitation":{"form":{}}}),
                    _ => json!({"elicitation":{"url":{}}}),
                };
                initialize(&connection, capabilities).await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["login".into()]))
                    .block_task()
                    .await?;
                let terminal = idle(&mut updates).await;
                assert!(matches!(
                    terminal.stop_reason,
                    Some(v2::StopReason::Error(_))
                ));
                assert!(
                    !serde_json::to_string(&terminal)
                        .unwrap()
                        .contains("STORE-SECRET")
                );
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn unanswered_interactions_time_out_without_authorizing_work(#[case] form: bool) {
    let root = tempfile::tempdir().unwrap();
    let name = if form { "ask_user" } else { "bash" };
    let arguments = if form {
        json!({"question":"answer?"})
    } else {
        json!({"command":"printf approved > sentinel"})
    };
    let server = Server::start(vec![
        Response::json(tool_call("openai-chat", name, arguments)),
        Response::json(support::chat_text("recovered")),
    ]);
    let (pending, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let form_pending = pending.clone();
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async move |_: v2::RequestPermissionRequest,
                        responder: Responder<v2::RequestPermissionResponse>,
                        _| {
                pending.send(Pending::Permission(responder)).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_: v2::CreateElicitationRequest,
                        responder: Responder<v2::CreateElicitationResponse>,
                        _| {
                form_pending.send(Pending::Form(responder)).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    client
        .connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &server.url, ""),
                bee::acp::Runtime::new(
                    bee::agent::Limits::default(),
                    bee::tool::builtins_with_bash(support::bash_settings()).unwrap(),
                ),
            ),
            async |connection| {
                initialize(&connection, json!({"elicitation":{"form":{}}})).await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["wait".into()]))
                    .block_task()
                    .await?;
                let held = tokio::time::timeout(Duration::from_secs(10), requests.recv())
                    .await
                    .unwrap()
                    .unwrap();
                tokio::time::pause();
                tokio::time::advance(bee::login::DEFAULT_TIMEOUT + Duration::from_secs(1)).await;
                tokio::time::resume();
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(10), idle(&mut updates))
                        .await
                        .unwrap()
                        .stop_reason,
                    Some(v2::StopReason::EndTurn)
                );
                assert!(held.cancellation().is_cancelled());
                held.accept();
                connection
                    .send_request(v2::ListSessionsRequest::new())
                    .block_task()
                    .await?;
                assert!(!root.path().join("sentinel").exists());
                Ok(())
            },
        )
        .await
        .unwrap();
}
