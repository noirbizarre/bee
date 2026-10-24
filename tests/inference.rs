//! Real ACP prompts using real local model HTTP requests.

mod support;

use std::time::Duration;

use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Client, Error};
use serde_json::{Value, json};
use support::{Response, Server, chat_text, providers};

fn answer(protocol: &str, text: &str) -> Value {
    match protocol {
        "openai-chat" => chat_text(text),
        "openai-responses" => {
            json!({"status":"completed","output":[{"id":"m1","type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}]})
        }
        "anthropic-messages" => {
            json!({"type":"message","role":"assistant","content":[{"type":"text","text":text}],"stop_reason":"end_turn"})
        }
        _ => unreachable!(),
    }
}

fn mixed_profiles(url: &str, preferred: &str) -> bee::provider::Providers {
    support::configured_providers(&format!(
        r#"
[providers.test]
model = "vendor/responses.v2"
prefer = ["{preferred}"]
max_output_tokens = 123
headers = {{ x-team = "shared" }}
model_protocols = {{ "vendor/chat.v2" = "openai-chat", "vendor/responses.v2" = "openai-responses", "vendor/messages.v2" = "anthropic-messages" }}
[providers.test.protocols.openai-chat]
base_url = "{url}/chat"
options = {{ max_tokens_field = "max_completion_tokens" }}
[providers.test.protocols.openai-responses]
base_url = "{url}/responses"
[providers.test.protocols.anthropic-messages]
base_url = "{url}/anthropic"
options = {{ version = "2023-06-01", max_tokens = 456 }}
"#
    ))
}

#[rstest::rstest]
#[case(
    "openai-chat",
    "vendor/chat.v2",
    "anthropic-messages",
    "/prefix/chat/chat/completions"
)]
#[case(
    "openai-responses",
    "vendor/responses.v2",
    "anthropic-messages",
    "/prefix/responses/responses"
)]
#[case(
    "anthropic-messages",
    "vendor/messages.v2",
    "openai-chat",
    "/prefix/anthropic/v1/messages"
)]
#[tokio::test]
async fn mapped_models_use_their_own_wire_protocol_url_options_and_shared_credentials(
    #[case] protocol: &str,
    #[case] model_id: &str,
    #[case] preferred: &str,
    #[case] path: &str,
) {
    let server = Server::start(vec![Response::json(answer(protocol, "answer"))]);
    let profiles = mixed_profiles(&server.url, preferred);
    let model = bee::llm::Model::new(
        profiles.get("test").unwrap().clone(),
        model_id.into(),
        Some(bee::auth::Secret::new("shared-key")),
    )
    .unwrap();
    let step = model
        .step(
            &[bee::llm::Message::User("question".into())],
            &[],
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(step.stop, bee::llm::Stop::End);
    let request = server.request();
    assert!(request.head.starts_with(&format!("POST {path} ")));
    assert_eq!(request.body["model"], model_id);
    assert!(request.head.contains("x-team: shared"));
    if protocol == "anthropic-messages" {
        assert!(request.head.contains("x-api-key: shared-key"));
        assert!(!request.head.contains("authorization:"));
        assert!(request.head.contains("anthropic-version: 2023-06-01"));
        assert_eq!(request.body["max_tokens"], 456);
    } else {
        assert!(request.head.contains("authorization: Bearer shared-key"));
        assert!(!request.head.contains("x-api-key:"));
        let limit = if protocol == "openai-chat" {
            "max_completion_tokens"
        } else {
            "max_output_tokens"
        };
        assert_eq!(request.body[limit], 123);
        assert!(request.body.get("max_tokens").is_none());
    }
}

#[tokio::test]
async fn an_explicitly_mapped_endpoint_failure_does_not_retry_the_preferred_protocol() {
    let mut response = Response::json(json!({"error":"unavailable"}));
    response.status = 503;
    // A live, successful fallback catches retries even if inference preserves
    // the original error after contacting the preferred endpoint.
    let server = Server::start(vec![
        response,
        Response::json(chat_text("unexpected fallback")),
    ]);
    let profiles = mixed_profiles(&server.url, "openai-chat");
    let model = bee::llm::Model::new(
        profiles.get("test").unwrap().clone(),
        "vendor/messages.v2".into(),
        None,
    )
    .unwrap();
    let error = model
        .step(
            &[bee::llm::Message::User("question".into())],
            &[],
            &mut |_| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(error, bee::llm::Error::Http(503)));
    assert!(
        server
            .request()
            .head
            .starts_with("POST /prefix/anthropic/v1/messages ")
    );
    server.assert_no_more_requests();
}

#[tokio::test]
async fn session_model_changes_resolve_protocols_again_and_drop_native_continuation() {
    let mut first = answer("openai-responses", "first answer");
    first["output"].as_array_mut().unwrap().insert(0, json!({"id":"reason-1","type":"reasoning","summary":[],"encrypted_content":"private-continuation"}));
    let server = Server::start(vec![
        Response::json(first),
        Response::json(answer("anthropic-messages", "second answer")),
        Response::json(answer("openai-responses", "third answer")),
    ]);
    let profiles = mixed_profiles(&server.url, "openai-responses");
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _connection| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_providers(profiles),
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
                for (index, model) in [None, Some("vendor/messages.v2"), Some("unknown/private.v3")]
                    .into_iter()
                    .enumerate()
                {
                    if let Some(model) = model {
                        connection
                            .send_request(v2::SetSessionConfigOptionRequest::new(
                                session.clone(),
                                "model",
                                model,
                            ))
                            .block_task()
                            .await?;
                    }
                    connection
                        .send_request(v2::PromptRequest::new(
                            session.clone(),
                            vec![format!("question {index}").into()],
                        ))
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
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let first = server.request();
    assert_eq!(first.body["model"], "vendor/responses.v2");
    assert!(first.head.starts_with("POST /prefix/responses/responses "));
    let second = server.request();
    assert_eq!(second.body["model"], "vendor/messages.v2");
    assert!(
        second
            .head
            .starts_with("POST /prefix/anthropic/v1/messages ")
    );
    assert_eq!(
        second.body["messages"][1]["content"][0]["text"],
        "first answer"
    );
    let third = server.request();
    assert_eq!(third.body["model"], "unknown/private.v3");
    assert!(third.head.starts_with("POST /prefix/responses/responses "));
    assert_eq!(third.body["input"].as_array().unwrap().len(), 5);
    assert_eq!(third.body["input"][3]["content"], "second answer");
    assert!(!third.body.to_string().contains("private-continuation"));
}

#[rstest::rstest]
#[case("openai-chat")]
#[case("openai-responses")]
#[case("anthropic-messages")]
#[tokio::test]
async fn successful_turns_reach_the_model_and_supply_context_to_the_next_prompt(
    #[case] protocol: &str,
) {
    let server = Server::start(vec![
        Response::json(answer(protocol, "first answer")),
        Response::json(answer(protocol, "second answer")),
    ]);
    let profiles = providers(protocol, &server.url, "");
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _connection| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_providers(profiles),
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
                for prompt in ["first question", "second question"] {
                    connection
                        .send_request(v2::PromptRequest::new(session.clone(), vec![prompt.into()]))
                        .block_task()
                        .await?;
                    loop {
                        let update = updates.recv().await.unwrap();
                        if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                            update.update
                        {
                            assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                            break;
                        }
                    }
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let key = if protocol == "openai-responses" {
        "input"
    } else {
        "messages"
    };
    assert_eq!(server.request().body[key].as_array().unwrap().len(), 1);
    let request = server.request();
    let previous = &request.body[key][1]["content"];
    let question = &request.body[key][2]["content"];
    if protocol == "openai-chat" {
        assert_eq!(previous, "first answer");
    } else {
        assert_eq!(previous[0]["text"], "first answer");
    }
    if protocol == "anthropic-messages" {
        assert_eq!(question[0]["text"], "second question");
    } else {
        assert_eq!(question, "second question");
    }
}

#[rstest::rstest]
#[case("openai-chat")]
#[case("openai-responses")]
#[case("anthropic-messages")]
#[tokio::test]
async fn cancelling_a_pending_http_request_leaves_no_context_or_late_output(
    #[case] protocol: &str,
) {
    let mut pending = Response::json(answer(protocol, "cancelled answer"));
    pending.delay = Duration::from_millis(100);
    let server = Server::start(vec![
        pending,
        Response::json(answer(protocol, "replacement answer")),
    ]);
    let profiles = providers(protocol, &server.url, "");
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _connection| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_providers(profiles),
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
                connection
                    .send_request(v2::PromptRequest::new(
                        session.clone(),
                        vec!["discard this".into()],
                    ))
                    .block_task()
                    .await?;
                // The server having the request is a barrier: cancellation interrupts
                // real pending I/O rather than winning before inference starts.
                server.request_async().await;
                connection
                    .send_notification(v2::CancelSessionNotification::new(session.clone()))?;
                loop {
                    let update = updates.recv().await.unwrap();
                    assert!(!matches!(
                        update.update,
                        v2::SessionUpdate::AgentMessageChunk(_)
                    ));
                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                        update.update
                    {
                        assert_eq!(idle.stop_reason, Some(v2::StopReason::Cancelled));
                        break;
                    }
                }
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["keep this".into()]))
                    .block_task()
                    .await?;
                let mut idle_count = 0;
                loop {
                    let update = updates.recv().await.unwrap();
                    assert!(!format!("{update:?}").contains("cancelled answer"));
                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                        update.update
                    {
                        assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                        idle_count += 1;
                        break;
                    }
                }
                assert_eq!(idle_count, 1);
                let request = server.request_async().await;
                let key = if protocol == "openai-responses" {
                    "input"
                } else {
                    "messages"
                };
                let content = if protocol == "anthropic-messages" {
                    json!([{"type":"text","text":"keep this"}])
                } else {
                    json!("keep this")
                };
                assert_eq!(
                    request.body[key],
                    json!([{"role":"user","content":content}])
                );
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}
