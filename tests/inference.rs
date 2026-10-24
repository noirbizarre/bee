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

fn native_call(protocol: &str, id: &str, name: &str, arguments: Value) -> Value {
    match protocol {
        "openai-chat" => {
            json!({"choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]})
        }
        "openai-responses" => {
            json!({"status":"completed","output":[{"id":format!("item-{id}"),"type":"function_call","call_id":id,"name":name,"arguments":arguments.to_string()}]})
        }
        "anthropic-messages" => {
            json!({"type":"message","role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":arguments}],"stop_reason":"tool_use"})
        }
        _ => unreachable!(),
    }
}

/// Drive ordinary ACP prompts, retaining each session's history between prompts.
async fn native_turns(
    profiles: bee::provider::Providers,
    runtime: bee::acp::Runtime,
    sessions: Vec<(std::path::PathBuf, Vec<bool>)>,
) {
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
            bee::acp::agent_with_runtime(profiles, runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                for (cwd, successes) in sessions {
                    let session = connection
                        .send_request(v2::NewSessionRequest::new(cwd))
                        .block_task()
                        .await?
                        .session_id;
                    for (index, success) in successes.into_iter().enumerate() {
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
                                if success {
                                    assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                                } else {
                                    assert!(matches!(
                                        idle.stop_reason,
                                        Some(v2::StopReason::Error(_))
                                    ));
                                }
                                break;
                            }
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
}

#[rstest::rstest]
#[case("openai-chat")]
#[case("openai-responses")]
#[case("anthropic-messages")]
#[tokio::test]
async fn acp_executes_shipped_tools_in_the_session_directory_and_retains_results(
    #[case] protocol: &str,
) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note.txt"), "session-specific-content\n").unwrap();
    let server = Server::start(vec![
        Response::json(native_call(
            protocol,
            "a",
            "glob",
            json!({"pattern":"*.txt"}),
        )),
        Response::json(native_call(protocol, "b", "list", json!({"path":"."}))),
        Response::json(native_call(
            protocol,
            "c",
            "read",
            json!({"path":"note.txt"}),
        )),
        Response::json(native_call(
            protocol,
            "d",
            "write",
            json!({"path":"created.txt","content":"before-edit\n"}),
        )),
        Response::json(native_call(
            protocol,
            "e",
            "edit",
            json!({"path":"created.txt","old_text":"before-edit","new_text":"after-edit"}),
        )),
        Response::json(native_call(
            protocol,
            "f",
            "read",
            json!({"path":"created.txt"}),
        )),
        Response::json(native_call(
            protocol,
            "g",
            "bash",
            json!({"command":"printf '%s' \"$(<created.txt)\"; printf bash-stderr >&2; exit 7"}),
        )),
        Response::json(answer(protocol, "done")),
        Response::json(answer(protocol, "remembered")),
    ]);
    native_turns(
        providers(protocol, &server.url, ""),
        bee::acp::Runtime::new(
            bee::agent::Limits::default(),
            bee::tool::builtins_with_bash(support::bash_settings()).unwrap(),
        ),
        vec![(root.path().into(), vec![true, true])],
    )
    .await;
    let first = server.request();
    let names: Vec<_> = first.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| {
            if protocol == "openai-chat" {
                tool["function"]["name"].as_str().unwrap()
            } else {
                tool["name"].as_str().unwrap()
            }
        })
        .collect();
    assert_eq!(names, ["bash", "edit", "glob", "list", "read", "write"]);
    for id in ["a", "b"] {
        let body = server.request().body.to_string();
        assert!(body.contains("note.txt"));
        assert!(body.contains(id));
    }
    let read_result = server.request();
    assert!(
        read_result
            .body
            .to_string()
            .contains("session-specific-content")
    );
    for id in ["d", "e"] {
        let body = server.request().body.to_string();
        assert!(body.contains("bytes_written"));
        assert!(body.contains(id));
    }
    let final_request = server.request();
    assert!(final_request.body.to_string().contains("after-edit"));
    let bash_result = server.request().body.to_string();
    assert!(bash_result.contains("after-edit"));
    assert!(bash_result.contains("bash-stderr"));
    assert!(bash_result.contains("exit_code"));
    assert!(bash_result.contains('g'));
    let next = server.request();
    assert!(next.body.to_string().contains("session-specific-content"));
    assert!(next.body.to_string().contains("after-edit"));
    assert!(next.body.to_string().contains("question 1"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("created.txt")).unwrap(),
        "after-edit\n"
    );
}

#[tokio::test]
async fn separate_acp_sessions_resolve_the_same_relative_path_independently() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(first.path().join("note.txt"), "first-directory").unwrap();
    std::fs::write(second.path().join("note.txt"), "second-directory").unwrap();
    let server = Server::start(vec![
        Response::json(native_call(
            "openai-chat",
            "a",
            "read",
            json!({"path":"note.txt"}),
        )),
        Response::json(chat_text("done")),
        Response::json(native_call(
            "openai-chat",
            "b",
            "read",
            json!({"path":"note.txt"}),
        )),
        Response::json(chat_text("done")),
    ]);
    native_turns(
        providers("openai-chat", &server.url, ""),
        bee::acp::Runtime::builtins(bee::agent::Limits::default()).unwrap(),
        vec![
            (first.path().into(), vec![true]),
            (second.path().into(), vec![true]),
        ],
    )
    .await;
    server.request();
    let first = server.request().body.to_string();
    server.request();
    let second = server.request().body.to_string();
    assert!(first.contains("first-directory"));
    assert!(!first.contains("second-directory"));
    assert!(second.contains("second-directory"));
    assert!(!second.contains("first-directory"));
}

#[rstest::rstest]
#[case::provider_failure(false)]
#[case::limit_exhaustion(true)]
#[tokio::test]
async fn failed_tool_turns_do_not_enter_the_next_prompt_context(#[case] limit: bool) {
    let mut failure = Response::json(json!({"error":"failed"}));
    failure.status = 503;
    let mut responses = vec![Response::json(native_call(
        "openai-chat",
        "a",
        "echo",
        json!({"text":"discarded-result"}),
    ))];
    if !limit {
        responses.push(failure);
    }
    responses.push(Response::json(chat_text("recovered")));
    let server = Server::start(responses);
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::new(if limit { 1 } else { 3 }, 2).unwrap(),
        support::native_registry(),
    );
    native_turns(
        providers("openai-chat", &server.url, ""),
        runtime,
        vec![(std::env::temp_dir(), vec![false, true])],
    )
    .await;
    server.request();
    if !limit {
        assert!(
            server
                .request()
                .body
                .to_string()
                .contains("discarded-result")
        );
    }
    let replacement = server.request();
    assert_eq!(
        replacement.body["messages"],
        json!([{"role":"user","content":"question 1"}])
    );
    server.assert_no_more_requests();
}

#[tokio::test]
async fn ordinary_native_failures_are_returned_to_the_model_for_recovery() {
    let server = Server::start(vec![
        Response::json(native_call(
            "anthropic-messages",
            "a",
            "echo",
            json!({"text":"fail"}),
        )),
        Response::json(answer("anthropic-messages", "recovered")),
    ]);
    native_turns(
        providers("anthropic-messages", &server.url, ""),
        bee::acp::Runtime::new(bee::agent::Limits::default(), support::native_registry()),
        vec![(std::env::temp_dir(), vec![true])],
    )
    .await;
    server.request();
    let result = server.request();
    assert_eq!(result.body["messages"][2]["content"][0]["is_error"], true);
    assert!(result.body.to_string().contains("Choose different text."));
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

struct PendingNative {
    started: tokio::sync::mpsc::UnboundedSender<tokio_util::sync::CancellationToken>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

struct DropSignal(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl bee::tool::Tool for PendingNative {
    type Input = Empty;
    type Output = Value;
    fn name(&self) -> &str {
        "pending"
    }
    fn description(&self) -> &str {
        "Wait for cancellation"
    }
    async fn execute(
        &self,
        _: Empty,
        context: &bee::tool::Context,
    ) -> Result<Value, bee::tool::Failure> {
        let _guard = DropSignal(self.dropped.clone());
        self.started.send(context.cancellation().clone()).unwrap();
        std::future::pending().await
    }
}

#[rstest::rstest]
#[case::cancel(false, false)]
#[case::close_and_resume(true, false)]
#[cfg_attr(unix, case::bash_cancel(false, true))]
#[cfg_attr(unix, case::bash_close_and_resume(true, true))]
#[tokio::test]
async fn acp_cancellation_drops_native_work_and_allows_a_clean_replacement(
    #[case] close: bool,
    #[case] bash: bool,
) {
    let root = tempfile::tempdir().unwrap();
    let server = Server::start(vec![
        Response::json(native_call(
            "openai-chat",
            "a",
            if bash { "bash" } else { "pending" },
            if bash {
                json!({"command":"(printf ready > ready; sleep 0.4; printf leaked > leaked) & wait"})
            } else {
                json!({})
            },
        )),
        Response::json(chat_text("replacement")),
    ]);
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut registry = bee::tool::Registry::default();
    registry
        .register(PendingNative {
            started,
            dropped: dropped.clone(),
        })
        .unwrap();
    if bash {
        registry.register(bee::tool::bash::Bash::default()).unwrap();
    }
    let runtime = bee::acp::Runtime::new(bee::agent::Limits::default(), registry);
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _connection| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let profiles = providers("openai-chat", &server.url, "");
    tokio::time::timeout(
        Duration::from_secs(10),
        client.connect_with(
            bee::acp::agent_with_runtime(profiles, runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let cwd = root.path().to_path_buf();
                let session = connection
                    .send_request(v2::NewSessionRequest::new(cwd.clone()))
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
                // Wait for actual native execution, not just HTTP acceptance.
                let token = if bash {
                    while !root.path().join("ready").exists() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    None
                } else {
                    let token = starts.recv().await.unwrap();
                    assert!(!token.is_cancelled());
                    Some(token)
                };
                if close {
                    connection
                        .send_request(v2::CloseSessionRequest::new(session.clone()))
                        .block_task()
                        .await?;
                } else {
                    connection
                        .send_notification(v2::CancelSessionNotification::new(session.clone()))?;
                }
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
                if let Some(token) = token {
                    token.cancelled().await;
                    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
                } else {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    assert!(!root.path().join("leaked").exists());
                }
                if close {
                    connection
                        .send_request(v2::ResumeSessionRequest::new(session.clone(), cwd))
                        .block_task()
                        .await?;
                    // Resume reports current state independently of turn completion.
                    loop {
                        if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                            updates.recv().await.unwrap().update
                        {
                            assert_eq!(idle.stop_reason, None);
                            break;
                        }
                    }
                }
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["replacement".into()]))
                    .block_task()
                    .await?;
                loop {
                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                        updates.recv().await.unwrap().update
                    {
                        // Any duplicate cancelled terminal update would fail this assertion.
                        assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                        break;
                    }
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    server.request();
    assert_eq!(
        server.request().body["messages"],
        json!([{"role":"user","content":"replacement"}])
    );
    server.assert_no_more_requests();
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
