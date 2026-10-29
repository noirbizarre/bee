//! OVH's real account gate and native adapters, exercised without remote credentials.

use super::*;
use crate::llm::{Message, Model, Stop};
use crate::test_support::{Response, Server, configured_providers, event, tool};
use serde_json::{Value, json};

fn chat(text: &str) -> Response {
    Response::sse(format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}]})
    ))
}

fn responses(item: Value) -> Response {
    // Use the same item lifecycle for calls and messages to verify native replay.
    let mut wire = event(
        "response.created",
        json!({"type":"response.created","response":{"id":"response-1","status":"in_progress"}}),
    );
    wire += &event(
        "response.output_item.added",
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":item["id"],"type":item["type"]}}),
    );
    if item["type"] == "function_call" {
        wire += &event(
            "response.function_call_arguments.delta",
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":item["id"],"delta":item["arguments"]}),
        );
        wire += &event(
            "response.function_call_arguments.done",
            json!({"type":"response.function_call_arguments.done","output_index":0,"item_id":item["id"],"arguments":item["arguments"]}),
        );
    }
    wire += &event(
        "response.output_item.done",
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
    );
    wire += &event(
        "response.completed",
        json!({"type":"response.completed","response":{"id":"response-1","status":"completed","output":[item]}}),
    );
    Response::sse(wire)
}

fn answer() -> Value {
    json!({"id":"message-1","type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]})
}

fn model(protocol: &str, server: &Server) -> Model {
    let accounts = configured_providers(&format!(
        "[accounts.work]\nprovider='ovh'\nprotocol='{protocol}'\nmodels=['gpt-oss-20b']\ncontext_window=32000"
    ));
    Model::new(
        accounts.get("work").unwrap().clone(),
        "gpt-oss-20b".into(),
        Some(crate::auth::Secret::new("private-work-token")),
    )
    .unwrap()
    .with_test_http(crate::http::InferenceHttp::test_routed(server.url.clone()))
}

#[tokio::test]
async fn ovh_tools_and_retained_history_use_stateless_native_requests_on_both_protocols() {
    for protocol in ["openai-chat", "openai-responses"] {
        let first = if protocol == "openai-chat" {
            Response::sse(format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"echo","arguments":"{\"text\":\"payload\"}"}}]},"finish_reason":"tool_calls"}]})
            ))
        } else {
            responses(
                json!({"id":"item-1","type":"function_call","call_id":"call-1","name":"echo","arguments":"{\"text\":\"payload\"}"}),
            )
        };
        let finish = || {
            if protocol == "openai-chat" {
                chat("done")
            } else {
                responses(answer())
            }
        };
        let server = Server::start(vec![first, finish(), finish()]);
        let model = model(protocol, &server);
        let mut messages = vec![Message::User("echo payload".into())];
        let step = model.step(&messages, &[tool()], &mut |_| {}).await.unwrap();
        assert_eq!(step.stop, Stop::Tools);
        messages.push(step.message);
        messages.push(Message::Tool {
            id: "call-1".into(),
            output: "payload".into(),
            is_error: false,
        });
        let mut text = String::new();
        let step = model
            .step(&messages, &[tool()], &mut |part| text.push_str(part))
            .await
            .unwrap();
        assert_eq!(step.stop, Stop::End);
        assert_eq!(text, "done");
        messages.push(step.message);
        messages.push(Message::User("remember the payload".into()));
        model.step(&messages, &[tool()], &mut |_| {}).await.unwrap();
        for index in 0..3 {
            let request = server.request();
            let suffix = if protocol == "openai-chat" {
                "chat/completions"
            } else {
                "responses"
            };
            assert!(
                request
                    .head
                    .starts_with(&format!("POST /prefix/v1/{suffix} "))
            );
            assert!(
                request
                    .head
                    .contains("authorization: Bearer private-work-token")
            );
            assert_eq!(request.body["model"], "gpt-oss-20b");
            assert_eq!(request.body["stream"], true);
            assert!(!request.body.to_string().contains("private-work-token"));
            assert_eq!(request.body["tools"][0]["type"], "function");
            let history = if protocol == "openai-chat" {
                &request.body["messages"]
            } else {
                assert_eq!(request.body["store"], false);
                assert!(request.body.get("previous_response_id").is_none());
                assert!(request.body.get("include").is_none());
                &request.body["input"]
            };
            assert_eq!(history[0]["content"], "echo payload");
            if index > 0 {
                let call_id = if protocol == "openai-chat" {
                    "tool_call_id"
                } else {
                    "call_id"
                };
                assert_eq!(history[2][call_id], "call-1");
                assert!(history[2].to_string().contains("payload"));
            }
            if index == 2 {
                assert!(history.to_string().contains("remember the payload"));
                assert!(history.to_string().contains("done"));
            }
        }
        server.assert_no_more_requests();
    }
}

#[tokio::test]
async fn ovh_status_failures_are_sanitized_without_protocol_or_account_fallback() {
    for protocol in ["openai-chat", "openai-responses"] {
        for (status, expected) in [
            (401, crate::llm::Error::Authentication),
            (403, crate::llm::Error::Authentication),
            (429, crate::llm::Error::RateLimit),
        ] {
            let mut response = Response::json(json!({"error":{"message":"private-work-token"}}));
            response.status = status;
            let server = Server::start(vec![response]);
            let error = model(protocol, &server)
                .step(&[Message::User("hello".into())], &[], &mut |_| {})
                .await
                .unwrap_err();
            assert_eq!(error, expected);
            assert!(!format!("{error:?} {error}").contains("private-work-token"));
            server.request();
            server.assert_no_more_requests();
        }
    }
}

#[tokio::test]
async fn ovh_acp_uses_only_the_selected_accounts_credentials_and_declared_model() {
    for (protocol, response) in [
        ("openai-chat", chat("done")),
        ("openai-responses", responses(answer())),
    ] {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(&config, format!("model='work/gpt-oss-20b'\n[accounts.work]\nprovider='ovh'\nprotocol='{protocol}'\nmodels=['gpt-oss-20b']\ncontext_window=32000\n[accounts.personal]\nprovider='ovh'\ncredential={{env='PERSONAL_OVH_TOKEN'}}\nmodels=['personal-only']")).unwrap();
        let env = crate::config::Environment::new(None, Some(root.path().into()), Some(config))
            .with_variable("OVH_AI_ENDPOINTS_ACCESS_TOKEN", "private-work-token")
            .with_variable("PERSONAL_OVH_TOKEN", "private-personal-token");
        let accounts =
            Providers::from_config(&crate::config::Config::load(&env, root.path()).unwrap())
                .unwrap();
        let server = Server::start(vec![response]);
        let mut runtime = Runtime::builtins(crate::agent::Limits::default())
            .unwrap()
            .with_auth(env, Arc::new(crate::http::AsyncLoginHttp));
        runtime.inference_http = Some(crate::http::InferenceHttp::test_routed(server.url.clone()));
        let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let client = Client.v2().on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            client.connect_with(agent_with_runtime(accounts, runtime), async |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        2.into(),
                        v2::Implementation::new("ovh-test", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["hello".into()]))
                    .block_task()
                    .await?;
                loop {
                    let update = updates.recv().await.unwrap();
                    let wire = serde_json::to_string(&update).unwrap();
                    assert!(!wire.contains("private-work-token"));
                    assert!(!wire.contains("private-personal-token"));
                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                        update.update
                    {
                        assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                        break;
                    }
                }
                Ok(())
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let request = server.request();
        assert!(
            request
                .head
                .contains("authorization: Bearer private-work-token")
        );
        assert!(!request.head.contains("private-personal-token"));
        assert_eq!(request.body["model"], "gpt-oss-20b");
        server.assert_no_more_requests();
    }
}
