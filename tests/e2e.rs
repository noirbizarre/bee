//! Official ACPv2 client interoperability with the real Bee subprocess.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::mcp_server::McpServer;
use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, Error, LineDirection};
use agent_client_protocol_rmcp::McpServerExt;
use serde_json::{Value, json};
use support::{Response, Server};

const TIMEOUT: Duration = Duration::from_secs(20);

fn subprocess(root: &tempfile::TempDir, url: &str) -> AcpAgent {
    subprocess_with_path(root, url, None)
}

fn subprocess_with_path(root: &tempfile::TempDir, url: &str, path: Option<&str>) -> AcpAgent {
    let config = root.path().join("bee.toml");
    std::fs::write(
        &config,
        support::native_bash_config(&format!(
            "[catalog]\nenabled=false\n[providers.test]\nmodel='test'\n[providers.test.protocols.openai-chat]\nbase_url='{url}'"
        )),
    )
    .unwrap();
    let shell = support::bash_executable();
    // Resolve the launcher before removing the child's executable search path.
    let shell = if path.is_some() && !shell.is_absolute() {
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join(&shell))
            .find(|candidate| candidate.is_file())
            .expect("Bash is installed on the test host")
    } else {
        shell
    };
    let mut launch =
        // The SDK has no cwd setting. A positional-argument launcher keeps both
        // project configuration and HOME isolated without process-global chdir.
        AcpAgentConfig::new(shell)
            .args([
                "--noprofile",
                "--norc",
                "-c",
                "cd -- \"$1\" && exec \"$2\"",
                "bee-e2e",
                root.path().to_str().unwrap(),
                env!("CARGO_BIN_EXE_bee"),
            ])
            .env("HOME", root.path().to_str().unwrap())
            .env("XDG_CONFIG_HOME", root.path().join("xdg").to_str().unwrap())
            .env("XDG_CACHE_HOME", root.path().join("cache").to_str().unwrap())
            .env("XDG_STATE_HOME", root.path().join("state").to_str().unwrap())
            .env("BEE_CONFIG", config.to_str().unwrap());
    if let Some(path) = path {
        // Restrict only the child; parallel tests retain their normal environment.
        launch = launch.env("PATH", path);
    }
    AcpAgent::new(launch).with_debug(|line, direction| {
        if matches!(direction, LineDirection::Stdout) {
            let message: Value = serde_json::from_str(line).expect("stdout is JSON-RPC only");
            assert_eq!(message["jsonrpc"], "2.0");
        }
    })
}

fn call(id: &str, name: &str, arguments: Value) -> Value {
    json!({"choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]})
}

#[tokio::test]
async fn the_binary_searches_without_rg_on_path_and_retains_correlated_results() {
    let isolated = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("note.txt"), "embedded needle\n").unwrap();
    let http = Server::start(vec![
        Response::json(call(
            "search",
            "grep",
            json!({"pattern":"needle","include":["*.txt"]}),
        )),
        Response::json(support::chat_text("found")),
        Response::json(support::chat_text("remembered")),
    ]);
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        TIMEOUT,
        client.connect_with(
            subprocess_with_path(&isolated, &http.url, Some("")),
            async |connection| {
                initialize(&connection).await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(workspace.path()))
                    .block_task()
                    .await?
                    .session_id;
                turn(&connection, &mut updates, &session, "found").await?;
                turn(&connection, &mut updates, &session, "remembered").await?;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    http.request();
    let searched = result(&http.request(), "search");
    assert_eq!(
        searched,
        json!({"data":{"matches":[{"path":"note.txt","line_number":1,"line":"embedded needle"}],"truncated":false,"skipped_binary_files":0}})
    );
    assert_eq!(result(&http.request(), "search"), searched);
    http.assert_no_more_requests();
}

fn result(request: &support::Request, id: &str) -> Value {
    let message = request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
        .expect("a result is correlated with the preceding call");
    serde_json::from_str(message["content"].as_str().unwrap()).unwrap()
}

async fn initialize(
    connection: &agent_client_protocol::V2ConnectionTo<Agent>,
) -> Result<(), Error> {
    let response = connection
        .send_request(v2::InitializeRequest::new(
            ProtocolVersion::V2,
            v2::Implementation::new("bee-e2e", "0"),
        ))
        .block_task()
        .await?;
    assert_eq!(response.protocol_version, ProtocolVersion::V2);
    let capabilities = serde_json::to_value(response.capabilities).unwrap();
    assert_eq!(capabilities, json!({"session":{"mcp":{"acp":{}}}}));
    Ok(())
}

async fn turn(
    connection: &agent_client_protocol::V2ConnectionTo<Agent>,
    updates: &mut tokio::sync::mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
    session: &v2::SessionId,
    expected: &str,
) -> Result<(), Error> {
    turn_with_gate(connection, updates, session, expected, None).await
}

async fn turn_with_gate(
    connection: &agent_client_protocol::V2ConnectionTo<Agent>,
    updates: &mut tokio::sync::mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
    session: &v2::SessionId,
    expected: &str,
    mut release: Option<std::sync::mpsc::Sender<()>>,
) -> Result<(), Error> {
    let accepted = connection
        .send_request(v2::PromptRequest::new(
            session.clone(),
            vec!["do the work".into()],
        ))
        .block_task()
        .await?;
    let mut text = String::new();
    let mut running = false;
    let mut echoed = false;
    let mut message_id = None;
    loop {
        let notification = updates.recv().await.expect("the update channel stays open");
        assert_eq!(&notification.session_id, session);
        match notification.update {
            v2::SessionUpdate::UserMessage(message) => {
                assert!(!echoed);
                assert_eq!(message.message_id, accepted.message_id);
                echoed = true;
            }
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_)) => {
                assert!(echoed);
                running = true;
            }
            v2::SessionUpdate::AgentMessageChunk(chunk) => {
                assert!(running);
                let id = message_id.get_or_insert(chunk.message_id.clone());
                assert_eq!(*id, chunk.message_id);
                let v2::ContentBlock::Text(content) = chunk.content else {
                    panic!("expected text");
                };
                text.push_str(&content.text);
                if let Some(release) = release.take() {
                    // Completion is withheld until ACP delivered the first chunk.
                    assert_eq!(text, "verified ");
                    release.send(()).unwrap();
                }
            }
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
                assert!(running);
                assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                assert_eq!(text, expected);
                break;
            }
            _ => {}
        }
    }
    // A dispatch barrier makes duplicate terminal updates observable.
    connection
        .send_request(v2::ListSessionsRequest::new())
        .block_task()
        .await?;
    assert!(updates.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn the_binary_completes_a_native_workflow_and_retains_correlated_results() {
    let isolated = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("note.txt"), "source-value\n").unwrap();
    std::fs::write(workspace.path().join("sentinel"), "untouched").unwrap();
    let first = format!(
        "data: {}\n\n",
        json!({"choices":[{"index":0,"delta":{"content":"verified "},"finish_reason":null}]})
    );
    let end = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":"edit"},"finish_reason":"stop"}]})
    );
    let (stream, release) = Response::sse(format!("{first}{end}")).gated_after(first.len());
    let mut stream = Some(stream);
    let mut step = 0;
    let http = Server::respond(9, move |request| {
        let response = match step {
            0 => call("discover", "glob", json!({"pattern":"*.txt"})),
            1 => {
                assert_eq!(
                    result(request, "discover")["data"]["entries"][0]["path"],
                    "note.txt"
                );
                call("list", "list", json!({"path":"."}))
            }
            2 => {
                assert!(
                    result(request, "list")["data"]
                        .to_string()
                        .contains("note.txt")
                );
                let discovered = result(request, "discover")["data"]["entries"][0]["path"].clone();
                call("read", "read", json!({"path":discovered}))
            }
            3 => {
                let read = result(request, "read");
                assert_eq!(read["data"]["content"], "1: source-value\n");
                let content = read["data"]["content"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("1: ")
                    .unwrap();
                call(
                    "write",
                    "write",
                    json!({"path":"created.txt","content":content}),
                )
            }
            4 => {
                assert_eq!(
                    result(request, "write"),
                    json!({"data":{"bytes_written":13}})
                );
                call(
                    "edit",
                    "edit",
                    json!({"path":"created.txt","old_text":"source","new_text":"edited"}),
                )
            }
            5 => {
                assert_eq!(
                    result(request, "edit"),
                    json!({"data":{"bytes_written":13}})
                );
                call("verify", "read", json!({"path":"created.txt"}))
            }
            6 => {
                assert_eq!(
                    result(request, "verify")["data"]["content"],
                    "1: edited-value\n"
                );
                call(
                    "bash",
                    "bash",
                    json!({"command":"printf '%s' \"$(<created.txt)\"; printf checked >&2; exit 7"}),
                )
            }
            7 => {
                let output = result(request, "bash");
                assert_eq!(output["data"]["stdout"], "edited-value");
                assert_eq!(output["data"]["stderr"], "checked");
                assert_eq!(output["data"]["exit_code"], 7);
                step += 1;
                return stream.take().unwrap();
            }
            8 => {
                assert_eq!(
                    result(request, "verify")["data"]["content"],
                    "1: edited-value\n"
                );
                support::chat_text("remembered")
            }
            _ => unreachable!(),
        };
        step += 1;
        Response::json(response)
    });
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_request(
            async |_request: v2::RequestPermissionRequest,
                   responder: agent_client_protocol::Responder<v2::RequestPermissionResponse>,
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
    tokio::time::timeout(
        TIMEOUT,
        client.connect_with(subprocess(&isolated, &http.url), async |connection| {
            initialize(&connection).await?;
            let session = connection
                .send_request(v2::NewSessionRequest::new(workspace.path().to_path_buf()))
                .block_task()
                .await?
                .session_id;
            turn_with_gate(
                &connection,
                &mut updates,
                &session,
                "verified edit",
                Some(release),
            )
            .await?;
            turn(&connection, &mut updates, &session, "remembered").await?;
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("created.txt")).unwrap(),
        "edited-value\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("sentinel")).unwrap(),
        "untouched"
    );
    for _ in 0..9 {
        http.request();
    }
    http.assert_no_more_requests();
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct EchoInput {
    nested: Value,
}

#[tokio::test]
async fn native_mutation_failures_and_output_limits_reach_the_model_and_allow_recovery() {
    let isolated = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("note.txt");
    std::fs::write(&path, "original").unwrap();
    std::fs::write(
        workspace.path().join("giant.txt"),
        "x".repeat(bee::tool::MAX_OUTPUT_BYTES + 1),
    )
    .unwrap();
    let inspected = path.clone();
    let mut step = 0;
    let http = Server::respond(5, move |request| {
        let response = match step {
            0 => call(
                "protected",
                "write",
                json!({"path":"note.txt","content":"unapproved"}),
            ),
            1 => {
                assert_eq!(result(request, "protected")["error"]["code"], "execution");
                assert_eq!(std::fs::read_to_string(&inspected).unwrap(), "original");
                call("large", "read", json!({"path":"giant.txt"}))
            }
            2 => {
                assert_eq!(result(request, "large")["error"]["code"], "output_limit");
                call("small", "read", json!({"path":"note.txt"}))
            }
            3 => {
                assert_eq!(result(request, "small")["data"]["content"], "1: original\n");
                call(
                    "corrected",
                    "write",
                    json!({"path":"note.txt","content":"corrected","overwrite":true}),
                )
            }
            4 => {
                assert_eq!(
                    result(request, "corrected"),
                    json!({"data":{"bytes_written":9}})
                );
                support::chat_text("recovered")
            }
            _ => unreachable!(),
        };
        step += 1;
        Response::json(response)
    });
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        TIMEOUT,
        client.connect_with(subprocess(&isolated, &http.url), async |connection| {
            initialize(&connection).await?;
            let session = connection
                .send_request(v2::NewSessionRequest::new(workspace.path().to_path_buf()))
                .block_task()
                .await?
                .session_id;
            turn(&connection, &mut updates, &session, "recovered").await
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "corrected");
    for _ in 0..5 {
        http.request();
    }
    http.assert_no_more_requests();
}

#[tokio::test]
async fn the_binary_calls_a_client_hosted_mcp_endpoint_and_retains_its_result() {
    let isolated = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let arguments = json!({"nested":{"items":["héllo 🐝",true,null,42]}});
    let expected = arguments.clone();
    let mut step = 0;
    let http = Server::respond(3, move |request| {
        step += 1;
        if step == 1 {
            let tools = request.body["tools"].as_array().unwrap();
            assert!(tools.iter().any(|tool| tool["function"]["name"] == "read"));
            let name = tools
                .iter()
                .find_map(|tool| {
                    tool["function"]["name"]
                        .as_str()
                        .filter(|name| name.starts_with("mcp_"))
                })
                .unwrap();
            Response::json(call("mcp-call", name, arguments.clone()))
        } else {
            assert_eq!(
                result(request, "mcp-call")["result"]["structuredContent"],
                arguments
            );
            Response::json(support::chat_text("MCP verified"))
        }
    });
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::clone(&seen);
    let server = McpServer::<Agent>::builder("e2e-tools")
        .tool_fn(
            "echo",
            "Echo structured data",
            async move |input: EchoInput, _| {
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
        TIMEOUT,
        client.connect_with(subprocess(&isolated, &http.url), async |connection| {
            initialize(&connection).await?;
            let session = connection
                .build_session(workspace.path())
                .with_mcp_server(server)?
                .start_session()
                .block_task()
                .await?
                .response()
                .session_id
                .clone();
            turn(&connection, &mut updates, &session, "MCP verified").await?;
            turn(&connection, &mut updates, &session, "MCP verified").await?;
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![expected]);
    for _ in 0..3 {
        http.request();
    }
    http.assert_no_more_requests();
}
