//! Explicit compaction through the production ACP and native HTTP boundaries.

mod support;

use std::time::Duration;

use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Client, Error};
use serde_json::{Value, json};
use support::{Response, Server};

fn answer(protocol: &str, text: &str) -> Value {
    match protocol {
        "openai-chat" => support::chat_text(text),
        "openai-responses" => {
            json!({"status":"completed","output":[{"id":"m1","type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}]})
        }
        "anthropic-messages" => {
            json!({"type":"message","role":"assistant","content":[{"type":"text","text":text}],"stop_reason":"end_turn"})
        }
        _ => unreachable!(),
    }
}

fn tool_answer(protocol: &str, turn: &str) -> Value {
    let arguments = json!({"path":format!("{turn}.txt")});
    let signature = format!("{turn}-private-native");
    match protocol {
        "openai-chat" => {
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":format!("{turn}-call"),"type":"function","function":{"name":"read","arguments":arguments.to_string()},"extra_content":{"google":{"thought_signature":signature}}}]},"finish_reason":"tool_calls"}]})
        }
        "openai-responses" => {
            json!({"status":"completed","output":[{"id":format!("{turn}-reason"),"type":"reasoning","encrypted_content":signature,"summary":[]},{"id":format!("{turn}-item"),"type":"function_call","call_id":format!("{turn}-call"),"name":"read","arguments":arguments.to_string()}]})
        }
        "anthropic-messages" => {
            json!({"type":"message","role":"assistant","content":[{"type":"thinking","thinking":"displayable thought, excluded from summarization","signature":signature},{"type":"tool_use","id":format!("{turn}-call"),"name":"read","input":arguments}],"stop_reason":"tool_use"})
        }
        _ => unreachable!(),
    }
}

async fn idle(
    updates: &mut tokio::sync::mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
) -> Vec<Value> {
    let mut result = Vec::new();
    loop {
        let update = serde_json::to_value(updates.recv().await.unwrap()).unwrap();
        let done = update["update"]["state"] == "idle";
        result.push(update);
        if done {
            return result;
        }
    }
}

/// Each invocation owns a fresh ACP runtime, so close/restore cannot reuse in-memory context.
async fn durable_chat_prompts(
    root: &std::path::Path,
    server: &Server,
    settings: &str,
    resume: Option<v2::SessionId>,
    prompts: Vec<String>,
) -> (v2::SessionId, Vec<Vec<Value>>) {
    let runtime = bee::acp::Runtime::new(Default::default(), Default::default())
        .with_history(bee::history::Store::open(root.join("state")).unwrap());
    let profiles = support::providers("openai-chat", &server.url, settings);
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    let saved = std::sync::Arc::new(std::sync::Mutex::new(None));
    let captured = saved.clone();
    let cwd = root.to_owned();
    tokio::time::timeout(
        Duration::from_secs(20),
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
                let id = if let Some(id) = resume {
                    connection
                        .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd))
                        .block_task()
                        .await?;
                    idle(&mut updates).await;
                    id
                } else {
                    connection
                        .send_request(v2::NewSessionRequest::new(cwd))
                        .block_task()
                        .await?
                        .session_id
                };
                let mut batches = Vec::new();
                for prompt in prompts {
                    connection
                        .send_request(v2::PromptRequest::new(id.clone(), vec![prompt.into()]))
                        .block_task()
                        .await?;
                    batches.push(idle(&mut updates).await);
                }
                connection
                    .send_request(v2::CloseSessionRequest::new(id.clone()))
                    .block_task()
                    .await?;
                *captured.lock().unwrap() = Some((id, batches));
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    saved.lock().unwrap().take().unwrap()
}

#[tokio::test]
async fn rolling_compaction_replaces_the_previous_summary_and_failed_replacement_survives_restore()
{
    let mut failure = Response::json(json!({"error":"private-upstream-sentinel"}));
    failure.status = 500;
    let server = Server::start(vec![
        Response::json(support::chat_text(&"first old answer ".repeat(200))),
        Response::json(support::chat_text(&"second old answer ".repeat(200))),
        Response::json(support::chat_text("first retained summary")),
        Response::json(support::chat_text(&"third answer ".repeat(200))),
        Response::json(support::chat_text("rolling retained summary")),
        Response::json(support::chat_text(&"fourth answer ".repeat(200))),
        failure,
        Response::json(support::chat_text("after restore")),
    ]);
    let root = tempfile::tempdir().unwrap();
    let (id, batches) = durable_chat_prompts(
        root.path(),
        &server,
        "",
        None,
        [
            "first", "second", "/compact", "third", "/compact", "fourth", "/compact",
        ]
        .map(String::from)
        .into(),
    )
    .await;
    for batch in &batches[..6] {
        assert_eq!(batch.last().unwrap()["update"]["stopReason"], "end_turn");
    }
    assert_eq!(batches[6].last().unwrap()["update"]["stopReason"], "error");
    server.request();
    server.request();
    server.request();
    server.request();
    let replacement = server.request().body.to_string();
    assert!(replacement.contains("first retained summary"));
    assert!(replacement.contains("second old answer"));
    assert!(!replacement.contains("first old answer") && !replacement.contains("third answer"));
    server.request();
    let failed_source = server.request().body.to_string();
    assert!(
        failed_source.contains("rolling retained summary")
            && failed_source.contains("third answer")
    );
    assert!(!failed_source.contains("fourth answer"));
    let (_, restored) =
        durable_chat_prompts(root.path(), &server, "", Some(id), vec!["after".into()]).await;
    assert_eq!(
        restored[0].last().unwrap()["update"]["stopReason"],
        "end_turn"
    );
    let restored = server.request().body.to_string();
    assert_eq!(restored.matches("rolling retained summary").count(), 1);
    assert!(!restored.contains("first retained summary"));
    assert!(restored.contains("third answer") && restored.contains("fourth answer"));
    assert!(!restored.contains("/compact"));
    server.assert_no_more_requests();
}

#[tokio::test]
async fn automatic_eviction_preserves_the_summary_and_protected_summary_overflow_rolls_back_after_restore()
 {
    let server = Server::start(vec![
        Response::json(support::chat_text(&"old answer ".repeat(200))),
        Response::json(support::chat_text("latest answer")),
        Response::json(support::chat_text("persistent retained summary")),
        Response::json(support::chat_text("probe answer")),
        Response::json(support::chat_text("evicted answer")),
        Response::json(support::chat_text("restored answer")),
        Response::json(support::chat_text("after protected overflow")),
    ]);
    let root = tempfile::tempdir().unwrap();
    let (id, initial) = durable_chat_prompts(
        root.path(),
        &server,
        "max_output_tokens=64",
        None,
        ["old", "latest", "/compact", "probe"]
            .map(String::from)
            .into(),
    )
    .await;
    assert!(
        initial
            .iter()
            .all(|batch| batch.last().unwrap()["update"]["stopReason"] == "end_turn")
    );
    server.request();
    server.request();
    server.request();
    let probe = server.request().body;
    let instructions = probe["messages"][0]["content"].as_str().unwrap().to_owned();
    // Derive an exact production budget from the observed native summary framing and real ACP catalog.
    let model = bee::llm::Model::new(
        support::profile("openai-chat", &server.url, "max_output_tokens=64"),
        "private-model".into(),
        None,
    )
    .unwrap()
    .with_instructions(instructions);
    let tools = [bee::acp::plan_tool()];
    let growing = "growing prompt ".repeat(300);
    let report = model
        .context(&[bee::llm::Message::User(growing.clone())], &tools)
        .unwrap();
    let window = report.estimated_input + report.reserved_output;
    let settings = format!("max_output_tokens=64\ncontext_window={window}");
    let (id, evicted) = durable_chat_prompts(
        root.path(),
        &server,
        &settings,
        Some(id),
        vec![growing, "oversized protected prompt ".repeat(3000)],
    )
    .await;
    assert_eq!(
        evicted[0].last().unwrap()["update"]["stopReason"],
        "end_turn"
    );
    assert!(
        evicted[0]
            .iter()
            .any(|update| update["update"]["sessionUpdate"] == "notice")
    );
    assert_eq!(evicted[1].last().unwrap()["update"]["stopReason"], "error");
    let request = server.request().body.to_string();
    assert_eq!(request.matches("persistent retained summary").count(), 1);
    assert!(!request.contains("latest answer") && !request.contains("probe answer"));
    server.assert_no_more_requests();
    let (id, restored) = durable_chat_prompts(
        root.path(),
        &server,
        &settings,
        Some(id),
        vec!["after restart".into()],
    )
    .await;
    assert_eq!(
        restored[0].last().unwrap()["update"]["stopReason"],
        "end_turn"
    );
    let restored = server.request().body.to_string();
    assert_eq!(restored.matches("persistent retained summary").count(), 1);
    assert!(!restored.contains("oversized protected prompt"));
    let protected = model
        .context(&[bee::llm::Message::User("protected".into())], &tools)
        .unwrap();
    let too_small = protected.estimated_input + protected.reserved_output - 1;
    let (id, failed) = durable_chat_prompts(
        root.path(),
        &server,
        &format!("max_output_tokens=64\ncontext_window={too_small}"),
        Some(id),
        vec!["protected".into()],
    )
    .await;
    assert_eq!(failed[0].last().unwrap()["update"]["stopReason"], "error");
    server.assert_no_more_requests();
    let (_, after) = durable_chat_prompts(
        root.path(),
        &server,
        &settings,
        Some(id),
        vec!["after overflow".into()],
    )
    .await;
    assert_eq!(after[0].last().unwrap()["update"]["stopReason"], "end_turn");
    let after = server.request().body.to_string();
    assert_eq!(after.matches("persistent retained summary").count(), 1);
    assert!(!after.contains("\"content\":\"protected\""));
    server.assert_no_more_requests();
}

#[rstest::rstest]
#[case("openai-chat", false)]
#[case("openai-responses", false)]
#[case("anthropic-messages", false)]
#[case("openai-chat", true)]
#[tokio::test]
async fn explicit_compaction_retains_the_latest_turn_and_replays_full_summaries(
    #[case] protocol: &str,
    #[case] durable: bool,
) {
    let server = Server::start(vec![
        Response::json(tool_answer(protocol, "old")),
        Response::json(answer(protocol, &"old answer ".repeat(200))),
        Response::json(tool_answer(protocol, "latest")),
        Response::json(answer(protocol, "latest answer")),
        Response::json(answer(protocol, "Remember the old goal.")),
        Response::json(answer(protocol, "next answer")),
        Response::json(answer(protocol, "isolated answer")),
    ]);
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("old.txt"), "old visible tool facts").unwrap();
    std::fs::write(root.path().join("latest.txt"), "latest visible tool facts").unwrap();
    let mut runtime = bee::acp::Runtime::new(Default::default(), bee::tool::builtins().unwrap());
    if durable {
        runtime =
            runtime.with_history(bee::history::Store::open(root.path().join("state")).unwrap());
    }
    let profiles = support::providers(protocol, &server.url, "");
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(20),
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
                let created = connection
                    .send_request(v2::NewSessionRequest::new(root.path().to_owned()))
                    .block_task()
                    .await?;
                assert_eq!(
                    serde_json::to_value(&created).unwrap()["availableCommands"][0]["name"],
                    "compact"
                );
                let id = created.session_id;
                for invalid in [
                    vec!["/compact extra".into()],
                    vec!["/compact".into(), "attachment".into()],
                ] {
                    assert!(
                        connection
                            .send_request(v2::PromptRequest::new(id.clone(), invalid))
                            .block_task()
                            .await
                            .is_err()
                    );
                }
                for text in [
                    "/compact",
                    "old goal",
                    "latest goal",
                    "/compact",
                    "/compact",
                    "next question",
                ] {
                    connection
                        .send_request(v2::PromptRequest::new(id.clone(), vec![text.into()]))
                        .block_task()
                        .await?;
                    let batch = idle(&mut updates).await;
                    assert_eq!(
                        batch.last().unwrap()["update"]["stopReason"],
                        "end_turn",
                        "{batch:?}"
                    );
                    let compactions: Vec<_> = batch
                        .iter()
                        .filter(|u| u["update"]["sessionUpdate"] == "compaction_update")
                        .collect();
                    if text == "/compact" && !compactions.is_empty() {
                        assert_eq!(compactions.len(), 2);
                        assert_eq!(compactions[0]["update"]["status"], "in_progress");
                        assert_eq!(compactions[1]["update"]["status"], "completed");
                        assert_eq!(
                            compactions[1]["update"]["summary"][0]["text"],
                            "Remember the old goal."
                        );
                        assert!(!batch.iter().any(|u| matches!(
                            u["update"]["sessionUpdate"].as_str(),
                            Some("agent_message" | "agent_message_chunk" | "agent_thought")
                        )));
                    }
                }
                connection
                    .send_request(v2::CloseSessionRequest::new(id.clone()))
                    .block_task()
                    .await?;
                for _ in 0..2 {
                    connection
                        .send_request(
                            v2::ResumeSessionRequest::new(id.clone(), root.path().to_owned())
                                .replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new())),
                        )
                        .block_task()
                        .await?;
                    let replay = idle(&mut updates).await;
                    let compactions: Vec<_> = replay
                        .iter()
                        .enumerate()
                        .filter(|(_, u)| u["update"]["sessionUpdate"] == "compaction_update")
                        .collect();
                    assert_eq!(compactions.len(), 1, "{replay:?}");
                    assert_eq!(compactions[0].1["update"]["status"], "completed");
                    assert_eq!(
                        compactions[0].1["update"]["summary"]
                            .as_array()
                            .unwrap()
                            .len(),
                        1
                    );
                    let old = replay
                        .iter()
                        .position(|u| {
                            u["update"]["content"][0]["text"]
                                .as_str()
                                .is_some_and(|text| text.starts_with("old answer"))
                        })
                        .unwrap();
                    let next = replay
                        .iter()
                        .position(|u| u["update"]["content"][0]["text"] == "next question")
                        .unwrap();
                    assert!(old < compactions[0].0 && compactions[0].0 < next);
                    connection
                        .send_request(v2::CloseSessionRequest::new(id.clone()))
                        .block_task()
                        .await?;
                }
                let other = connection
                    .send_request(v2::NewSessionRequest::new(root.path().to_owned()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(other, vec!["isolated".into()]))
                    .block_task()
                    .await?;
                idle(&mut updates).await;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let first = server.request().body;
    let old_continuation = server.request().body;
    let second = server.request().body;
    let latest_continuation = server.request().body;
    let summary = server.request().body;
    let next = server.request().body;
    let other = server.request().body;
    assert!(first.to_string().contains("old goal"));
    assert!(second.to_string().contains("old answer"));
    assert!(
        old_continuation
            .to_string()
            .contains("old visible tool facts")
    );
    assert!(
        latest_continuation
            .to_string()
            .contains("latest-private-native")
    );
    assert!(summary.to_string().contains("old answer"));
    assert!(summary.to_string().contains("old visible tool facts"));
    assert!(!summary.to_string().contains("private-native"));
    assert!(!summary.to_string().contains("displayable thought"));
    assert!(!summary.to_string().contains("latest answer"));
    assert!(summary.get("tools").is_none());
    assert!(next.to_string().contains("Remember the old goal."));
    assert!(next.to_string().contains("latest answer"));
    assert!(next.to_string().contains("latest-private-native"));
    assert!(next.to_string().contains("latest visible tool facts"));
    assert!(!next.to_string().contains("old-private-native"));
    assert!(!next.to_string().contains("old answer"));
    assert!(!next.to_string().contains("/compact"));
    assert!(!other.to_string().contains("Remember the old goal."));
    server.assert_no_more_requests();
}

#[rstest::rstest]
#[case("refusal")]
#[case("truncation")]
#[case("empty")]
#[case("oversize")]
#[case("malformed")]
#[case("failure")]
#[case("nonshrinking")]
#[case("tools")]
#[tokio::test]
async fn unsuccessful_compaction_never_replaces_successful_context(#[case] failure: &str) {
    let result = match failure {
        "refusal" => Response::json(
            json!({"choices":[{"index":0,"message":{"role":"assistant","refusal":"no"},"finish_reason":"stop"}]}),
        ),
        "truncation" => Response::json(
            json!({"choices":[{"index":0,"message":{"role":"assistant","content":"partial"},"finish_reason":"length"}]}),
        ),
        "empty" => Response::json(support::chat_text("")),
        "oversize" => Response::json(support::chat_text(&"x".repeat(16 * 1024 + 1))),
        "malformed" => Response::json(json!({"choices":[]})),
        "nonshrinking" => Response::json(support::chat_text(&"summary".repeat(1000))),
        "tools" => Response::json(tool_answer("openai-chat", "unsolicited")),
        "failure" => {
            let mut r = Response::json(json!({"error":"private-provider-value"}));
            r.status = 500;
            r
        }
        _ => unreachable!(),
    };
    let server = Server::start(vec![
        Response::json(support::chat_text(&"old answer ".repeat(200))),
        Response::json(support::chat_text("latest answer")),
        result,
        Response::json(support::chat_text("after failure")),
    ]);
    let root = tempfile::tempdir().unwrap();
    let runtime = bee::acp::Runtime::new(Default::default(), Default::default())
        .with_history(bee::history::Store::open(root.path().join("state")).unwrap());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let client = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    );
    tokio::time::timeout(
        Duration::from_secs(20),
        client.connect_with(
            bee::acp::agent_with_runtime(
                support::providers("openai-chat", &server.url, ""),
                runtime,
            ),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let id = connection
                    .send_request(v2::NewSessionRequest::new(root.path().to_owned()))
                    .block_task()
                    .await?
                    .session_id;
                for text in ["old", "latest", "/compact", "after"] {
                    connection
                        .send_request(v2::PromptRequest::new(id.clone(), vec![text.into()]))
                        .block_task()
                        .await?;
                    let batch = idle(&mut updates).await;
                    if text == "/compact" {
                        assert_eq!(
                            batch.last().unwrap()["update"]["stopReason"],
                            "error",
                            "{batch:?}"
                        );
                        let last = batch
                            .iter()
                            .rev()
                            .find(|u| u["update"]["sessionUpdate"] == "compaction_update")
                            .unwrap();
                        assert_eq!(last["update"]["status"], "failed");
                        assert!(last["update"]["summary"].is_null());
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
    server.request();
    server.request();
    let after = server.request().body.to_string();
    assert!(after.contains("old answer") && after.contains("latest answer"));
    assert!(!after.contains("/compact"));
}
