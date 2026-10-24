//! Production inference through client-hosted MCP, not a test-local agent.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::mcp_server::McpServer;
use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Agent, Client, Error, Responder};
use agent_client_protocol_rmcp::McpServerExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use support::{Response, Server};

#[derive(Deserialize, Serialize, JsonSchema)]
struct Input {
    message: String,
    nested: Value,
}

fn arguments() -> Value {
    json!({"message":"héllo 🐝", "nested":{"items":[1, true, null, {"key":"value"}]}})
}

fn tool_name(protocol: &str, body: &Value) -> String {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|tool| {
            let tool = if protocol == "openai-chat" {
                &tool["function"]
            } else {
                tool
            };
            tool["name"]
                .as_str()
                .filter(|name| name.starts_with("mcp_"))
                .map(str::to_owned)
        })
        .expect("MCP tool advertised")
}

fn call(protocol: &str, name: &str) -> Value {
    let arguments = arguments();
    match protocol {
        "openai-chat" => {
            json!({"choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call-1","type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]})
        }
        "openai-responses" => {
            json!({"status":"completed","output":[{"id":"reason-1","type":"reasoning","encrypted_content":"native-reasoning","summary":[]},{"id":"item-1","type":"function_call","call_id":"call-1","name":name,"arguments":arguments.to_string()}]})
        }
        "anthropic-messages" => {
            json!({"type":"message","role":"assistant","content":[{"type":"tool_use","id":"call-1","name":name,"input":arguments}],"stop_reason":"tool_use"})
        }
        _ => unreachable!(),
    }
}

fn answer(protocol: &str) -> Value {
    match protocol {
        "openai-chat" => support::chat_text("done"),
        "openai-responses" => {
            json!({"status":"completed","output":[{"id":"answer","type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]})
        }
        "anthropic-messages" => {
            json!({"type":"message","role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"})
        }
        _ => unreachable!(),
    }
}

fn model(protocol: &'static str) -> Server {
    let mut index = 0;
    Server::respond(3, move |request| {
        index += 1;
        Response::json(if index == 1 {
            call(protocol, &tool_name(protocol, &request.body))
        } else {
            answer(protocol)
        })
    })
}

async fn initialize(
    connection: &agent_client_protocol::V2ConnectionTo<Agent>,
) -> Result<(), Error> {
    connection
        .send_request(v2::InitializeRequest::new(
            ProtocolVersion::V2,
            v2::Implementation::new("mcp-test", "0"),
        ))
        .block_task()
        .await?;
    Ok(())
}

async fn idle(
    updates: &mut tokio::sync::mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
) -> v2::IdleStateUpdate {
    completion(updates).await.0
}

async fn completion(
    updates: &mut tokio::sync::mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
) -> (v2::IdleStateUpdate, String) {
    let mut text = String::new();
    loop {
        let update = updates.recv().await.unwrap().update;
        if let v2::SessionUpdate::AgentMessageChunk(chunk) = &update
            && let v2::ContentBlock::Text(content) = &chunk.content
        {
            text.push_str(&content.text);
        }
        if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) = update {
            return (idle, text);
        }
    }
}

#[rstest::rstest]
#[case("openai-chat")]
#[case("openai-responses")]
#[case("anthropic-messages")]
#[tokio::test]
async fn production_inference_calls_a_client_hosted_tool_and_retains_its_structured_result(
    #[case] protocol: &'static str,
) {
    let http = model(protocol);
    let profiles = support::providers(protocol, &http.url, "");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::clone(&seen);
    let server = McpServer::<Agent>::builder("echo-server")
        .tool_fn(
            "echo.original",
            "Echo nested input",
            async move |input: Input, _| {
                calls
                    .lock()
                    .unwrap()
                    .push(serde_json::to_value(&input).unwrap());
                Ok(input)
            },
            agent_client_protocol_rmcp::tool_fn!(),
        )
        .build();
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
                profiles,
                bee::acp::Runtime::new(
                    bee::agent::Limits::default(),
                    bee::tool::builtins_with_bash(support::bash_settings()).unwrap(),
                ),
            ),
            async move |connection| {
                initialize(&connection).await?;
                let session = connection
                    .build_session(std::env::temp_dir())
                    .with_mcp_server(server)?
                    .start_session()
                    .block_task()
                    .await?
                    .response()
                    .session_id
                    .clone();
                for _ in 0..2 {
                    connection
                        .send_request(v2::PromptRequest::new(
                            session.clone(),
                            vec!["question".into()],
                        ))
                        .block_task()
                        .await?;
                    let (idle, text) = completion(&mut updates).await;
                    assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                    assert_eq!(text, "done");
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![arguments()]);
    let first = http.request();
    assert!(
        first.body.to_string().contains("bash"),
        "native tools coexist"
    );
    let second = http.request().body;
    let result = result(protocol, &second);
    assert_eq!(result["result"]["structuredContent"], arguments());
    if protocol == "openai-responses" {
        assert!(second.to_string().contains("native-reasoning"));
    }
    assert!(
        http.request()
            .body
            .to_string()
            .contains("structuredContent"),
        "successful history retained"
    );
}

fn result(protocol: &str, body: &Value) -> Value {
    let text = match protocol {
        "openai-chat" => {
            let message = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "tool")
                .unwrap();
            assert_eq!(message["tool_call_id"], "call-1");
            message["content"].as_str().unwrap()
        }
        "openai-responses" => {
            let item = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["type"] == "function_call_output")
                .unwrap();
            assert_eq!(item["call_id"], "call-1");
            item["output"].as_str().unwrap()
        }
        "anthropic-messages" => {
            let item = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().into_iter().flatten())
                .find(|m| m["type"] == "tool_result")
                .unwrap();
            assert_eq!(item["tool_use_id"], "call-1");
            item["content"].as_str().unwrap()
        }
        _ => unreachable!(),
    };
    serde_json::from_str(text).unwrap()
}

fn declaration() -> v2::McpServer {
    serde_json::from_value(json!({"type":"acp","serverId":"client-server","name":"client-tools"}))
        .unwrap()
}

#[tokio::test]
async fn paginated_servers_with_identical_tool_names_route_stably_and_stay_session_scoped() {
    let mut index = 0;
    let http = Server::respond(5, move |request| {
        let step = index;
        index += 1;
        Response::json(if step == 0 || step == 2 {
            let wanted = if step == 0 { "first /" } else { "second /" };
            let tool = request.body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| {
                    tool["function"]["description"]
                        .as_str()
                        .unwrap()
                        .starts_with(wanted)
                })
                .unwrap();
            call("openai-chat", tool["function"]["name"].as_str().unwrap())
        } else {
            answer("openai-chat")
        })
    });
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::clone(&seen);
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let mut ids = std::collections::HashSet::new();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: v2::MessageMcpRequest,
                        responder: Responder<v2::MessageMcpResponse>,
                        _| {
                assert!(ids.insert(request.request_id.to_string()));
                let params = request.params.unwrap();
                if request.method == "tools/list" {
                    return responder.respond(v2::MessageMcpResponse::success(
                        if params.contains_key("cursor") {
                            assert_eq!(params["cursor"], "page2");
                            json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]})
                        } else {
                            json!({"tools":[],"nextCursor":"page2"})
                        },
                    ));
                }
                assert_eq!(params["name"], "echo");
                assert_eq!(params["arguments"], arguments());
                calls.lock().unwrap().push(request.server_id.to_string());
                responder.respond(v2::MessageMcpResponse::success(
                    json!({"structuredContent":params["arguments"]}),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                bee::acp::Runtime::new(
                    bee::agent::Limits::default(),
                    bee::tool::Registry::default(),
                ),
            ),
            async move |connection| {
                initialize(&connection).await?;
                for names in [vec!["first", "second"], vec!["second", "first"], vec![]] {
                    let servers = names
                        .into_iter()
                        .map(|name| {
                            serde_json::from_value(
                                json!({"type":"acp","serverId":name,"name":name}),
                            )
                            .unwrap()
                        })
                        .collect();
                    let session = connection
                        .send_request(
                            v2::NewSessionRequest::new(std::env::temp_dir()).mcp_servers(servers),
                        )
                        .block_task()
                        .await?
                        .session_id;
                    connection
                        .send_request(v2::PromptRequest::new(session, vec!["question".into()]))
                        .block_task()
                        .await?;
                    assert_eq!(
                        idle(&mut updates).await.stop_reason,
                        Some(v2::StopReason::EndTurn)
                    );
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(*seen.lock().unwrap(), vec!["first", "second"]);
    let first = http.request().body;
    http.request();
    assert_eq!(
        first["tools"],
        http.request().body["tools"],
        "discovery order cannot rename tools"
    );
    http.request();
    assert!(
        http.request().body.get("tools").is_none(),
        "attachments cannot leak to another session"
    );
}

#[rstest::rstest]
#[case::cycle("cycle")]
#[case::schema("schema")]
#[case::malformed("malformed")]
#[case::denied("denied")]
#[case::duplicate("duplicate")]
#[case::too_many("too_many")]
#[case::catalog_bytes("catalog_bytes")]
#[tokio::test]
async fn failed_discovery_is_sanitized_and_resume_with_no_servers_recovers(
    #[case] kind: &'static str,
) {
    let http = Server::start(vec![Response::json(support::chat_text("recovered"))]);
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(async move |update: v2::UpdateSessionNotification, _| {
        tx.send(update).map_err(Error::into_internal_error)
    }, agent_client_protocol::on_receive_notification!()).on_receive_request(
        async move |_: v2::MessageMcpRequest, responder: Responder<v2::MessageMcpResponse>, _| {
            let tool = json!({"name":"echo","inputSchema":{"type":"object"}});
            responder.respond(match kind {
                "cycle" => v2::MessageMcpResponse::success(json!({"tools":[],"nextCursor":"private-sentinel"})),
                "schema" => v2::MessageMcpResponse::success(json!({"tools":[{"name":"echo","inputSchema":{"type":"object","$ref":"https://private-sentinel/schema"}}]})),
                "malformed" => v2::MessageMcpResponse::success(json!({"tools":"private-sentinel"})),
                "duplicate" => v2::MessageMcpResponse::success(json!({"tools":[tool,tool]})),
                "too_many" => v2::MessageMcpResponse::success(json!({"tools":(0..257).map(|id| json!({"name":format!("tool-{id}"),"inputSchema":{"type":"object"}})).collect::<Vec<_>>()})),
                "catalog_bytes" => v2::MessageMcpResponse::success(json!({"tools":[],"extension":"private-sentinel".repeat(100_000)})),
                "denied" => v2::MessageMcpResponse::error(v2::McpError::new(-32000,"private-sentinel")),
                _ => unreachable!(),
            })
        }, agent_client_protocol::on_receive_request!(),
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                bee::acp::Runtime::new(
                    bee::agent::Limits::default(),
                    bee::tool::Registry::default(),
                ),
            ),
            async |connection| {
                initialize(&connection).await?;
                let cwd = std::env::temp_dir();
                let session = connection
                    .send_request(
                        v2::NewSessionRequest::new(cwd.clone()).mcp_servers(vec![declaration()]),
                    )
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(
                        session.clone(),
                        vec!["discarded-question".into()],
                    ))
                    .block_task()
                    .await?;
                let stopped = idle(&mut updates).await;
                assert!(matches!(
                    stopped.stop_reason,
                    Some(v2::StopReason::Error(_))
                ));
                assert!(
                    !serde_json::to_string(&stopped)
                        .unwrap()
                        .contains("private-sentinel")
                );
                http.assert_no_more_requests();
                // A rejected resume cannot accidentally erase the previous attachment.
                assert!(
                    connection
                        .send_request(v2::ResumeSessionRequest::new(
                            session.clone(),
                            cwd.join("wrong")
                        ))
                        .block_task()
                        .await
                        .is_err()
                );
                connection
                    .send_request(v2::PromptRequest::new(
                        session.clone(),
                        vec!["discarded-again".into()],
                    ))
                    .block_task()
                    .await?;
                assert!(matches!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::Error(_))
                ));
                connection
                    .send_request(v2::ResumeSessionRequest::new(session.clone(), cwd))
                    .block_task()
                    .await?;
                idle(&mut updates).await;
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["recover".into()]))
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
    let request = http.request();
    assert!(!request.body.to_string().contains("discarded"));
    assert!(
        request
            .body
            .get("tools")
            .is_none_or(|tools| tools.as_array().unwrap().is_empty())
    );
}

#[rstest::rstest]
#[case::discovery(false)]
#[case::invocation(true)]
#[tokio::test]
async fn cancellation_drops_pending_mcp_work_without_committing_the_turn(
    #[case] during_call: bool,
    #[values(false, true)] close: bool,
) {
    let mut index = 0;
    let http = Server::respond(if during_call { 3 } else { 2 }, move |request| {
        index += 1;
        Response::json(if during_call && index == 1 {
            call("openai-chat", &tool_name("openai-chat", &request.body))
        } else {
            answer("openai-chat")
        })
    });
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let (started, mut pending) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::clone(&release);
    let cancellation = Arc::new(Mutex::new(
        None::<agent_client_protocol::RequestCancellation>,
    ));
    let observed = Arc::clone(&cancellation);
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                if matches!(&update.update, v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle))
                    if idle.stop_reason == Some(v2::StopReason::Cancelled)) {
                    // Client dispatch must see request cancellation before idle,
                    // rather than merely ignore an eventual late tool result.
                    assert!(observed.lock().unwrap().as_ref().unwrap().is_cancelled());
                }
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: v2::MessageMcpRequest,
                        responder: Responder<v2::MessageMcpResponse>,
                        connection: agent_client_protocol::V2ConnectionTo<Agent>| {
                if during_call && request.method == "tools/list" {
                    return responder.respond(v2::MessageMcpResponse::success(
                        json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}),
                    ));
                }
                *cancellation.lock().unwrap() = Some(responder.cancellation());
                started.send(()).unwrap();
                let gate = Arc::clone(&gate);
                // Leave dispatch free to process cancellation while the remote operation waits.
                connection.spawn(async move {
                    gate.notified().await;
                    let _ = responder.respond(v2::MessageMcpResponse::success(
                        json!({"structuredContent":"late-result"}),
                    ));
                    Ok(())
                })
            },
            agent_client_protocol::on_receive_request!(),
        );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                bee::acp::Runtime::new(
                    bee::agent::Limits::default(),
                    bee::tool::Registry::default(),
                ),
            ),
            async move |connection| {
                initialize(&connection).await?;
                let cwd = std::env::temp_dir();
                let session = connection
                    .send_request(
                        v2::NewSessionRequest::new(cwd.clone()).mcp_servers(vec![declaration()]),
                    )
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(
                        session.clone(),
                        vec!["cancelled-question".into()],
                    ))
                    .block_task()
                    .await?;
                pending.recv().await.unwrap();
                // A blocked client tool must not block another session's inference.
                let other = connection
                    .send_request(v2::NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(other, vec!["independent".into()]))
                    .block_task()
                    .await?;
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::EndTurn)
                );
                if close {
                    connection
                        .send_request(v2::CloseSessionRequest::new(session.clone()))
                        .block_task()
                        .await?;
                } else {
                    connection
                        .send_notification(v2::CancelSessionNotification::new(session.clone()))?;
                }
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::Cancelled)
                );
                connection
                    .send_request(v2::CloseSessionRequest::new(session.clone()))
                    .block_task()
                    .await?;
                connection
                    .send_request(v2::ResumeSessionRequest::new(session.clone(), cwd))
                    .block_task()
                    .await?;
                idle(&mut updates).await;
                release.notify_one();
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["recover".into()]))
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
    if during_call {
        http.request();
    }
    http.request();
    let recovered = http.request().body.to_string();
    assert!(!recovered.contains("cancelled-question"));
    assert!(!recovered.contains("late-result"));
}

#[rstest::rstest]
#[case::denied("denied")]
#[case::tool_error("tool_error")]
#[case::unavailable("unavailable")]
#[case::malformed("malformed")]
#[case::interactive("interactive")]
#[case::oversized("oversized")]
#[case::invalid_arguments("invalid_arguments")]
#[tokio::test]
async fn tool_failures_reach_the_model_and_leave_the_session_usable(#[case] kind: &'static str) {
    let http = model("anthropic-messages");
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let mut ids = std::collections::HashSet::new();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: v2::MessageMcpRequest,
                        responder: Responder<v2::MessageMcpResponse>,
                        _| {
                assert!(ids.insert(request.request_id.to_string()));
                assert_eq!(request.server_id.to_string(), "client-server");
                let params = request.params.unwrap();
                assert_eq!(
                    params["_meta"]["io.modelcontextprotocol/protocolVersion"],
                    "2026-07-28"
                );
                if request.method == "tools/list" {
                    let schema = if kind == "invalid_arguments" {
                        json!({"type":"object","required":["missing"]})
                    } else {
                        json!({"type":"object"})
                    };
                    return responder.respond(v2::MessageMcpResponse::success(
                        json!({"tools":[{"name":"echo.original","inputSchema":schema}]}),
                    ));
                }
                assert_eq!(params["name"], "echo.original");
                assert_eq!(params["arguments"], arguments());
                if kind == "unavailable" {
                    return responder
                        .respond_with_error(Error::internal_error().data("private-sentinel"));
                }
                responder.respond(match kind {
                    "denied" => v2::MessageMcpResponse::error(
                        v2::McpError::new(-32000, "Permission denied").data(Value::Null),
                    ),
                    "tool_error" => v2::MessageMcpResponse::success(
                        json!({"isError":true,"content":[{"type":"text","text":"tool disabled"}]}),
                    ),
                    "malformed" => {
                        v2::MessageMcpResponse::success(json!({"isError":"private-sentinel"}))
                    }
                    "interactive" => v2::MessageMcpResponse::success(
                        json!({"resultType":"input_required","requestState":"private-sentinel"}),
                    ),
                    "oversized" => v2::MessageMcpResponse::success(
                        json!({"structuredContent":"x".repeat(bee::tool::MAX_OUTPUT_BYTES)}),
                    ),
                    _ => unreachable!(),
                })
            },
            agent_client_protocol::on_receive_request!(),
        );
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("anthropic-messages", &http.url, ""),
                bee::acp::Runtime::new(
                    bee::agent::Limits::default(),
                    bee::tool::Registry::default(),
                ),
            ),
            async move |connection| {
                initialize(&connection).await?;
                let session = connection
                    .send_request(
                        v2::NewSessionRequest::new(std::env::temp_dir())
                            .mcp_servers(vec![declaration()]),
                    )
                    .block_task()
                    .await?
                    .session_id;
                for _ in 0..2 {
                    connection
                        .send_request(v2::PromptRequest::new(
                            session.clone(),
                            vec!["question".into()],
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(
                        idle(&mut updates).await.stop_reason,
                        Some(v2::StopReason::EndTurn)
                    );
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    http.request();
    let second = http.request().body;
    assert!(second.to_string().contains("\"is_error\":true"));
    let output = result("anthropic-messages", &second);
    assert!(!output.to_string().contains("private-sentinel"));
    if kind == "denied" {
        assert!(output["error"]["data"].is_null());
        assert_eq!(output["error"]["code"], -32000);
    }
    http.request();
}
