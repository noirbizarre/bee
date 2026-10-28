//! Copilot authentication, native model routes and tool continuations through ACP.

use super::*;
use crate::auth::{Audience, CredentialStore, FileStore};
use crate::http::{HttpResponse, LoginHttp, Unreachable};
use crate::provider::Protocol;
use crate::test_support::{Response, Server, event};
use agent_client_protocol::{Client, Responder};
use rstest::rstest;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct OAuth {
    pending: AtomicBool,
    polls: AtomicUsize,
    polled: tokio::sync::Notify,
}

impl OAuth {
    fn new(pending: bool) -> Self {
        Self {
            pending: AtomicBool::new(pending),
            polls: AtomicUsize::new(0),
            polled: tokio::sync::Notify::new(),
        }
    }
}

impl LoginHttp for OAuth {
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        body: String,
    ) -> BoxFuture<'a, std::result::Result<HttpResponse, Unreachable>> {
        Box::pin(async move {
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["client_id"], crate::auth::copilot::CLIENT_ID);
            let value = if url == "https://github.com/login/device/code" {
                json!({"verification_uri":"https://github.com/login/device","device_code":"device-secret","user_code":"ABCD-EFGH","expires_in":900,"interval":1})
            } else {
                assert_eq!(url, "https://github.com/login/oauth/access_token");
                assert_eq!(body["device_code"], "device-secret");
                self.polls.fetch_add(1, Ordering::SeqCst);
                self.polled.notify_one();
                if self.pending.load(Ordering::SeqCst) {
                    json!({"error":"authorization_pending"})
                } else {
                    json!({"access_token":"github-user-secret","token_type":"bearer"})
                }
            };
            Ok(HttpResponse {
                status: 200,
                body: value.to_string(),
            })
        })
    }

    fn get_authenticated<'a>(
        &'a self,
        url: &'a str,
        headers: reqwest::header::HeaderMap,
    ) -> BoxFuture<'a, std::result::Result<HttpResponse, Unreachable>> {
        assert_eq!(url, "https://api.github.com/copilot_internal/user");
        assert_eq!(headers["authorization"], "Bearer github-user-secret");
        Box::pin(async {
            Ok(HttpResponse { status: 200, body: json!({"chat_enabled":true,"endpoints":{"api":"https://api.business.githubcopilot.com"}}).to_string() })
        })
    }
}

struct Catalogue(Protocol, bool);
impl crate::models::Http for Catalogue {
    fn get<'a>(
        &'a self,
        url: &'a str,
        headers: reqwest::header::HeaderMap,
        _: usize,
    ) -> BoxFuture<'a, std::result::Result<Vec<u8>, crate::models::Error>> {
        assert_eq!(url, "https://api.business.githubcopilot.com/models");
        assert_eq!(headers["authorization"], "Bearer github-user-secret");
        assert!(!headers.contains_key("x-initiator"));
        if self.1 {
            // An explicitly routed turn must finish while the optional account catalogue never responds.
            return Box::pin(futures::future::pending());
        }
        let endpoint = match self.0 {
            Protocol::OpenAiChat => "/chat/completions",
            Protocol::OpenAiResponses => "/responses",
            Protocol::AnthropicMessages => "/v1/messages",
        };
        let bytes = json!({"data":[{"id":"native-model","name":"Account model","model_picker_enabled":true,"supported_endpoints":[endpoint],"capabilities":{"limits":{"max_context_window_tokens":32000},"supports":{"tool_calls":true,"streaming":true}}}]}).to_string().into_bytes();
        Box::pin(async move { Ok(bytes) })
    }
}

fn answer(protocol: Protocol, tools: bool) -> Response {
    match protocol {
        Protocol::OpenAiResponses => Response::sse(super::oauth_tests::stream(if tools {
            vec![
                json!({"id":"reasoning","type":"reasoning","summary":[],"encrypted_content":"opaque-reasoning"}),
                json!({"id":"call-item","type":"function_call","name":"read","call_id":"read-1","arguments":"{\"path\":\"payload.txt\"}"}),
            ]
        } else {
            vec![
                json!({"id":"answer","type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}),
            ]
        })),
        Protocol::OpenAiChat => {
            let delta = if tools {
                json!({"tool_calls":[{"index":0,"id":"read-1","type":"function","function":{"name":"read","arguments":"{\"path\":\"payload.txt\"}"}}]})
            } else {
                json!({"content":"done"})
            };
            Response::sse(
                event(
                    "",
                    json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
                ) + &event(
                    "",
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":if tools {"tool_calls"} else {"stop"}}]}),
                ) + "data: [DONE]\n\n",
            )
        }
        Protocol::AnthropicMessages => {
            let block = if tools {
                json!({"type":"tool_use","id":"read-1","name":"read","input":{}})
            } else {
                json!({"type":"text","text":""})
            };
            let delta = if tools {
                json!({"type":"input_json_delta","partial_json":"{\"path\":\"payload.txt\"}"})
            } else {
                json!({"type":"text_delta","text":"done"})
            };
            Response::sse(
                event(
                    "message_start",
                    json!({"type":"message_start","message":{"id":"msg","role":"assistant","type":"message","content":[]}}),
                ) + &event(
                    "content_block_start",
                    json!({"type":"content_block_start","index":0,"content_block":block}),
                ) + &event(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":0,"delta":delta}),
                ) + &event(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":0}),
                ) + &event(
                    "message_delta",
                    json!({"type":"message_delta","delta":{"stop_reason":if tools {"tool_use"} else {"end_turn"}}}),
                ) + &event("message_stop", json!({"type":"message_stop"})),
            )
        }
    }
}

async fn idle(
    updates: &mut tokio::sync::mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
    reason: v2::StopReason,
) {
    loop {
        let update = updates.recv().await.unwrap();
        let wire = serde_json::to_string(&update).unwrap();
        assert!(!wire.contains("github-user-secret"));
        assert!(!wire.contains("device-secret"));
        if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) = update.update {
            assert_eq!(idle.stop_reason, Some(reason));
            break;
        }
    }
}

#[rstest]
#[case(Protocol::OpenAiChat, false, true, false)]
#[case(Protocol::OpenAiResponses, false, true, false)]
#[case(Protocol::AnthropicMessages, false, true, false)]
#[case(Protocol::OpenAiResponses, true, true, false)]
#[case(Protocol::AnthropicMessages, false, false, false)]
#[case(Protocol::OpenAiChat, false, true, true)]
#[tokio::test]
async fn copilot_streams_native_tools_with_account_routes_and_correct_initiator_headers(
    #[case] protocol: Protocol,
    #[case] login: bool,
    #[case] discover: bool,
    #[case] stalled_catalogue: bool,
) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("payload.txt"), "fixture-file-content").unwrap();
    let path = root.path().join("config.toml");
    let credential = if login {
        ""
    } else {
        "credential={env='COPILOT_GITHUB_TOKEN'}"
    };
    let explicit = if discover && !stalled_catalogue {
        String::new()
    } else {
        format!(
            "discovery=false\nmodels=['native-model']\nmodel_protocols={{native-model='{}'}}",
            protocol.name()
        )
    };
    std::fs::write(
        &path,
        format!("model='account/native-model'\n[accounts.account]\nprovider='github'\n{credential}\n{explicit}"),
    )
    .unwrap();
    let env = crate::config::Environment::new(None, Some(root.path().into()), Some(path))
        .with_variable("COPILOT_GITHUB_TOKEN", "github-user-secret");
    let profiles =
        Providers::from_config(&crate::config::Config::load(&env, root.path()).unwrap()).unwrap();
    let store = FileStore::from_environment(&env).unwrap();
    let mut index = 0;
    let server = Server::respond(3, move |_| {
        index += 1;
        answer(protocol, index == 1)
    });
    let oauth = Arc::new(OAuth::new(false));
    let discovery = Arc::new(
        crate::models::Service::new(
            crate::models::Settings {
                enabled: false,
                ..Default::default()
            },
            Arc::new(Catalogue(protocol, stalled_catalogue)),
            Arc::new(crate::models::FileCache(None)),
            Arc::new(crate::models::SystemClock),
        )
        .with_login_http(oauth.clone()),
    );
    let mut runtime = Runtime::builtins(crate::agent::Limits::default())
        .unwrap()
        .with_auth(env, oauth.clone())
        .with_discovery(discovery)
        .with_history(crate::history::Store::open(root.path().join("state")).unwrap());
    runtime.inference_http = Some(crate::http::InferenceHttp::test_routed(server.url.clone()));
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let completions = Arc::new(AtomicUsize::new(0));
    let observed = completions.clone();
    let client = Client
        .v2()
        .on_receive_request(
            async move |request: v2::CreateElicitationRequest,
                        responder: Responder<v2::CreateElicitationResponse>,
                        _| {
                let value = serde_json::to_value(request).unwrap();
                assert_eq!(value["url"], "https://github.com/login/device");
                assert!(value.to_string().contains("ABCD-EFGH"));
                assert!(!value.to_string().contains("device-secret"));
                responder.respond(serde_json::from_value(json!({"action":"accept"})).unwrap())?;
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
            async move |_: v2::CompleteElicitationNotification, _| {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    tokio::time::timeout(std::time::Duration::from_secs(15), client.connect_with(agent_with_runtime(profiles, runtime), async |connection| {
        connection.send_request(serde_json::from_value::<v2::InitializeRequest>(json!({"protocolVersion":2,"info":{"name":"copilot-fixture","version":"0"},"capabilities":{"elicitation":{"url":{}}}})).unwrap()).block_task().await?;
        let session = connection.send_request(v2::NewSessionRequest::new(root.path())).block_task().await?.session_id;
        connection.send_request(v2::PromptRequest::new(session.clone(), vec!["read the payload".into()])).block_task().await?;
        idle(&mut updates, v2::StopReason::EndTurn).await;
        // Close/resume is a completion barrier and exercises native history replay before the next turn.
        connection.send_request(v2::CloseSessionRequest::new(session.clone())).block_task().await?;
        connection.send_request(v2::ResumeSessionRequest::new(session.clone(), root.path())).block_task().await?;
        while let Some(update) = updates.recv().await {
            if matches!(update.update, v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))) { break; }
        }
        connection.send_request(v2::PromptRequest::new(session, vec!["continue".into()])).block_task().await?;
        idle(&mut updates, v2::StopReason::EndTurn).await;
        Ok(())
    })).await.unwrap().unwrap();
    assert_eq!(completions.load(Ordering::SeqCst), usize::from(login));
    assert_eq!(
        store.get(Audience::Provider("account")).unwrap().is_some(),
        login
    );
    let first = server.request();
    let second = server.request();
    let third = server.request();
    let expected_path = match protocol {
        Protocol::OpenAiChat => "/chat/completions",
        Protocol::OpenAiResponses => "/responses",
        Protocol::AnthropicMessages => "/v1/messages",
    };
    for request in [&first, &second, &third] {
        assert!(
            request
                .head
                .starts_with(&format!("POST /prefix{expected_path} "))
        );
        assert!(
            request
                .head
                .contains("authorization: Bearer github-user-secret")
        );
        assert!(!request.head.contains("x-api-key:"));
        assert!(request.head.contains("x-github-api-version: 2026-08-01"));
        assert!(request.head.contains("user-agent: bee/"));
        assert!(
            request
                .head
                .contains("x-interaction-type: conversation-agent")
        );
        assert_eq!(request.body["model"], "native-model");
    }
    let interaction = |request: &crate::test_support::Request| {
        request
            .head
            .lines()
            .find(|line| line.starts_with("x-interaction-id:"))
            .unwrap()
            .to_owned()
    };
    assert_eq!(interaction(&first), interaction(&second));
    assert_eq!(interaction(&first), interaction(&third));
    assert!(first.head.contains("x-initiator: user"));
    assert!(second.head.contains("x-initiator: agent"));
    assert!(third.head.contains("x-initiator: user"));
    assert!(second.body.to_string().contains("fixture-file-content"));
    assert!(third.body.to_string().contains("fixture-file-content"));
    if protocol == Protocol::OpenAiResponses {
        assert_eq!(first.body["store"], false);
        assert_eq!(
            first.body["include"],
            json!(["reasoning.encrypted_content"])
        );
        assert_eq!(first.body["tools"][0]["type"], "function");
        assert!(first.body.get("max_output_tokens").is_some());
        assert!(second.body.to_string().contains("opaque-reasoning"));
        assert!(third.body.to_string().contains("opaque-reasoning"));
    }
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn cancelling_or_closing_a_copilot_device_login_stops_polling_before_completion(
    #[case] close: bool,
) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    std::fs::write(
        &path,
        "model='account/explicit-model'\n[accounts.account]\nprovider='github'\nmodels=['explicit-model']\ndiscovery=false",
    )
    .unwrap();
    let env = crate::config::Environment::new(None, Some(root.path().into()), Some(path));
    let profiles =
        Providers::from_config(&crate::config::Config::load(&env, root.path()).unwrap()).unwrap();
    let store = FileStore::from_environment(&env).unwrap();
    let oauth = Arc::new(OAuth::new(true));
    let runtime = Runtime::new(
        crate::agent::Limits::default(),
        crate::tool::Registry::default(),
    )
    .with_auth(env, oauth.clone());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let completed = Arc::new(AtomicUsize::new(0));
    let observed = completed.clone();
    let client = Client
        .v2()
        .on_receive_request(
            async move |_: v2::CreateElicitationRequest,
                        responder: Responder<v2::CreateElicitationResponse>,
                        _| {
                responder.respond(serde_json::from_value(json!({"action":"accept"})).unwrap())?;
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
            async move |_: v2::CompleteElicitationNotification, _| {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    tokio::time::timeout(std::time::Duration::from_secs(10), client.connect_with(agent_with_runtime(profiles, runtime), async |connection| {
        connection.send_request(serde_json::from_value::<v2::InitializeRequest>(json!({"protocolVersion":2,"info":{"name":"cancel-fixture","version":"0"},"capabilities":{"elicitation":{"url":{}}}})).unwrap()).block_task().await?;
        let session = connection.send_request(v2::NewSessionRequest::new(root.path())).block_task().await?.session_id;
        connection.send_request(v2::PromptRequest::new(session.clone(), vec!["hello".into()])).block_task().await?;
        oauth.polled.notified().await;
        if close { connection.send_request(v2::CloseSessionRequest::new(session)).block_task().await?; }
        else {
            connection.send_notification(v2::CancelSessionNotification::new(session))?;
            idle(&mut updates, v2::StopReason::Cancelled).await;
        }
        oauth.pending.store(false, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert_eq!(oauth.polls.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        assert!(store.get(Audience::Provider("account")).unwrap().is_none());
        Ok(())
    })).await.unwrap().unwrap();
}
