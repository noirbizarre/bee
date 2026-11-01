//! Production parent/model/independent-child cycles and bounded ownership.

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agent_client_protocol::{Agent, Client, Error, Responder, V2ConnectionTo, schema::v2};
use bee::config::{Config, Environment};
use serde_json::{Value, json};
use support::{Response, Server};

fn fixture() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let target = root.join("target/acp-client-fixture");
        let output = std::process::Command::new(env!("CARGO"))
            .args(["build", "--offline", "--locked", "--manifest-path"])
            .arg(root.join("tests/fixtures/acp-agent/Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        target.join("debug").join(format!(
            "bee-acp-client-fixture{}",
            std::env::consts::EXE_SUFFIX
        ))
    })
}

fn config(root: &Path, text: &str) -> (Config, Environment) {
    let path = root.join("settings.toml");
    std::fs::write(&path, text).unwrap();
    let environment = Environment::new(None, None, Some(path));
    (Config::load(&environment, root).unwrap(), environment)
}

fn agent(name: &str, mode: &str, root: &Path) -> String {
    let quote = |p: &Path| serde_json::to_string(&p.display().to_string()).unwrap();
    format!(
        "[agents.{name}]\ncommand={}\nargs=['{mode}']\n[agents.{name}.env]\nFIXTURE_PID={}\nFIXTURE_PROMPT={}\nFIXTURE_CLOSED={}\nFIXTURE_DEPTH={}\nFIXTURE_RELEASE={}\nBEE_DELEGATION_REMAINING_DEPTH='64'\n",
        quote(fixture()),
        quote(&root.join(format!("{name}.pid"))),
        quote(&root.join(format!("{name}.prompt"))),
        quote(&root.join(format!("{name}.closed"))),
        quote(&root.join(format!("{name}.depth"))),
        quote(&root.join(format!("{name}.release")))
    )
}

fn runtime(config: &Config, environment: &Environment) -> bee::acp::Runtime {
    bee::acp::Runtime::new(Default::default(), bee::tool::Registry::default())
        .with_permissions(bee::permissions::Settings::from_config(config, environment).unwrap())
        .with_delegation(
            bee::acp::client::config::Agents::from_config(config).unwrap(),
            bee::delegation::Settings::from_config(config)
                .unwrap()
                .with_environment(environment)
                .unwrap(),
        )
}

fn task(id: &str, name: &str) -> Value {
    json!({"id":id,"agent":name,"task":"Inspect the parser","context":"Only malformed input"})
}

fn tool_call(protocol: &str, arguments: Value) -> Value {
    match protocol {
        "openai-chat" => {
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"call-1","type":"function","function":{"name":"delegate","arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]})
        }
        "openai-responses" => {
            json!({"status":"completed","output":[{"type":"function_call","id":"item-1","call_id":"call-1","name":"delegate","arguments":arguments.to_string()}]})
        }
        _ => {
            json!({"type":"message","role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"delegate","input":arguments}],"stop_reason":"tool_use"})
        }
    }
}

fn answer(protocol: &str) -> Value {
    match protocol {
        "openai-chat" => support::chat_text("synthesized"),
        "openai-responses" => {
            json!({"status":"completed","output":[{"type":"message","id":"message","role":"assistant","content":[{"type":"output_text","text":"synthesized"}]}]})
        }
        _ => {
            json!({"type":"message","role":"assistant","content":[{"type":"text","text":"synthesized"}],"stop_reason":"end_turn"})
        }
    }
}

fn tool_result(protocol: &str, body: &Value) -> Value {
    let text = match protocol {
        "openai-chat" => body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap(),
        "openai-responses" => body["input"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["type"] == "function_call_output")
            .unwrap()["output"]
            .as_str()
            .unwrap(),
        _ => body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .find(|m| m["type"] == "tool_result")
            .unwrap()["content"]
            .as_str()
            .unwrap(),
    };
    serde_json::from_str(text).unwrap()
}

async fn initialize(connection: &V2ConnectionTo<Agent>, capabilities: Value) -> Result<(), Error> {
    connection.send_request(serde_json::from_value::<v2::InitializeRequest>(json!({"protocolVersion":2,"info":{"name":"delegation-test","version":"0"},"capabilities":capabilities})).unwrap()).block_task().await?;
    Ok(())
}

async fn start(connection: &V2ConnectionTo<Agent>, cwd: &Path) -> Result<v2::SessionId, Error> {
    let id = connection
        .send_request(v2::NewSessionRequest::new(cwd.to_path_buf()))
        .block_task()
        .await?
        .session_id;
    connection
        .send_request(v2::PromptRequest::new(
            id.clone(),
            vec!["PRIVATE_PARENT_CONVERSATION".into()],
        ))
        .block_task()
        .await?;
    Ok(id)
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

async fn exists(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[rstest::rstest]
#[case("openai-chat")]
#[case("openai-responses")]
#[case("anthropic-messages")]
#[tokio::test]
async fn independent_delegates_receive_only_explicit_context_and_feed_parent_synthesis(
    #[case] protocol: &'static str,
) {
    let root = tempfile::tempdir().unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\nenabled=true\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n{}{}",
            agent("first", "normal", root.path()),
            agent("second", "normal", root.path())
        ),
    );
    let http = Server::start(vec![
        Response::json(tool_call(
            protocol,
            json!({"tasks":[task("a", "first"),task("b", "second")]}),
        )),
        Response::json(answer(protocol)),
    ]);
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            let _ = send.send(update);
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let cwd = root.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers(protocol, &http.url, ""),
                runtime(&config, &environment),
            ),
            async move |connection| {
                initialize(&connection, json!({})).await?;
                let session = start(&connection, &cwd).await?;
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::EndTurn)
                );
                let pid = std::fs::read_to_string(cwd.join("first.pid")).unwrap();
                connection
                    .send_request(v2::CloseSessionRequest::new(session.clone()))
                    .block_task()
                    .await?;
                let mut resume = v2::ResumeSessionRequest::new(session, &cwd);
                resume.replay_from = Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new()));
                connection.send_request(resume).block_task().await?;
                let mut retained = false;
                loop {
                    let update = updates.recv().await.unwrap().update;
                    if serde_json::to_value(&update).unwrap()["rawOutput"]["results"][0]["id"]
                        == "a"
                    {
                        retained = true;
                    }
                    if matches!(
                        update,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                    ) {
                        break;
                    }
                }
                assert!(retained, "delegation result is replayed as presentation");
                assert_eq!(
                    std::fs::read_to_string(cwd.join("first.pid")).unwrap(),
                    pid,
                    "replay never launches children"
                );
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let first = http.request().body;
    assert!(first.to_string().contains("delegate"));
    assert!(first.to_string().contains("second"));
    let result = tool_result(protocol, &http.request().body);
    assert_eq!(result["results"][0]["id"], "a");
    assert_eq!(result["results"][1]["id"], "b");
    for child in ["first", "second"] {
        let prompt = std::fs::read_to_string(root.path().join(format!("{child}.prompt"))).unwrap();
        assert!(prompt.contains("Inspect the parser"));
        assert!(prompt.contains("Only malformed input"));
        assert!(!prompt.contains("PRIVATE_PARENT_CONVERSATION"));
        assert_eq!(
            std::fs::read_to_string(root.path().join(format!("{child}.depth"))).unwrap(),
            "0"
        );
        assert!(root.path().join(format!("{child}.closed")).exists());
    }
    assert_eq!(result["results"][0]["status"], "completed");
    assert_eq!(
        result["results"][0]["messages"][0]["content"][0]["text"],
        "replacement"
    );
}

#[rstest::rstest]
#[case("", None, "normal")]
#[case("enabled=true", Some(0), "normal")]
#[case("enabled=true", None, "reject")]
#[case("enabled=true\ntimeout_seconds=1", None, "partial-hang")]
#[case("enabled=true", None, "large")]
#[case("enabled=true", None, "remote-error")]
#[case("enabled=true", None, "refusal")]
#[case("enabled=true", None, "partial-eof")]
#[case("enabled=true", None, "ignore-close")]
#[case("enabled=true", None, "form")]
#[case("enabled=true", None, "url")]
#[tokio::test]
async fn disabled_depth_and_child_failures_are_bounded_and_actionable(
    #[case] settings: &str,
    #[case] depth: Option<usize>,
    #[case] mode: &str,
) {
    let root = tempfile::tempdir().unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\n{settings}\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n{}",
            agent("child", mode, root.path())
        ),
    );
    let policy = bee::delegation::Settings::from_config(&config).unwrap();
    let policy = depth.map_or(policy, |d| policy.with_remaining_depth(d));
    let runtime = runtime(&config, &environment).with_delegation(
        bee::acp::client::config::Agents::from_config(&config).unwrap(),
        policy,
    );
    let http = Server::start(vec![
        Response::json(tool_call(
            "openai-chat",
            json!({"tasks":[task("a", "child")]}),
        )),
        Response::json(answer("openai-chat")),
    ]);
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            let _ = send.send(update);
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let cwd = root.path().to_path_buf();
    let disabled = settings.is_empty() || depth == Some(0);
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(support::providers("openai-chat", &http.url, ""), runtime),
            async move |connection| {
                initialize(&connection, json!({})).await?;
                start(&connection, &cwd).await?;
                let stop = idle(&mut updates).await.stop_reason;
                if disabled {
                    assert!(matches!(stop, Some(v2::StopReason::Error(_))));
                } else {
                    assert_eq!(stop, Some(v2::StopReason::EndTurn));
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let first = http.request().body;
    let advertised = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["function"]["name"] == "delegate");
    assert_eq!(advertised, !disabled);
    if disabled {
        assert!(!root.path().join("child.pid").exists());
        return;
    }
    let result = tool_result("openai-chat", &http.request().body);
    assert!(!result.to_string().contains("SECRET_"));
    if mode == "partial-hang" || mode == "ignore-close" {
        assert_eq!(result["results"][0]["status"], "timed_out");
        assert_eq!(
            result["results"][0]["messages"][0]["content"][0]["text"],
            "replacement"
        );
    } else if mode == "refusal" {
        assert_eq!(result["results"][0]["status"], "refused");
    } else {
        assert_eq!(result["results"][0]["status"], "failed");
    }
    if mode == "partial-eof" {
        assert_eq!(
            result["results"][0]["messages"][0]["content"][0]["text"],
            "replacement"
        );
    }
    if mode == "large" {
        assert_eq!(result["results"][0]["truncated"], true);
    }
    assert!(result.to_string().len() <= bee::tool::MAX_OUTPUT_BYTES);
    if mode != "ignore-close" && mode != "partial-eof" {
        assert!(root.path().join("child.closed").exists());
    }
}

#[tokio::test]
async fn child_count_is_shared_across_calls_and_reset_for_the_next_turn() {
    let root = tempfile::tempdir().unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\nenabled=true\nmax_children=1\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n{}",
            agent("child", "normal", root.path())
        ),
    );
    let call = || {
        Response::json(tool_call(
            "openai-chat",
            json!({"tasks":[task("a", "child")]}),
        ))
    };
    let mut second = tool_call("openai-chat", json!({"tasks":[task("b", "child")]}));
    second["choices"][0]["message"]["tool_calls"][0]["id"] = json!("call-2");
    let http = Server::start(vec![
        call(),
        Response::json(second),
        Response::json(answer("openai-chat")),
        call(),
        Response::json(answer("openai-chat")),
    ]);
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            let _ = send.send(update);
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let cwd = root.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                runtime(&config, &environment),
            ),
            async move |connection| {
                initialize(&connection, json!({})).await?;
                let id = start(&connection, &cwd).await?;
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::EndTurn)
                );
                connection
                    .send_request(v2::PromptRequest::new(id, vec!["next".into()]))
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
    http.request();
    assert_eq!(
        tool_result("openai-chat", &http.request().body)["results"][0]["status"],
        "completed"
    );
    assert_eq!(
        tool_result("openai-chat", &http.request().body)["error"]["code"],
        "child_limit"
    );
    http.request();
    assert_eq!(
        tool_result("openai-chat", &http.request().body)["results"][0]["status"],
        "completed"
    );
}

#[tokio::test]
async fn sibling_tasks_overlap_and_results_keep_input_order() {
    let root = tempfile::tempdir().unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\nenabled=true\nmax_concurrent=2\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n{}{}",
            agent("first", "barrier", root.path()),
            agent("second", "barrier", root.path())
        ),
    );
    let http = Server::start(vec![
        Response::json(tool_call(
            "openai-chat",
            json!({"tasks":[task("a", "first"),task("b", "second")]}),
        )),
        Response::json(answer("openai-chat")),
    ]);
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            let _ = send.send(update);
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let cwd = root.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                runtime(&config, &environment),
            ),
            async move |connection| {
                initialize(&connection, json!({})).await?;
                start(&connection, &cwd).await?;
                // Both children must reach their prompt barrier before either can complete.
                exists(&cwd.join("first.prompt")).await;
                exists(&cwd.join("second.prompt")).await;
                std::fs::write(cwd.join("second.release"), "go").unwrap();
                exists(&cwd.join("second.closed")).await;
                assert!(!cwd.join("first.closed").exists());
                std::fs::write(cwd.join("first.release"), "go").unwrap();
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
    http.request();
    let result = tool_result("openai-chat", &http.request().body);
    assert_eq!(result["results"][0]["id"], "a");
    assert_eq!(result["results"][1]["id"], "b");
    assert_eq!(result["results"][0]["stop_reason"], "end_turn");
}

#[tokio::test]
async fn queued_children_time_out_without_launching_after_capacity_is_exhausted() {
    let root = tempfile::tempdir().unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\nenabled=true\nmax_concurrent=1\ntimeout_seconds=1\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n{}{}",
            agent("first", "partial-hang", root.path()),
            agent("second", "normal", root.path())
        ),
    );
    let http = Server::start(vec![
        Response::json(tool_call(
            "openai-chat",
            json!({"tasks":[task("a", "first"),task("b", "second")]}),
        )),
        Response::json(answer("openai-chat")),
    ]);
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            let _ = send.send(update);
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let cwd = root.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                runtime(&config, &environment),
            ),
            async move |connection| {
                initialize(&connection, json!({})).await?;
                start(&connection, &cwd).await?;
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::EndTurn)
                );
                assert!(cwd.join("first.closed").exists());
                assert!(!cwd.join("second.pid").exists());
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    http.request();
    let result = tool_result("openai-chat", &http.request().body);
    assert_eq!(result["results"][0]["status"], "timed_out");
    assert_eq!(result["results"][1]["status"], "timed_out");
}

#[rstest::rstest]
#[case(false, "partial-hang")]
#[case(true, "partial-hang")]
#[case(false, "hang-init")]
#[case(true, "ignore-cancel")]
#[tokio::test]
async fn cancelling_or_closing_the_parent_reaps_children_before_terminal_idle(
    #[case] close: bool,
    #[case] mode: &str,
) {
    let root = tempfile::tempdir().unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\nenabled=true\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n{}",
            agent("child", mode, root.path())
        ),
    );
    let http = Server::start(vec![Response::json(tool_call(
        "openai-chat",
        json!({"tasks":[task("a", "child")]}),
    ))]);
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            let _ = send.send(update);
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let cwd = root.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                runtime(&config, &environment),
            ),
            async move |connection| {
                initialize(&connection, json!({})).await?;
                let id = start(&connection, &cwd).await?;
                exists(&cwd.join(if mode == "hang-init" {
                    "child.pid"
                } else {
                    "child.prompt"
                }))
                .await;
                if close {
                    connection
                        .send_request(v2::CloseSessionRequest::new(id))
                        .block_task()
                        .await?;
                } else {
                    connection.send_notification(v2::CancelSessionNotification::new(id))?;
                }
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::Cancelled)
                );
                assert!(
                    mode == "hang-init" || cwd.join("child.closed").exists(),
                    "cleanup precedes idle and close response"
                );
                #[cfg(unix)]
                {
                    let pid: i32 = std::fs::read_to_string(cwd.join("child.pid"))
                        .unwrap()
                        .parse()
                        .unwrap();
                    assert!(
                        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
                        "child is reaped before terminal idle"
                    );
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

#[rstest::rstest]
#[case("permission")]
#[case("form")]
#[case("form-setup")]
#[case("url")]
#[tokio::test]
async fn child_interactions_preserve_options_schemas_and_namespaced_url_completion(
    #[case] mode: &str,
) {
    let root = tempfile::tempdir().unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\nenabled=true\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n{}{}",
            agent("first", mode, root.path()),
            agent("second", mode, root.path())
        ),
    );
    let http = Server::start(vec![
        Response::json(tool_call(
            "openai-chat",
            json!({"tasks":[task("a", "first"),task("b", "second")]}),
        )),
        Response::json(answer("openai-chat")),
    ]);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let complete = seen.clone();
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                let _ = send.send(update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: v2::RequestPermissionRequest,
                        responder: Responder<v2::RequestPermissionResponse>,
                        _| {
                assert_eq!(request.options[0].option_id.to_string(), "yes");
                assert_eq!(request.options[1].option_id.to_string(), "no");
                responder.respond(v2::RequestPermissionResponse::new(
                    v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(
                        "yes",
                    )),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::CreateElicitationRequest,
                        responder: Responder<v2::CreateElicitationResponse>,
                        _| {
                assert!(matches!(request.scope(), v2::ElicitationScope::Session(_)));
                let response = match request.mode {
                    v2::ElicitationMode::Form(form) => {
                        assert!(
                            serde_json::to_value(form.requested_schema)
                                .unwrap()
                                .to_string()
                                .contains("count")
                        );
                        json!({"action":"accept","content":{"count":2}})
                    }
                    v2::ElicitationMode::Url(url) => {
                        assert!(url.elicitation_id.to_string().starts_with("bee-delegate-"));
                        captured
                            .lock()
                            .unwrap()
                            .push(("create", url.elicitation_id.to_string()));
                        json!({"action":"accept"})
                    }
                    _ => panic!("unsupported request"),
                };
                responder.respond(serde_json::from_value(response).unwrap())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: v2::CompleteElicitationNotification, _| {
                complete
                    .lock()
                    .unwrap()
                    .push(("complete", notification.elicitation_id.to_string()));
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    let cwd = root.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                runtime(&config, &environment),
            ),
            async move |connection| {
                initialize(&connection, json!({"elicitation":{"form":{},"url":{}}})).await?;
                start(&connection, &cwd).await?;
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
    http.request();
    let result = tool_result("openai-chat", &http.request().body);
    assert_eq!(result["results"][0]["status"], "completed", "{result}");
    assert_eq!(result["results"][1]["status"], "completed", "{result}");
    if mode == "url" {
        let seen = seen.lock().unwrap();
        let created: std::collections::BTreeSet<_> = seen
            .iter()
            .filter(|(kind, _)| *kind == "create")
            .map(|(_, id)| id.clone())
            .collect();
        let completed: std::collections::BTreeSet<_> = seen
            .iter()
            .filter(|(kind, _)| *kind == "complete")
            .map(|(_, id)| id.clone())
            .collect();
        assert_eq!(created.len(), 2);
        assert_eq!(created, completed);
    }
}

#[test]
fn inherited_authority_is_nonincreasing_and_invalid_settings_are_value_redacted() {
    let root = tempfile::tempdir().unwrap();
    for value in [
        "enabled='SECRET'",
        "max_children=0",
        "max_concurrent=65",
        "timeout_seconds=-1",
        "max_depth=65",
        "SECRET=1",
    ] {
        let (config, _) = config(root.path(), &format!("[delegation]\n{value}"));
        let error = bee::delegation::Settings::from_config(&config).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("SECRET"));
    }
    for value in ["", "-1", " 1", "+1", "SECRET", "99999999999999999999999999"] {
        let env = Environment::default().with_variable(bee::delegation::REMAINING_DEPTH, value);
        let error = bee::delegation::Settings::default()
            .with_environment(&env)
            .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("SECRET"));
    }
}

#[rstest::rstest]
#[case(1)]
#[case(2)]
#[tokio::test]
async fn real_bee_children_enforce_inherited_depth_despite_configuration_and_environment_overrides(
    #[case] depth: usize,
) {
    let root = tempfile::tempdir().unwrap();
    let nested = json!({"tasks":[task("nested", "grandchild")]});
    let mut responses = vec![Response::json(tool_call("openai-chat", nested))];
    if depth == 2 {
        responses.push(Response::json(answer("openai-chat")));
    }
    let child_http = Server::start(responses);
    let child_config = root.path().join("child.toml");
    std::fs::write(&child_config, format!("model='test/private-model'\n[accounts.test]\nprovider='custom'\nmodels=['private-model']\ndiscovery=false\n[accounts.test.protocols.openai-chat]\nbase_url='{}'\n[delegation]\nenabled=true\nmax_depth=64\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n{}", child_http.url, agent("grandchild", "normal", root.path()))).unwrap();
    let quote = |p: &Path| serde_json::to_string(&p.display().to_string()).unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\nenabled=true\nmax_depth={depth}\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n[agents.child]\ncommand={}\n[agents.child.env]\nBEE_CONFIG={}\nBEE_PROFILE=''\nHOME={}\nXDG_CONFIG_HOME={}\nXDG_DATA_HOME={}\nXDG_STATE_HOME={}\nBEE_DELEGATION_REMAINING_DEPTH='64'\n",
            quote(Path::new(env!("CARGO_BIN_EXE_bee"))),
            quote(&child_config),
            quote(root.path()),
            quote(&root.path().join("config")),
            quote(&root.path().join("data")),
            quote(&root.path().join("state"))
        ),
    );
    let http = Server::start(vec![
        Response::json(tool_call(
            "openai-chat",
            json!({"tasks":[task("a", "child")]}),
        )),
        Response::json(answer("openai-chat")),
    ]);
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            let _ = send.send(update);
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let cwd = root.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                runtime(&config, &environment),
            ),
            async move |connection| {
                initialize(&connection, json!({})).await?;
                start(&connection, &cwd).await?;
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::EndTurn)
                );
                assert_eq!(cwd.join("grandchild.pid").exists(), depth == 2);
                if depth == 2 {
                    assert_eq!(
                        std::fs::read_to_string(cwd.join("grandchild.depth")).unwrap(),
                        "0"
                    );
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let request = child_http.request().body;
    assert_eq!(
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "delegate"),
        depth == 2
    );
    http.request();
    assert_eq!(
        tool_result("openai-chat", &http.request().body)["results"][0]["status"],
        if depth == 2 { "completed" } else { "failed" }
    );
}

#[tokio::test]
async fn resolving_one_child_permission_preserves_the_other_outstanding_wait() {
    let root = tempfile::tempdir().unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\nenabled=true\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n{}{}",
            agent("first", "permission", root.path()),
            agent("second", "permission", root.path())
        ),
    );
    let http = Server::start(vec![
        Response::json(tool_call(
            "openai-chat",
            json!({"tasks":[task("a", "first"),task("b", "second")]}),
        )),
        Response::json(answer("openai-chat")),
    ]);
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let (requests, mut pending) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                let _ = send.send(update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: v2::RequestPermissionRequest,
                        responder: Responder<v2::RequestPermissionResponse>,
                        _| {
                let _ = requests.send((request, responder));
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        );
    let cwd = root.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                runtime(&config, &environment),
            ),
            async move |connection| {
                initialize(&connection, json!({})).await?;
                start(&connection, &cwd).await?;
                let a = pending.recv().await.unwrap();
                let b = pending.recv().await.unwrap();
                let (first, second) = if a.0.title.contains("first") {
                    (a, b)
                } else {
                    (b, a)
                };
                while updates.try_recv().is_ok() {}
                let approve = || {
                    v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Selected(
                        v2::SelectedPermissionOutcome::new("yes"),
                    ))
                };
                first.1.respond(approve())?;
                exists(&cwd.join("first.closed")).await;
                while let Ok(update) = updates.try_recv() {
                    assert!(
                        !matches!(
                            update.update,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
                        ),
                        "a sibling permission is still outstanding"
                    );
                }
                second.1.respond(approve())?;
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

#[tokio::test]
async fn default_ask_launch_rejection_never_starts_a_delegate() {
    let root = tempfile::tempdir().unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\nenabled=true\n{}",
            agent("child", "normal", root.path())
        ),
    );
    let http = Server::start(vec![
        Response::json(tool_call(
            "openai-chat",
            json!({"tasks":[task("a", "child")]}),
        )),
        Response::json(answer("openai-chat")),
    ]);
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _| {
                let _ = send.send(update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: v2::RequestPermissionRequest,
                        responder: Responder<v2::RequestPermissionResponse>,
                        _| {
                assert!(request.title.contains("child"));
                responder.respond(v2::RequestPermissionResponse::new(
                    v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(
                        "reject_once",
                    )),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        );
    let cwd = root.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                runtime(&config, &environment),
            ),
            async move |connection| {
                initialize(&connection, json!({})).await?;
                start(&connection, &cwd).await?;
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::EndTurn)
                );
                assert!(!cwd.join("child.pid").exists());
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    http.request();
    assert_eq!(
        tool_result("openai-chat", &http.request().body)["error"]["code"],
        "permission_denied"
    );
}

#[tokio::test]
async fn concurrent_parent_sessions_share_capacity_and_cancellation_releases_it_after_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let quote = |p: &Path| serde_json::to_string(&p.display().to_string()).unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\nenabled=true\nmax_concurrent=1\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n{}{}FIXTURE_REQUIRE_CLOSED={}\nFIXTURE_ORDER={}\n",
            agent("first", "partial-hang", root.path()),
            agent("second", "normal", root.path()),
            quote(&root.path().join("first.closed")),
            quote(&root.path().join("order"))
        ),
    );
    let http = Server::start(vec![
        Response::json(tool_call(
            "openai-chat",
            json!({"tasks":[task("a", "first")]}),
        )),
        Response::json(tool_call(
            "openai-chat",
            json!({"tasks":[task("b", "second")]}),
        )),
        Response::json(answer("openai-chat")),
    ]);
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            let _ = send.send(update);
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let cwd = root.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                runtime(&config, &environment),
            ),
            async move |connection| {
                initialize(&connection, json!({})).await?;
                let first = start(&connection, &cwd).await?;
                exists(&cwd.join("first.prompt")).await;
                let second = start(&connection, &cwd).await?;
                loop {
                    let update = updates.recv().await.unwrap();
                    if update.session_id == second
                        && serde_json::to_value(update.update).unwrap()["status"] == "in_progress"
                    {
                        break;
                    }
                }
                connection.send_notification(v2::CancelSessionNotification::new(first.clone()))?;
                let mut stopped = std::collections::BTreeSet::new();
                while stopped.len() < 2 {
                    let update = updates.recv().await.unwrap();
                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                        update.update
                    {
                        assert_eq!(
                            idle.stop_reason,
                            Some(if update.session_id == first {
                                v2::StopReason::Cancelled
                            } else {
                                v2::StopReason::EndTurn
                            })
                        );
                        stopped.insert(update.session_id.to_string());
                    }
                }
                assert_eq!(std::fs::read_to_string(cwd.join("order")).unwrap(), "true");
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

#[rstest::rstest]
#[case(json!({"tasks":[]}))]
#[case(json!({"tasks":[task("a","unknown")]}))]
#[case(json!({"tasks":[task("a","child"),task("a","child")]}))]
#[case(json!({"tasks":[task("a","child"),task("b","child")]}))]
#[case(json!({"tasks":[{"id":"a","agent":"child","task":"","context":"explicit"}]}))]
#[case(json!({"tasks":[{"id":"a","agent":"child","task":"inspect","context":null}]}))]
#[case(json!({"tasks":[{"id":"a","agent":"child","task":"inspect","history":"do not forward"}]}))]
#[case(json!({"tasks":[task("a","child")],"max_depth":64}))]
#[tokio::test]
async fn invalid_or_oversized_batches_have_no_child_process_effects(#[case] arguments: Value) {
    let root = tempfile::tempdir().unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\nenabled=true\nmax_children=1\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n{}",
            agent("child", "normal", root.path())
        ),
    );
    let http = Server::start(vec![
        Response::json(tool_call("openai-chat", arguments)),
        Response::json(answer("openai-chat")),
    ]);
    let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            let _ = send.send(update);
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let cwd = root.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                runtime(&config, &environment),
            ),
            async move |connection| {
                initialize(&connection, json!({})).await?;
                start(&connection, &cwd).await?;
                assert_eq!(
                    idle(&mut updates).await.stop_reason,
                    Some(v2::StopReason::EndTurn)
                );
                assert!(!cwd.join("child.pid").exists());
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    http.request();
    assert_eq!(
        tool_result("openai-chat", &http.request().body)["error"]["code"],
        "invalid_arguments"
    );
}

#[tokio::test]
async fn parent_disconnect_cancels_and_drains_owned_child_processes() {
    let root = tempfile::tempdir().unwrap();
    let (config, environment) = config(
        root.path(),
        &format!(
            "[delegation]\nenabled=true\n[[permissions]]\naction='delegate'\nresource='*'\neffect='allow'\n{}",
            agent("child", "partial-hang", root.path())
        ),
    );
    let http = Server::start(vec![Response::json(tool_call(
        "openai-chat",
        json!({"tasks":[task("a", "child")]}),
    ))]);
    let cwd = root.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(15),
        Client.v2().connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &http.url, ""),
                runtime(&config, &environment),
            ),
            async move |connection| {
                initialize(&connection, json!({})).await?;
                start(&connection, &cwd).await?;
                exists(&cwd.join("child.prompt")).await;
                // Returning closes the client transport while its parent turn is still waiting.
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    exists(&root.path().join("child.closed")).await;
    #[cfg(unix)]
    {
        let pid: i32 = std::fs::read_to_string(root.path().join("child.pid"))
            .unwrap()
            .parse()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("disconnect cleanup must reap the child");
    }
}
