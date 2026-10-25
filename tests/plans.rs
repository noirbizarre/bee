//! Standard ACP plan consumption, through the production inference/execution path.

mod support;

use std::time::Duration;

use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Client, Error};
use serde_json::{Value, json};
use support::{Response, Server};

struct PanicTool;

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Empty {}

impl bee::tool::Tool for PanicTool {
    type Input = Empty;
    type Output = Value;

    fn name(&self) -> &str {
        "panic_tool"
    }
    fn description(&self) -> &str {
        "Exercise terminal panic cleanup"
    }

    async fn execute(&self, _: Empty, _: &bee::tool::Context) -> Result<Value, bee::tool::Failure> {
        panic!("injected tool panic")
    }
}

fn entry(content: &str, status: &str) -> Value {
    json!({"content":content,"priority":"medium","status":status})
}

fn call(protocol: &str, id: usize, entries: Value) -> Value {
    let arguments = json!({"entries":entries});
    match protocol {
        "openai-chat" => {
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":format!("call-{id}"),"type":"function","function":{"name":"update_plan","arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]})
        }
        "openai-responses" => {
            json!({"status":"completed","output":[{"type":"function_call","id":format!("item-{id}"),"call_id":format!("call-{id}"),"name":"update_plan","arguments":arguments.to_string()}]})
        }
        _ => {
            json!({"type":"message","role":"assistant","content":[{"type":"tool_use","id":format!("call-{id}"),"name":"update_plan","input":arguments}],"stop_reason":"tool_use"})
        }
    }
}

fn answer(protocol: &str) -> Value {
    match protocol {
        "openai-chat" => support::chat_text("done"),
        "openai-responses" => {
            json!({"status":"completed","output":[{"type":"message","id":"answer","role":"assistant","content":[{"type":"output_text","text":"done"}]}]})
        }
        _ => {
            json!({"type":"message","role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"})
        }
    }
}

async fn collect(
    updates: &mut tokio::sync::mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
) -> (Vec<Value>, v2::IdleStateUpdate) {
    let mut plans = Vec::new();
    loop {
        match updates.recv().await.unwrap().update {
            v2::SessionUpdate::PlanUpdate(update) => {
                plans.push(serde_json::to_value(update).unwrap());
            }
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => return (plans, idle),
            _ => {}
        }
    }
}

#[rstest::rstest]
#[case("openai-chat", false)]
#[case("openai-responses", false)]
#[case("anthropic-messages", false)]
#[case("openai-chat", true)]
#[case("openai-responses", true)]
#[case("anthropic-messages", true)]
#[tokio::test]
async fn plans_replace_ordered_snapshots_and_replay_independently_of_model_context(
    #[case] protocol: &str,
    #[case] durable: bool,
) {
    let root = tempfile::tempdir().unwrap();
    let first = json!([entry("Inspect", "in_progress"), entry("Verify", "pending")]);
    let revised = json!([
        entry("Verify carefully", "in_progress"),
        entry("Inspected", "completed")
    ]);
    let terminal = json!([
        entry("Verify carefully", "cancelled"),
        entry("Inspected", "completed")
    ]);
    let server = Server::start(vec![
        Response::json(call(protocol, 1, first.clone())),
        Response::json(call(protocol, 2, first.clone())),
        Response::json(call(protocol, 3, revised.clone())),
        Response::json(answer(protocol)),
        Response::json(call(protocol, 4, json!([entry("Temporary", "pending")]))),
        Response::json(call(protocol, 5, json!([]))),
        Response::json(answer(protocol)),
    ]);
    let mut runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    );
    if durable {
        runtime =
            runtime.with_history(bee::history::Store::open(root.path().join("state")).unwrap());
    }
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(support::providers(protocol, &server.url, ""), runtime),
            async |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("standard-plan-client", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session.clone(), vec!["work".into()]))
                    .block_task()
                    .await?;
                let (plans, idle) = collect(&mut updates).await;
                assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                assert_eq!(
                    plans,
                    vec![
                        json!({"plan":{"type":"items","planId":"plan-1","entries":first}}),
                        json!({"plan":{"type":"items","planId":"plan-1","entries":revised}}),
                        json!({"plan":{"type":"items","planId":"plan-1","entries":terminal}}),
                    ]
                );
                connection
                    .send_request(v2::PromptRequest::new(session.clone(), vec!["next".into()]))
                    .block_task()
                    .await?;
                let (second, _) = collect(&mut updates).await;
                assert_eq!(
                    second,
                    vec![
                        json!({"plan":{"type":"items","planId":"plan-2","entries":[entry("Temporary", "pending")]}}),
                        json!({"plan":{"type":"items","planId":"plan-2","entries":[]}}),
                    ]
                );
                // Close and reattach exercises disk projection for durable sessions,
                // and the isolated retained-state path for embedded sessions.
                connection
                    .send_request(v2::CloseSessionRequest::new(session.clone()))
                    .block_task()
                    .await?;
                let mut resume = v2::ResumeSessionRequest::new(session, root.path());
                resume.replay_from = Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new()));
                connection.send_request(resume).block_task().await?;
                let (replayed, idle) = collect(&mut updates).await;
                assert_eq!(idle.stop_reason, None);
                assert_eq!(replayed, vec![plans[2].clone(), second[1].clone()]);
                assert!(updates.try_recv().is_err(), "nothing may arrive after idle");
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    for index in 0..7 {
        let request = server.request();
        assert!(request.body.to_string().contains("update_plan"));
        if index > 0 && index != 4 {
            assert!(request.body.to_string().contains("updated"));
        }
    }
}

#[rstest::rstest]
#[case("normal")]
#[case("error")]
#[case("refusal")]
#[case("max_tokens")]
#[case("cancel")]
#[case("close")]
#[case("limit")]
#[case("panic")]
#[tokio::test]
async fn terminal_stops_cancel_only_unfinished_entries_before_idle(#[case] stop: &str) {
    let root = tempfile::tempdir().unwrap();
    let initial = json!([
        entry("Done", "completed"),
        entry("Working", "in_progress"),
        entry("Later", "pending")
    ]);
    let mut terminal = match stop {
        "error" => {
            let mut response = Response::json(json!({"error":"failure"}));
            response.status = 500;
            response
        }
        "refusal" => Response::json(
            json!({"choices":[{"index":0,"message":{"refusal":"declined"},"finish_reason":"stop"}]}),
        ),
        "max_tokens" => Response::json(
            json!({"choices":[{"index":0,"message":{"content":"partial"},"finish_reason":"length"}]}),
        ),
        "limit" => Response::json(call("openai-chat", 2, initial.clone())),
        "panic" => {
            let mut value = call("openai-chat", 2, json!([]));
            value["choices"][0]["message"]["tool_calls"][0]["function"]["name"] =
                "panic_tool".into();
            value["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] = "{}".into();
            Response::json(value)
        }
        _ => Response::json(answer("openai-chat")),
    };
    if matches!(stop, "cancel" | "close") {
        terminal.delay = Duration::from_secs(1);
    }
    let server = Server::start(vec![
        Response::json(call("openai-chat", 1, initial)),
        terminal,
    ]);
    let limits = bee::agent::Limits::new(if stop == "limit" { 2 } else { 3 }, 64).unwrap();
    let mut registry = bee::tool::Registry::default();
    registry.register(PanicTool).unwrap();
    let runtime = bee::acp::Runtime::new(limits, registry)
        .with_history(bee::history::Store::open(root.path().join("state")).unwrap());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &server.url, ""),
                runtime,
            ),
            async |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session.clone(), vec!["work".into()]))
                    .block_task()
                    .await?;
                server.request_async().await;
                server.request_async().await;
                if stop == "cancel" {
                    connection
                        .send_notification(v2::CancelSessionNotification::new(session.clone()))?;
                } else if stop == "close" {
                    connection
                        .send_request(v2::CloseSessionRequest::new(session.clone()))
                        .block_task()
                        .await?;
                }
                let (plans, idle) = collect(&mut updates).await;
                assert_eq!(plans.len(), 2);
                assert_eq!(
                    plans[1]["plan"]["entries"],
                    json!([
                        entry("Done", "completed"),
                        entry("Working", "cancelled"),
                        entry("Later", "cancelled")
                    ])
                );
                assert!(match stop {
                    "error" | "limit" | "panic" =>
                        matches!(idle.stop_reason, Some(v2::StopReason::Error(_))),
                    "refusal" => idle.stop_reason == Some(v2::StopReason::Refusal),
                    "max_tokens" => idle.stop_reason == Some(v2::StopReason::MaxTokens),
                    "cancel" | "close" => idle.stop_reason == Some(v2::StopReason::Cancelled),
                    _ => idle.stop_reason == Some(v2::StopReason::EndTurn),
                });
                let mut resume = v2::ResumeSessionRequest::new(session, root.path());
                resume.replay_from = Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new()));
                connection.send_request(resume).block_task().await?;
                let (replay, _) = collect(&mut updates).await;
                assert_eq!(replay, vec![plans[1].clone()]);
                assert!(updates.try_recv().is_err());
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

#[rstest::rstest]
#[case(json!(null))]
#[case(json!([{"content":" ","priority":"medium","status":"pending"}]))]
#[case(json!([{"content":"x","priority":"medium","status":"failed"}]))]
#[case(json!([{"content":"x","priority":"medium","status":"pending","id":"invented"}]))]
#[case(json!([{"content":"x","status":"pending"}]))]
#[tokio::test]
async fn malformed_plans_do_not_publish_or_mutate_state(#[case] invalid: Value) {
    let server = Server::start(vec![
        Response::json(call("openai-chat", 1, invalid)),
        Response::json(answer("openai-chat")),
    ]);
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &server.url, ""),
                bee::acp::Runtime::new(
                    bee::agent::Limits::default(),
                    bee::tool::Registry::default(),
                ),
            ),
            async |connection| {
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
                    .send_request(v2::PromptRequest::new(session, vec!["work".into()]))
                    .block_task()
                    .await?;
                let (plans, idle) = collect(&mut updates).await;
                assert!(plans.is_empty());
                assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    server.request();
    assert!(
        server
            .request()
            .body
            .to_string()
            .contains("invalid_arguments")
    );
}
