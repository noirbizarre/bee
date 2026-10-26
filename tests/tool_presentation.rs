//! Standard tool and terminal presentation through production inference and ACP serialization.

mod support;

use agent_client_protocol::{
    Client, Error,
    schema::{ProtocolVersion, v2},
};
use serde_json::{Value, json};
use std::time::Duration;
use support::{Response, Server};

struct BreakHistory;

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Empty {}

impl bee::tool::Tool for BreakHistory {
    type Input = Empty;
    type Output = Value;
    fn name(&self) -> &str {
        "break_history"
    }
    fn description(&self) -> &str {
        "Inject a deterministic storage failure after dispatch"
    }
    async fn execute(
        &self,
        _: Empty,
        context: &bee::tool::Context,
    ) -> Result<Value, bee::tool::Failure> {
        use std::io::Write;
        let session = std::fs::read_dir(context.cwd().join("state/sessions"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        // Change the owned journal length after the started update has been committed.
        std::fs::OpenOptions::new()
            .append(true)
            .open(session.join("session.jsonl"))
            .unwrap()
            .write_all(b"damaged-tail")
            .unwrap();
        Ok(json!({"uncommitted":"must-not-be-published"}))
    }
}

fn call(protocol: &str, id: &str, name: &str, arguments: Value) -> Value {
    match protocol {
        "openai-chat" => {
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]})
        }
        "openai-responses" => {
            json!({"status":"completed","output":[{"type":"function_call","id":format!("item-{id}"),"call_id":id,"name":name,"arguments":arguments.to_string()}]})
        }
        _ => {
            json!({"type":"message","role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":arguments}],"stop_reason":"tool_use"})
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
) -> Vec<Value> {
    let mut result = Vec::new();
    loop {
        let value = serde_json::to_value(updates.recv().await.unwrap().update).unwrap();
        let idle = value["state"] == "idle";
        result.push(value);
        if idle {
            return result;
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
async fn committed_file_and_directory_changes_have_correlated_lifecycles_and_idempotent_replay(
    #[case] protocol: &str,
    #[case] durable: bool,
) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("tree")).unwrap();
    std::fs::create_dir(root.path().join("tree/empty")).unwrap();
    std::fs::write(root.path().join("tree/binary"), [0, 255, 42]).unwrap();
    let operations = [
        ("write", json!({"path":"a.txt","content":"before\n"})),
        (
            "edit",
            json!({"path":"a.txt","old_text":"before","new_text":"after"}),
        ),
        ("copy", json!({"source":"a.txt","destination":"copy.txt"})),
        (
            "move",
            json!({"source":"copy.txt","destination":"moved.txt"}),
        ),
        ("delete", json!({"path":"moved.txt"})),
        (
            "copy",
            json!({"source":"tree","destination":"copied","recursive":true}),
        ),
        ("move", json!({"source":"copied","destination":"renamed"})),
        ("delete", json!({"path":"renamed","recursive":true})),
        ("read", json!({"path":42})),
    ];
    let mut responses = operations
        .iter()
        .enumerate()
        .map(|(i, (name, args))| {
            Response::json(call(protocol, &format!("call-{i}"), name, args.clone()))
        })
        .collect::<Vec<_>>();
    responses.push(Response::json(answer(protocol)));
    // Reusing a provider ID in a subsequent turn must not overwrite an earlier card.
    responses.push(Response::json(call(
        protocol,
        "call-0",
        "read",
        json!({"path":"a.txt"}),
    )));
    responses.push(Response::json(answer(protocol)));
    let server = Server::start(responses);
    let mut runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::builtins_with_bash(support::bash_settings()).unwrap(),
    );
    if durable {
        runtime =
            runtime.with_history(bee::history::Store::open(root.path().join("state")).unwrap());
    }
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            send.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(30),
        client.connect_with(
            bee::acp::agent_with_runtime(support::providers(protocol, &server.url, ""), runtime),
            async |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("tool-client", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(
                        session.clone(),
                        vec!["mutate".into()],
                    ))
                    .block_task()
                    .await?;
                let live = collect(&mut updates).await;
                assert_eq!(live.last().unwrap()["stopReason"], "end_turn");
                for (index, (name, _)) in operations.iter().enumerate() {
                    let id = format!("tool-1-{}", index + 1);
                    let states = live
                        .iter()
                        .filter(|u| {
                            u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == id
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(states.len(), 3);
                    assert_eq!(states[0]["name"], *name);
                    assert_eq!(states[0]["status"], "pending");
                    assert_eq!(states[1]["status"], "in_progress");
                    assert_eq!(
                        states[2]["status"],
                        if index == operations.len() - 1 {
                            "failed"
                        } else {
                            "completed"
                        },
                        "{name}: {}",
                        states[2]
                    );
                    assert!(states[2].get("rawOutput").is_some());
                }
                let diffs = live
                    .iter()
                    .filter(|u| u["content"]["type"] == "diff")
                    .collect::<Vec<_>>();
                for operation in ["add", "modify", "copy", "move", "delete"] {
                    assert!(
                        diffs.iter().any(|d| d["content"]["changes"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|c| c["operation"] == operation)),
                        "{operation}"
                    );
                }
                for diff in &diffs {
                    for change in diff["content"]["changes"].as_array().unwrap() {
                        assert!(
                            std::path::Path::new(change["path"].as_str().unwrap()).is_absolute()
                        );
                    }
                }
                assert!(
                    diffs
                        .iter()
                        .any(|d| d["content"]["patch"]["format"] == "git_patch")
                );
                assert!(!root.path().join("renamed").exists());
                assert_eq!(
                    std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
                    "after\n"
                );
                connection
                    .send_request(v2::PromptRequest::new(session.clone(), vec!["read".into()]))
                    .block_task()
                    .await?;
                let second = collect(&mut updates).await;
                assert!(second.iter().any(|u| u["toolCallId"] == "tool-2-1"));
                connection
                    .send_request(v2::CloseSessionRequest::new(session.clone()))
                    .block_task()
                    .await?;
                let mut previous = None;
                for _ in 0..2 {
                    let mut resume = v2::ResumeSessionRequest::new(session.clone(), root.path());
                    resume.replay_from = Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new()));
                    connection.send_request(resume).block_task().await?;
                    let replay = collect(&mut updates)
                        .await
                        .into_iter()
                        .filter(|u| u["sessionUpdate"] == "tool_call_update")
                        .collect::<Vec<_>>();
                    assert_eq!(replay.len(), operations.len() + 1);
                    assert_eq!(replay[0]["status"], "completed");
                    assert_eq!(replay[0]["content"][0]["type"], "diff");
                    if let Some(previous) = &previous {
                        assert_eq!(&replay, previous);
                    }
                    previous = Some(replay);
                }
                assert!(updates.try_recv().is_err());
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    for _ in 0..operations.len() + 3 {
        server.request();
    }
    server.assert_no_more_requests();
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn raw_terminal_bytes_and_cancelled_tool_are_published_before_idle(#[case] close: bool) {
    use base64::Engine;
    let root = tempfile::tempdir().unwrap();
    let server = Server::start(vec![Response::json(call(
        "openai-chat",
        "shell",
        "bash",
        json!({"command":"printf '\\377\\000hi'; while :; do :; done"}),
    ))]);
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                send.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async |_: v2::RequestPermissionRequest, responder, _| {
                responder.respond(
                    serde_json::from_value::<v2::RequestPermissionResponse>(
                        json!({"outcome":{"outcome":"selected","optionId":"allow_once"}}),
                    )
                    .unwrap(),
                )
            },
            agent_client_protocol::on_receive_request!(),
        );
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::builtins_with_bash(support::bash_settings()).unwrap(),
    );
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &server.url, ""),
                runtime,
            ),
            async |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("terminal-client", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session.clone(), vec!["run".into()]))
                    .block_task()
                    .await?;
                let mut bytes = Vec::new();
                let mut terminal = None;
                while bytes.len() < 4 {
                    let value = serde_json::to_value(updates.recv().await.unwrap().update).unwrap();
                    if value["sessionUpdate"] == "terminal_update" {
                        terminal = Some(value["terminalId"].clone());
                    }
                    if value["sessionUpdate"] == "terminal_output_chunk" {
                        assert_eq!(Some(value["terminalId"].clone()), terminal);
                        bytes.extend(
                            base64::engine::general_purpose::STANDARD
                                .decode(value["data"].as_str().unwrap())
                                .unwrap(),
                        );
                    }
                }
                assert_eq!(bytes, [255, 0, b'h', b'i']);
                let mut resume = v2::ResumeSessionRequest::new(session.clone(), root.path());
                resume.replay_from = Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new()));
                connection.send_request(resume).block_task().await?;
                let mut replayed_terminal = false;
                let mut replayed_tool = false;
                loop {
                    let value = serde_json::to_value(updates.recv().await.unwrap().update).unwrap();
                    if value["sessionUpdate"] == "terminal_update" {
                        assert_eq!(
                            base64::engine::general_purpose::STANDARD
                                .decode(value["output"]["data"].as_str().unwrap())
                                .unwrap(),
                            bytes
                        );
                        replayed_terminal = true;
                    }
                    if value["sessionUpdate"] == "tool_call_update" {
                        assert!(
                            replayed_terminal,
                            "terminal definition precedes its reference"
                        );
                        assert_eq!(value["status"], "in_progress");
                        replayed_tool = true;
                    }
                    if value["state"] == "running" {
                        break;
                    }
                }
                assert!(replayed_terminal && replayed_tool);
                if close {
                    connection
                        .send_request(v2::CloseSessionRequest::new(session.clone()))
                        .block_task()
                        .await?;
                } else {
                    connection
                        .send_notification(v2::CancelSessionNotification::new(session.clone()))?;
                }
                let tail = collect(&mut updates).await;
                assert!(tail.iter().any(
                    |u| u["sessionUpdate"] == "terminal_update" && u["exitStatus"].is_object()
                ));
                assert!(
                    tail.iter()
                        .any(|u| u["toolCallId"] == "tool-1-1" && u["status"] == "cancelled")
                );
                assert_eq!(tail.last().unwrap()["stopReason"], "cancelled");
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
#[case("copy")]
#[case("move")]
#[case("delete")]
#[tokio::test]
async fn nested_directory_guidance_reaches_the_model_before_any_mutation(
    #[case] operation: &'static str,
) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("source/nested")).unwrap();
    std::fs::write(
        root.path().join("source/nested/AGENTS.md"),
        "NESTED-MUTATION-GUIDANCE",
    )
    .unwrap();
    let checked_root = root.path().to_path_buf();
    let args = if operation == "delete" {
        json!({"path":"source","recursive":true})
    } else {
        json!({"source":"source","destination":"destination","recursive":true})
    };
    let mut args = args;
    if operation == "move" {
        args.as_object_mut().unwrap().remove("recursive");
    }
    let mut step = 0;
    let server = Server::respond(3, move |request| {
        step += 1;
        match step {
            1 => Response::json(call("openai-chat", "initial", operation, args.clone())),
            2 => {
                assert!(checked_root.join("source/nested/AGENTS.md").exists());
                assert!(!checked_root.join("destination").exists());
                assert!(
                    request.body["messages"]
                        .to_string()
                        .contains("NESTED-MUTATION-GUIDANCE")
                );
                assert!(
                    request.body["messages"]
                        .to_string()
                        .contains("guidance_required")
                );
                Response::json(call("openai-chat", "reconsidered", operation, args.clone()))
            }
            _ => Response::json(answer("openai-chat")),
        }
    });
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::builtins().unwrap(),
    )
    .with_instructions(bee::instructions::Sources::new(None));
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            send.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &server.url, ""),
                runtime,
            ),
            async |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("guidance-client", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(
                        session,
                        vec!["mutate directory".into()],
                    ))
                    .block_task()
                    .await?;
                let live = collect(&mut updates).await;
                assert_eq!(live.last().unwrap()["stopReason"], "end_turn");
                assert!(
                    live.iter()
                        .any(|u| u["toolCallId"] == "tool-1-1" && u["status"] == "failed")
                );
                assert!(
                    live.iter()
                        .any(|u| u["toolCallId"] == "tool-1-2" && u["status"] == "completed")
                );
                assert!(
                    !live
                        .iter()
                        .any(|u| u["toolCallId"] == "tool-1-1" && u["content"]["type"] == "diff")
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
async fn storage_failure_after_execution_closes_the_published_card_without_exposing_uncommitted_results()
 {
    let root = tempfile::tempdir().unwrap();
    let server = Server::start(vec![Response::json(call(
        "openai-chat",
        "break",
        "break_history",
        json!({}),
    ))]);
    let mut registry = bee::tool::Registry::default();
    registry.register(BreakHistory).unwrap();
    let runtime = bee::acp::Runtime::new(bee::agent::Limits::default(), registry)
        .with_history(bee::history::Store::open(root.path().join("state")).unwrap());
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            send.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &server.url, ""),
                runtime,
            ),
            async |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("storage-client", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["work".into()]))
                    .block_task()
                    .await?;
                let live = collect(&mut updates).await;
                assert_eq!(live.last().unwrap()["stopReason"], "error");
                assert!(
                    live.iter()
                        .any(|u| u["toolCallId"] == "tool-1-1" && u["status"] == "in_progress")
                );
                assert!(
                    live.iter()
                        .any(|u| u["toolCallId"] == "tool-1-1" && u["status"] == "failed")
                );
                assert!(
                    !serde_json::to_string(&live)
                        .unwrap()
                        .contains("must-not-be-published")
                );
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}
