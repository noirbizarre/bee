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
