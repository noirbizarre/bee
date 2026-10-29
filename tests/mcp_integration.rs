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

fn project_config(root: &std::path::Path, text: &str) {
    std::fs::create_dir_all(root.join(".agents")).unwrap();
    std::fs::write(root.join(".agents/mcp.toml"), text).unwrap();
}

fn project_environment() -> bee::config::Environment {
    bee::config::Environment::default().with_variable("PATH", &std::env::var("PATH").unwrap())
}

fn stdio_config(root: &std::path::Path, block: bool) -> String {
    let command = support::bash_executable();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/mcp.sh");
    // TOML literal strings preserve Windows paths without escaping executable settings.
    format!(
        "[servers.echo]\ncommand=['{}', '{}']\n[servers.echo.env]\nMCP_MARKER='{}'\nMCP_CALLS='{}'\nMCP_BLOCK='{}'\n",
        command.display(),
        script.display(),
        root.join("started").display(),
        root.join("called").display(),
        if block { "yes" } else { "no" }
    )
}

fn direct_http(count: usize, sse: bool) -> Server {
    Server::respond(count, move |request| {
        let result = match request.body["method"].as_str().unwrap() {
            "initialize" => {
                json!({"protocolVersion":request.body["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
            }
            "notifications/initialized" => {
                let mut response = Response::json(json!({}));
                response.status = 202;
                return response;
            }
            "tools/list" => {
                json!({"tools":[{"name":"echo.original","inputSchema":{"type":"object"}}]})
            }
            "tools/call" => {
                json!({"content":[],"structuredContent":{"nested":[1,true,null]},"extension":42})
            }
            _ => panic!("unexpected method"),
        };
        let response = json!({"jsonrpc":"2.0","id":request.body["id"],"result":result});
        if sse {
            Response::sse(format!("event: message\ndata: {response}\n\n"))
        } else {
            Response::json(response)
        }
    })
}

#[tokio::test]
async fn changed_credentials_require_fresh_connection_consent_and_invalid_reload_retires_old_processes()
 {
    let root = tempfile::tempdir().unwrap();
    let config = stdio_config(root.path(), false);
    project_config(root.path(), &config);
    let model = Server::start(
        (0..3)
            .map(|_| Response::json(answer("openai-chat")))
            .collect(),
    );
    let permissions = Arc::new(Mutex::new(Vec::new()));
    let recorded = permissions.clone();
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async move |request: v2::RequestPermissionRequest,
                        responder: Responder<v2::RequestPermissionResponse>,
                        _| {
                recorded
                    .lock()
                    .unwrap()
                    .push(serde_json::to_value(request.subject).unwrap());
                responder.respond(
                    serde_json::from_value(
                        json!({"outcome":{"outcome":"selected","optionId":"allow_once"}}),
                    )
                    .unwrap(),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        );
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_project_mcp(project_environment());
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &model.url, ""),
                runtime,
            ),
            async |connection| {
                initialize(&connection).await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path().to_path_buf()))
                    .block_task()
                    .await?
                    .session_id;
                for step in 0..4 {
                    if step == 2 {
                        project_config(
                            root.path(),
                            &format!("{config}PRIVATE='private-credential-sentinel'\n"),
                        );
                    }
                    if step == 3 {
                        project_config(root.path(), "private-credential-sentinel = [");
                    }
                    connection
                        .send_request(v2::PromptRequest::new(
                            session.clone(),
                            vec!["question".into()],
                        ))
                        .block_task()
                        .await?;
                    let stop = idle(&mut updates).await.stop_reason;
                    if step == 3 {
                        assert!(matches!(stop, Some(v2::StopReason::Error(_))));
                    } else {
                        assert_eq!(stop, Some(v2::StopReason::EndTurn));
                    }
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let permissions = permissions.lock().unwrap();
    assert_eq!(permissions.len(), 2);
    assert_ne!(permissions[0]["revision"], permissions[1]["revision"]);
    assert!(
        !serde_json::to_string(&*permissions)
            .unwrap()
            .contains("private-credential-sentinel")
    );
    let pids = std::fs::read_to_string(root.path().join("started")).unwrap();
    assert_eq!(pids.lines().count(), 2);
    #[cfg(unix)]
    for pid in pids.lines() {
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid.parse().unwrap()), None).is_err()
        );
    }
}

#[rstest::rstest]
#[case("openai-chat", "stdio")]
#[case("openai-responses", "stdio")]
#[case("anthropic-messages", "stdio")]
#[case("openai-chat", "http")]
#[case("openai-responses", "http")]
#[case("anthropic-messages", "http")]
#[case("openai-chat", "sse")]
#[case("openai-chat", "attachment-stdio")]
#[tokio::test]
async fn project_servers_require_separate_approvals_reuse_connections_and_preserve_results(
    #[case] protocol: &'static str,
    #[case] transport: &str,
) {
    let root = tempfile::tempdir().unwrap();
    let model = model(protocol);
    let http = matches!(transport, "http" | "sse").then(|| direct_http(5, transport == "sse"));
    project_config(
        root.path(),
        &http
            .as_ref()
            .map(|s| format!("[servers.echo]\nurl='{}'", s.url))
            .unwrap_or_else(|| stdio_config(root.path(), false)),
    );
    let attachments = if transport == "attachment-stdio" {
        let definition = bee::mcp::config::discover(root.path(), &project_environment())
            .unwrap()
            .remove(0);
        std::fs::remove_file(root.path().join(".agents/mcp.toml")).unwrap();
        let bee::mcp::config::Transport::Stdio {
            command, args, env, ..
        } = definition.transport
        else {
            panic!()
        };
        serde_json::from_value(json!([{"type":"stdio","name":"echo","command":command,"args":args,"env":env.into_iter().map(|(name,value)| json!({"name":name,"value":value})).collect::<Vec<_>>()}])).unwrap()
    } else {
        Vec::new()
    };
    let profiles = support::providers(protocol, &model.url, "");
    let permissions = Arc::new(Mutex::new(Vec::new()));
    let recorded = permissions.clone();
    let marker = root.path().join("started");
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async move |request: v2::RequestPermissionRequest,
                        responder: Responder<v2::RequestPermissionResponse>,
                        _| {
                let subject = serde_json::to_value(&request.subject).unwrap();
                if subject["type"] == "_bee_mcp_connection" {
                    assert!(!marker.exists(), "spawn preceded approval");
                }
                recorded.lock().unwrap().push(subject);
                responder.respond(
                    serde_json::from_value(
                        json!({"outcome":{"outcome":"selected","optionId":"allow_once"}}),
                    )
                    .unwrap(),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        );
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_project_mcp(project_environment());
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(profiles, runtime),
            async |connection| {
                initialize(&connection).await?;
                let mut request = v2::NewSessionRequest::new(root.path().to_path_buf());
                request.mcp_servers = attachments;
                let session = connection
                    .send_request(request)
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
                connection
                    .send_request(v2::CloseSessionRequest::new(session))
                    .block_task()
                    .await?;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let requests = permissions.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "one connection approval and one call approval"
    );
    assert_eq!(requests[0]["type"], "_bee_mcp_connection");
    assert_eq!(requests[1]["type"], "tool_call");
    if transport.ends_with("stdio") {
        let started = std::fs::read_to_string(root.path().join("started")).unwrap();
        assert_eq!(
            started.lines().count(),
            1,
            "second prompt must reuse process"
        );
        #[cfg(unix)]
        assert!(
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(started.trim().parse().unwrap()),
                None
            )
            .is_err(),
            "close response preceded process reap"
        );
    }
    model.request();
    let result_request = model.request();
    assert!(result_request.body.to_string().contains("extension"));
    assert!(result_request.body.to_string().contains("42"));
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn denied_connection_has_no_process_or_network_effects(#[case] http: bool) {
    let root = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    project_config(
        root.path(),
        &if http {
            format!(
                "[servers.echo]\nurl='http://{}/mcp'",
                listener.local_addr().unwrap()
            )
        } else {
            stdio_config(root.path(), false)
        },
    );
    let model = model("openai-chat");
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async |_: v2::RequestPermissionRequest,
                   responder: Responder<v2::RequestPermissionResponse>,
                   _| {
                responder.respond(
                    serde_json::from_value(
                        json!({"outcome":{"outcome":"selected","optionId":"reject_once"}}),
                    )
                    .unwrap(),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        );
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_project_mcp(project_environment());
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &model.url, ""),
                runtime,
            ),
            async |connection| {
                initialize(&connection).await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path().to_path_buf()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["question".into()]))
                    .block_task()
                    .await?;
                assert!(matches!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::Error(_))
                ));
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!root.path().join("started").exists());
    assert!(listener.accept().is_err());
    model.assert_no_more_requests();
}

#[rstest::rstest]
#[case("cancel", false)]
#[case("close", false)]
#[case("resume", false)]
#[case("cancel", true)]
#[case("close", true)]
#[tokio::test]
async fn interrupted_stdio_work_is_reaped_before_terminal_idle(
    #[case] operation: &str,
    #[case] initializing: bool,
) {
    let root = tempfile::tempdir().unwrap();
    let mut config = stdio_config(root.path(), true);
    if initializing {
        config.push_str("MCP_INIT_BLOCK='yes'\n");
    }
    project_config(root.path(), &config);
    let model = model("openai-chat");
    let marker = root.path().join("started");
    let observed_marker = marker.clone();
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async |_: v2::RequestPermissionRequest,
                   responder: Responder<v2::RequestPermissionResponse>,
                   _| {
                responder.respond(
                    serde_json::from_value(
                        json!({"outcome":{"outcome":"selected","optionId":"allow_once"}}),
                    )
                    .unwrap(),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                if matches!(
                    &update.update,
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                ) {
                    #[cfg(unix)]
                    if let Ok(pid) = std::fs::read_to_string(&observed_marker) {
                        assert!(
                            nix::sys::signal::kill(
                                nix::unistd::Pid::from_raw(pid.trim().parse().unwrap()),
                                None
                            )
                            .is_err(),
                            "idle preceded process cleanup"
                        );
                    }
                }
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        );
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_project_mcp(project_environment());
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &model.url, ""),
                runtime,
            ),
            async |connection| {
                initialize(&connection).await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(root.path().to_path_buf()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(
                        session.clone(),
                        vec!["question".into()],
                    ))
                    .block_task()
                    .await?;
                let waiting = if initializing {
                    marker
                } else {
                    root.path().join("called")
                };
                loop {
                    // Opening the marker creates an empty file before printf writes.
                    // Wait for complete records so cancellation cannot interrupt PID
                    // publication; blocked calls also publish the descendant's PID.
                    if std::fs::read_to_string(&waiting).is_ok_and(|contents| {
                        contents.ends_with('\n')
                            && contents.lines().count() >= if initializing { 1 } else { 2 }
                    }) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                match operation {
                    "close" => {
                        connection
                            .send_request(v2::CloseSessionRequest::new(session.clone()))
                            .block_task()
                            .await?;
                    }
                    "resume" => {
                        project_config(root.path(), "");
                        connection
                            .send_request(v2::ResumeSessionRequest::new(
                                session.clone(),
                                root.path(),
                            ))
                            .block_task()
                            .await?;
                    }
                    _ => connection
                        .send_notification(v2::CancelSessionNotification::new(session.clone()))?,
                }
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::Cancelled)
                );
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    if initializing {
        model.assert_no_more_requests();
    } else {
        model.request();
        model.assert_no_more_requests();
    }
}

#[tokio::test]
async fn client_direct_attachments_and_same_named_project_servers_coexist_without_overrides() {
    let root = tempfile::tempdir().unwrap();
    let project_server = direct_http(4, false);
    let client_server = direct_http(3, false);
    project_config(
        root.path(),
        &format!("[servers.echo]\nurl='{}'", project_server.url),
    );
    let mut index = 0;
    let model = Server::respond(2, move |request| {
        index += 1;
        let tools: Vec<_> = request.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["function"]["name"].as_str().unwrap().starts_with("mcp_"))
            .collect();
        assert_eq!(tools.len(), 2);
        assert_ne!(tools[0]["function"]["name"], tools[1]["function"]["name"]);
        let project = tools
            .iter()
            .find(|t| {
                t["function"]["description"]
                    .as_str()
                    .unwrap()
                    .starts_with("project/")
            })
            .unwrap();
        Response::json(if index == 1 {
            call("openai-chat", project["function"]["name"].as_str().unwrap())
        } else {
            answer("openai-chat")
        })
    });
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async |_: v2::RequestPermissionRequest,
                   responder: Responder<v2::RequestPermissionResponse>,
                   _| {
                responder.respond(
                    serde_json::from_value(
                        json!({"outcome":{"outcome":"selected","optionId":"allow_once"}}),
                    )
                    .unwrap(),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        );
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    )
    .with_project_mcp(project_environment());
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &model.url, ""),
                runtime,
            ),
            async |connection| {
                initialize(&connection).await?;
                let mut request = v2::NewSessionRequest::new(root.path().to_path_buf());
                request.mcp_servers = serde_json::from_value(
                    json!([{"type":"http","name":"echo","url":client_server.url,"headers":[]}]),
                )
                .unwrap();
                let session = connection
                    .send_request(request)
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
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
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
    let mut tool_states = std::collections::BTreeMap::new();
    loop {
        let notification = updates.recv().await.unwrap();
        let session = notification.session_id.to_string();
        let update = notification.update;
        if let v2::SessionUpdate::ToolCallUpdate(tool) = &update {
            let value = serde_json::to_value(tool).unwrap();
            if let Some(status) = value["status"].as_str() {
                tool_states.insert(
                    (session.clone(), tool.tool_call_id.to_string()),
                    status.to_owned(),
                );
            }
        }
        if let v2::SessionUpdate::AgentMessageChunk(chunk) = &update
            && let v2::ContentBlock::Text(content) = &chunk.content
        {
            text.push_str(&content.text);
        }
        if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) = update {
            assert!(
                tool_states
                    .iter()
                    .filter(|((id, _), _)| id == &session)
                    .map(|(_, status)| status)
                    .all(|status| matches!(status.as_str(), "completed" | "failed" | "cancelled")),
                "unfinished MCP tool before idle"
            );
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
    let presentation = Arc::new(Mutex::new(Vec::new()));
    let observed = presentation.clone();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            observed
                .lock()
                .unwrap()
                .push(serde_json::to_value(&update.update).unwrap());
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
    let presentation = presentation.lock().unwrap();
    let statuses = presentation
        .iter()
        .filter(|u| u["sessionUpdate"] == "tool_call_update")
        .collect::<Vec<_>>();
    assert_eq!(statuses.len(), 3);
    assert_eq!(statuses[0]["toolCallId"], "tool-1-1");
    assert_eq!(statuses[0]["kind"], "other");
    assert!(
        statuses[0]["name"]
            .as_str()
            .unwrap()
            .contains("echo.original")
    );
    assert_eq!(
        statuses[2]["rawOutput"]["result"]["structuredContent"],
        arguments()
    );
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
            let wanted = if step == 0 {
                "client/acp/first /"
            } else {
                "client/acp/second /"
            };
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
    assert_eq!(
        http.request().body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["update_plan"],
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
    assert_eq!(
        request.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["update_plan"]
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
    let permission_requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_permissions = permission_requests.clone();
    let client = Client
        .v2()
        .on_receive_request(
            async move |_: v2::RequestPermissionRequest, responder, _| {
                observed_permissions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                responder.respond_with_error(Error::internal_error())
            },
            agent_client_protocol::on_receive_request!(),
        )
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
    assert_eq!(
        permission_requests.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(second.to_string().contains("\"is_error\":true"));
    let output = result("anthropic-messages", &second);
    assert!(!output.to_string().contains("private-sentinel"));
    if kind == "denied" {
        assert!(output["error"]["data"].is_null());
        assert_eq!(output["error"]["code"], -32000);
    }
    http.request();
}
