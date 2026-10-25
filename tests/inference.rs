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
#[case::failed(0)]
#[case::refused(1)]
#[case::cancelled(2)]
#[tokio::test]
async fn unsuccessful_truncation_preserves_prior_context_across_close_and_resume(#[case] kind: u8) {
    let mut rejected = Response::json(if kind == 1 {
        json!({"choices":[{"index":0,"message":{"role":"assistant","refusal":"declined"},"finish_reason":"stop"}]})
    } else {
        json!({"error":{"code":"context_length_exceeded"}})
    });
    if kind == 0 {
        rejected.status = 400;
    }
    if kind == 2 {
        rejected.delay = Duration::from_secs(1);
    }
    let server = Server::start(vec![
        Response::json(chat_text("retained-answer")),
        rejected,
        Response::json(chat_text("recovered")),
    ]);
    let first = "old".repeat(1000);
    let model = bee::llm::Model::new(
        support::profile("openai-chat", &server.url, "max_output_tokens=20"),
        "private-model".into(),
        None,
    )
    .unwrap();
    let window = model
        .context(&[bee::llm::Message::User(first.clone())], &[])
        .unwrap()
        .estimated_input
        + 420;
    let profiles = providers(
        "openai-chat",
        &server.url,
        &format!("max_output_tokens=20\ncontext_window={window}"),
    );
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::Registry::default(),
    );
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
            bee::acp::agent_with_runtime(profiles, runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let cwd = std::env::temp_dir();
                let session = connection
                    .send_request(v2::NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?
                    .session_id;
                for (index, prompt) in [first, "discarded".repeat(400), "tiny".into()]
                    .into_iter()
                    .enumerate()
                {
                    if index == 2 {
                        connection
                            .send_request(v2::CloseSessionRequest::new(session.clone()))
                            .block_task()
                            .await?;
                        let mut request =
                            v2::ResumeSessionRequest::new(session.clone(), cwd.clone());
                        // Empty ACP replay must not discard committed model context.
                        request.replay_from =
                            Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new()));
                        connection.send_request(request).block_task().await?;
                        // Resume's idle snapshot is not a completed prompt.
                        assert!(matches!(
                            updates.recv().await.unwrap().update,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                        ));
                    }
                    connection
                        .send_request(v2::PromptRequest::new(session.clone(), vec![prompt.into()]))
                        .block_task()
                        .await?;
                    let request = server.request_async().await;
                    if index == 1 {
                        assert!(!request.body.to_string().contains("retained-answer"));
                        if kind == 2 {
                            connection.send_notification(v2::CancelSessionNotification::new(
                                session.clone(),
                            ))?;
                        }
                    }
                    if index == 2 {
                        assert!(request.body.to_string().contains("retained-answer"));
                        assert!(!request.body.to_string().contains("discarded"));
                    }
                    let mut reports = 0;
                    loop {
                        match updates.recv().await.unwrap().update {
                            v2::SessionUpdate::UsageUpdate(_) => reports += 1,
                            v2::SessionUpdate::Notice(_) => {
                                panic!("failed pruning must not be reported as committed")
                            }
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
                                assert_eq!(reports, usize::from(index != 1));
                                if index == 1 {
                                    assert!(match kind {
                                        0 => matches!(
                                            idle.stop_reason,
                                            Some(v2::StopReason::Error(_))
                                        ),
                                        1 => idle.stop_reason == Some(v2::StopReason::Refusal),
                                        _ => idle.stop_reason == Some(v2::StopReason::Cancelled),
                                    });
                                }
                                break;
                            }
                            _ => {}
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

fn native_batch(protocol: &str, calls: Vec<(&str, &str, Value)>) -> Value {
    let mut calls = calls.into_iter();
    let (id, name, args) = calls.next().unwrap();
    let mut value = native_call(protocol, id, name, args);
    for (id, name, args) in calls {
        let other = native_call(protocol, id, name, args);
        match protocol {
            "openai-chat" => value["choices"][0]["message"]["tool_calls"]
                .as_array_mut()
                .unwrap()
                .extend(
                    other["choices"][0]["message"]["tool_calls"]
                        .as_array()
                        .unwrap()
                        .clone(),
                ),
            "openai-responses" => value["output"]
                .as_array_mut()
                .unwrap()
                .extend(other["output"].as_array().unwrap().clone()),
            _ => value["content"]
                .as_array_mut()
                .unwrap()
                .extend(other["content"].as_array().unwrap().clone()),
        }
    }
    value
}

fn instruction_text<'a>(protocol: &str, body: &'a Value) -> &'a str {
    match protocol {
        "openai-chat" => body["messages"][0]["content"].as_str().unwrap(),
        "openai-responses" => body["instructions"].as_str().unwrap(),
        _ => body["system"].as_str().unwrap(),
    }
}

#[rstest::rstest]
#[case("openai-chat", true)]
#[case("openai-responses", true)]
#[case("anthropic-messages", true)]
#[case("openai-chat", false)]
#[case("openai-responses", false)]
#[case("anthropic-messages", false)]
#[tokio::test]
async fn activated_skills_survive_eviction_but_failed_turns_do_not_commit_them(
    #[case] protocol: &str,
    #[case] succeeds: bool,
) {
    let project = tempfile::tempdir().unwrap();
    let skill = project.path().join(".agents/skills/demo/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: demo\ndescription: Demo\n---\nACTIVE-SNAPSHOT\n",
    )
    .unwrap();
    let mut finish = Response::json(answer(protocol, &"OLD-ANSWER".repeat(3000)));
    if !succeeds {
        finish.status = 500;
    }
    let server = Server::start(vec![
        Response::json(native_call(
            protocol,
            "skill",
            "read",
            json!({"path":".agents/skills/demo/SKILL.md","limit":1}),
        )),
        finish,
        Response::json(answer(protocol, "done")),
    ]);
    let runtime = bee::acp::Runtime::builtins(bee::agent::Limits::default())
        .unwrap()
        .with_instructions(bee::instructions::Sources::default());
    native_turns(
        providers(
            protocol,
            &server.url,
            "context_window=12000\nmax_output_tokens=100",
        ),
        runtime,
        vec![(project.path().into(), vec![succeeds, true])],
    )
    .await;
    let first = server.request();
    assert!(!instruction_text(protocol, &first.body).contains("ACTIVE-SNAPSHOT"));
    let second = server.request();
    assert!(instruction_text(protocol, &second.body).contains("ACTIVE-SNAPSHOT"));
    let third = server.request();
    assert_eq!(
        instruction_text(protocol, &third.body).contains("ACTIVE-SNAPSHOT"),
        succeeds
    );
    assert!(!third.body.to_string().contains("OLD-ANSWER"));
    // Eviction must remove the call and result together, not preserve orphan results.
    match protocol {
        "openai-chat" => assert!(
            third.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["role"] != "tool" && m.get("tool_calls").is_none())
        ),
        "openai-responses" => assert!(
            third.body["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["type"] != "function_call" && m["type"] != "function_call_output")
        ),
        _ => assert!(
            third.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().unwrap())
                .all(|b| b["type"] != "tool_use" && b["type"] != "tool_result")
        ),
    }
}

#[rstest::rstest]
#[case("openai-chat")]
#[case("openai-responses")]
#[case("anthropic-messages")]
#[tokio::test]
async fn scoped_guidance_and_skill_activation_reach_the_model_before_batched_mutations(
    #[case] protocol: &str,
) {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let home = root.path().join("home");
    for directory in ["sub", "sibling", ".agents/skills/demo/references"] {
        std::fs::create_dir_all(project.join(directory)).unwrap();
    }
    std::fs::create_dir_all(home.join(".agents")).unwrap();
    std::fs::write(home.join(".agents/AGENTS.md"), "USER-GUIDANCE").unwrap();
    std::fs::write(project.join("AGENTS.md"), "ROOT-GUIDANCE").unwrap();
    std::fs::write(project.join("sub/AGENTS.md"), "NESTED-GUIDANCE").unwrap();
    std::fs::write(project.join("sibling/AGENTS.md"), "UNRELATED-GUIDANCE").unwrap();
    std::fs::write(
        project.join(".agents/skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: Demo skill\n---\nACTIVATED-BODY\nSee references/guide.md\n",
    )
    .unwrap();
    std::fs::write(
        project.join(".agents/skills/demo/references/guide.md"),
        "SUPPORTING-RESOURCE",
    )
    .unwrap();
    let write = |path| json!({"path":path,"content":"written"});
    let (second, release_second) = Response::json(native_batch(
        protocol,
        vec![
            ("w3", "write", write("root.txt")),
            ("w4", "write", write("sub/file.txt")),
        ],
    ))
    .gated();
    let (fourth, release_fourth) =
        Response::json(native_call(protocol, "w6", "write", write("activated.txt"))).gated();
    let server = Server::start(vec![
        Response::json(native_batch(
            protocol,
            vec![
                ("w1", "write", write("root.txt")),
                ("w2", "write", write("sub/file.txt")),
            ],
        )),
        second,
        Response::json(native_batch(
            protocol,
            vec![
                (
                    "s1",
                    "read",
                    json!({"path":".agents/skills/demo/SKILL.md","limit":1}),
                ),
                ("w5", "write", write("activated.txt")),
            ],
        )),
        fourth,
        Response::json(native_call(
            protocol,
            "r1",
            "read",
            json!({"path":".agents/skills/demo/references/guide.md"}),
        )),
        Response::json(answer(protocol, "done")),
        Response::json(answer(protocol, "retained")),
    ]);
    let runtime = bee::acp::Runtime::builtins(bee::agent::Limits::default())
        .unwrap()
        .with_instructions(bee::instructions::Sources::new(Some(home)));
    let profiles = providers(protocol, &server.url, "");
    let drive = native_turns(profiles, runtime, vec![(project.clone(), vec![true, true])]);
    let inspect = async {
        let first = server.request_async().await;
        let text = instruction_text(protocol, &first.body);
        assert!(text.contains("You are Bee"));
        assert!(text.contains("ROOT-GUIDANCE") && text.contains("USER-GUIDANCE"));
        assert!(text.contains("Demo skill"));
        assert!(!text.contains("ACTIVATED-BODY") && !text.contains("NESTED-GUIDANCE"));
        let second = server.request_async().await;
        assert!(!project.join("root.txt").exists());
        assert!(!project.join("sub/file.txt").exists());
        let text = instruction_text(protocol, &second.body);
        assert!(text.contains("NESTED-GUIDANCE"));
        assert!(!text.contains("UNRELATED-GUIDANCE"));
        assert_eq!(
            second.body.to_string().matches("guidance_required").count(),
            2
        );
        release_second.send(()).unwrap();
        server.request_async().await;
        let fourth = server.request_async().await;
        assert!(!project.join("activated.txt").exists());
        assert!(instruction_text(protocol, &fourth.body).contains("ACTIVATED-BODY"));
        release_fourth.send(()).unwrap();
        server.request_async().await;
        let sixth = server.request_async().await;
        assert!(sixth.body.to_string().contains("SUPPORTING-RESOURCE"));
        let seventh = server.request_async().await;
        assert!(instruction_text(protocol, &seventh.body).contains("ACTIVATED-BODY"));
    };
    tokio::join!(drive, inspect);
    assert_eq!(
        std::fs::read_to_string(project.join("activated.txt")).unwrap(),
        "written"
    );
}

#[rstest::rstest]
#[case("openai-chat")]
#[case("openai-responses")]
#[case("anthropic-messages")]
#[tokio::test]
async fn search_defers_for_root_guidance_without_activating_matched_skills(#[case] protocol: &str) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("sub")).unwrap();
    std::fs::create_dir_all(root.path().join(".agents/skills/demo")).unwrap();
    std::fs::write(root.path().join("AGENTS.md"), "ROOT-GUIDANCE").unwrap();
    std::fs::write(root.path().join("sub/AGENTS.md"), "SEARCH-ROOT-GUIDANCE").unwrap();
    std::fs::write(root.path().join("sub/file"), "needle").unwrap();
    std::fs::write(
        root.path().join(".agents/skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: Demo skill\n---\nACTIVATED-BODY\n",
    )
    .unwrap();
    let search = json!({"pattern":"needle","path":"sub"});
    let server = Server::start(vec![
        Response::json(native_call(protocol, "deferred", "grep", search.clone())),
        Response::json(native_call(protocol, "searched", "grep", search)),
        Response::json(native_call(
            protocol,
            "skill",
            "grep",
            json!({"pattern":"ACTIVATED-BODY","path":".agents/skills/demo"}),
        )),
        Response::json(answer(protocol, "done")),
    ]);
    let runtime = bee::acp::Runtime::builtins(bee::agent::Limits::default())
        .unwrap()
        .with_instructions(bee::instructions::Sources::new(None));
    assert_eq!(
        native_turns(
            providers(protocol, &server.url, ""),
            runtime,
            vec![(root.path().into(), vec![true])]
        )
        .await,
        ["done"]
    );
    let first = server.request();
    assert!(!instruction_text(protocol, &first.body).contains("SEARCH-ROOT-GUIDANCE"));
    let second = server.request();
    assert!(instruction_text(protocol, &second.body).contains("SEARCH-ROOT-GUIDANCE"));
    assert_eq!(
        native_result(protocol, &second.body, "deferred")["error"]["code"],
        "guidance_required"
    );
    let third = server.request();
    assert_eq!(
        native_result(protocol, &third.body, "searched")["data"]["matches"][0]["path"],
        "file"
    );
    let fourth = server.request();
    assert_eq!(
        native_result(protocol, &fourth.body, "skill")["data"]["matches"][0]["line"],
        "ACTIVATED-BODY"
    );
    assert!(!instruction_text(protocol, &fourth.body).contains("ACTIVATED-BODY"));
}

/// Drive ordinary ACP prompts, retaining each session's history between prompts.
async fn native_turns(
    profiles: bee::provider::Providers,
    runtime: bee::acp::Runtime,
    sessions: Vec<(std::path::PathBuf, Vec<bool>)>,
) -> Vec<String> {
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
                let mut answers = Vec::new();
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
                        let mut answer = String::new();
                        loop {
                            let notification = updates.recv().await.unwrap();
                            assert_eq!(notification.session_id, session);
                            if let v2::SessionUpdate::AgentMessageChunk(chunk) =
                                &notification.update
                                && let v2::ContentBlock::Text(text) = &chunk.content
                            {
                                answer.push_str(&text.text);
                            }
                            if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                                notification.update
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
                        answers.push(answer);
                    }
                }
                Ok(answers)
            },
        ),
    )
    .await
    .unwrap()
    .unwrap()
}

#[rstest::rstest]
#[case::targeted(0)]
#[case::bash(1)]
#[case::final_call(2)]
#[tokio::test]
async fn changing_agents_refreshes_guidance_before_targeted_targetless_and_followup_operations(
    #[case] kind: u8,
) {
    let project = tempfile::tempdir().unwrap();
    let (revised, release_revised) = Response::json(native_call(
        "openai-chat",
        "fresh",
        "write",
        json!({"path":"file","content":"revised"}),
    ))
    .gated();
    let mut calls = vec![(
        "rules",
        "write",
        json!({"path":"AGENTS.md","content":"NEW-GUIDANCE"}),
    )];
    if kind == 0 {
        calls.push(("stale", "write", json!({"path":"file","content":"stale"})));
    }
    if kind == 1 {
        calls.push(("stale", "bash", json!({"command":"printf stale > file"})));
    }
    let server = Server::start(vec![
        Response::json(native_batch("openai-chat", calls)),
        revised,
        Response::json(chat_text("done")),
    ]);
    let runtime = bee::acp::Runtime::new(
        bee::agent::Limits::default(),
        bee::tool::builtins_with_bash(support::bash_settings()).unwrap(),
    )
    .with_instructions(bee::instructions::Sources::default());
    let inspect = async {
        server.request_async().await;
        let second = server.request_async().await;
        assert!(!project.path().join("file").exists());
        assert!(instruction_text("openai-chat", &second.body).contains("NEW-GUIDANCE"));
        assert_eq!(
            second.body.to_string().matches("guidance_required").count(),
            usize::from(kind != 2)
        );
        release_revised.send(()).unwrap();
        server.request_async().await;
    };
    tokio::join!(
        native_turns(
            providers("openai-chat", &server.url, ""),
            runtime,
            vec![(project.path().into(), vec![true])]
        ),
        inspect
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("file")).unwrap(),
        "revised"
    );
}

#[tokio::test]
async fn malformed_skills_emit_actionable_acp_notices_even_when_inference_fails() {
    let project = tempfile::tempdir().unwrap();
    let skill = project.path().join(".agents/skills/demo/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        skill,
        "---\nname: demo\ndescription: [secret-sentinel\n---\n",
    )
    .unwrap();
    let mut failure = Response::json(json!({"error":"fixture"}));
    failure.status = 500;
    let server = Server::start(vec![failure]);
    let runtime = bee::acp::Runtime::builtins(bee::agent::Limits::default())
        .unwrap()
        .with_instructions(bee::instructions::Sources::default());
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
            bee::acp::agent_with_runtime(providers("openai-chat", &server.url, ""), runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(project.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["question".into()]))
                    .block_task()
                    .await?;
                let mut warnings = 0;
                loop {
                    match updates.recv().await.unwrap().update {
                        v2::SessionUpdate::Notice(notice) => {
                            warnings += 1;
                            let wire = serde_json::to_string(&notice).unwrap();
                            assert!(
                                wire.contains("bee::skills::metadata")
                                    && wire.contains("fix SKILL.md")
                            );
                            assert!(!wire.contains("secret-sentinel"));
                        }
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
                            assert!(matches!(idle.stop_reason, Some(v2::StopReason::Error(_))));
                            break;
                        }
                        _ => {}
                    }
                }
                assert_eq!(warnings, 1);
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    server.request();
}

#[rstest::rstest]
#[case::successful(0)]
#[case::refused(1)]
#[case::cancelled(2)]
#[tokio::test]
async fn resume_and_model_changes_retain_only_committed_skill_activations(#[case] kind: u8) {
    let project = tempfile::tempdir().unwrap();
    let skill = project.path().join(".agents/skills/demo/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        skill,
        "---\nname: demo\ndescription: Demo\n---\nTRANSACTIONAL-SKILL\n",
    )
    .unwrap();
    let terminal = Response::json(if kind == 1 {
        json!({"choices":[{"index":0,"message":{"role":"assistant","refusal":"declined"},"finish_reason":"stop"}]})
    } else {
        chat_text("first answer")
    });
    let (terminal, release_terminal) = if kind == 2 {
        let (terminal, release) = terminal.gated();
        (terminal, Some(release))
    } else {
        (terminal, None)
    };
    let server = Server::start(vec![
        Response::json(native_call(
            "openai-chat",
            "skill",
            "read",
            json!({"path":".agents/skills/demo/SKILL.md","limit":1}),
        )),
        terminal,
        Response::json(chat_text("replacement answer")),
    ]);
    let runtime = bee::acp::Runtime::builtins(bee::agent::Limits::default())
        .unwrap()
        .with_instructions(bee::instructions::Sources::default());
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
            bee::acp::agent_with_runtime(providers("openai-chat", &server.url, ""), runtime),
            async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v2::NewSessionRequest::new(project.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(v2::PromptRequest::new(
                        session.clone(),
                        vec!["activate".into()],
                    ))
                    .block_task()
                    .await?;
                server.request_async().await;
                let second = server.request_async().await;
                assert!(
                    instruction_text("openai-chat", &second.body).contains("TRANSACTIONAL-SKILL")
                );
                if kind == 2 {
                    connection
                        .send_notification(v2::CancelSessionNotification::new(session.clone()))?;
                }
                loop {
                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                        updates.recv().await.unwrap().update
                    {
                        assert_eq!(
                            idle.stop_reason,
                            Some(match kind {
                                0 => v2::StopReason::EndTurn,
                                1 => v2::StopReason::Refusal,
                                _ => v2::StopReason::Cancelled,
                            })
                        );
                        break;
                    }
                }
                if let Some(release) = release_terminal {
                    // The cancelled idle proves cancellation was processed before HTTP can finish.
                    release.send(()).unwrap();
                }
                connection
                    .send_request(v2::CloseSessionRequest::new(session.clone()))
                    .block_task()
                    .await?;
                connection
                    .send_request(v2::ResumeSessionRequest::new(
                        session.clone(),
                        project.path(),
                    ))
                    .block_task()
                    .await?;
                // Consume the resume snapshot before observing the replacement prompt.
                assert!(matches!(
                    updates.recv().await.unwrap().update,
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                ));
                connection
                    .send_request(v2::SetSessionConfigOptionRequest::new(
                        session.clone(),
                        "model",
                        "replacement-model",
                    ))
                    .block_task()
                    .await?;
                connection
                    .send_request(v2::PromptRequest::new(session, vec!["continue".into()]))
                    .block_task()
                    .await?;
                let third = server.request_async().await;
                assert_eq!(third.body["model"], "replacement-model");
                assert_eq!(
                    instruction_text("openai-chat", &third.body).contains("TRANSACTIONAL-SKILL"),
                    kind == 0
                );
                loop {
                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                        updates.recv().await.unwrap().update
                    {
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
        Response::json(native_call(
            protocol,
            "h",
            "grep",
            json!({"pattern":"^after-edit$","include":["*.txt"]}),
        )),
        Response::json(answer(protocol, "done")),
        Response::json(answer(protocol, "remembered")),
    ]);
    let answers = native_turns(
        providers(protocol, &server.url, ""),
        bee::acp::Runtime::new(
            bee::agent::Limits::default(),
            bee::tool::builtins_with_bash(support::bash_settings()).unwrap(),
        ),
        vec![(root.path().into(), vec![true, true])],
    )
    .await;
    assert_eq!(answers, ["done", "remembered"]);
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
    assert_eq!(
        names,
        ["bash", "edit", "glob", "grep", "list", "read", "write"]
    );
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
    let bash_result = native_result(protocol, &server.request().body, "g");
    assert_eq!(bash_result["data"]["stdout"], "after-edit");
    assert_eq!(bash_result["data"]["stderr"], "bash-stderr");
    assert_eq!(bash_result["data"]["exit_code"], 7);
    let grep_result = native_result(protocol, &server.request().body, "h");
    assert_eq!(
        grep_result["data"]["matches"],
        json!([{"path":"created.txt","line_number":1,"line":"after-edit"}])
    );
    let next = server.request();
    assert_eq!(native_result(protocol, &next.body, "h"), grep_result);
    assert!(next.body.to_string().contains("session-specific-content"));
    assert!(next.body.to_string().contains("after-edit"));
    assert!(next.body.to_string().contains("question 1"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("created.txt")).unwrap(),
        "after-edit\n"
    );
}

fn native_result(protocol: &str, body: &serde_json::Value, id: &str) -> serde_json::Value {
    let content = match protocol {
        "openai-chat" => &body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
            .expect("correlated chat tool result")["content"],
        "openai-responses" => &body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == id)
            .expect("correlated Responses tool result")["output"],
        "anthropic-messages" => &body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .find(|item| item["type"] == "tool_result" && item["tool_use_id"] == id)
            .expect("correlated Messages tool result")["content"],
        _ => unreachable!(),
    };
    serde_json::from_str(content.as_str().unwrap()).unwrap()
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
context_window = 100000
model_context_windows = {{ "vendor/messages.v2" = 20000 }}
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
                    let mut reported_window = None;
                    loop {
                        match updates.recv().await.unwrap().update {
                            v2::SessionUpdate::UsageUpdate(usage) => {
                                reported_window = Some(usage.size)
                            }
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
                                assert_eq!(idle.stop_reason, Some(v2::StopReason::EndTurn));
                                assert_eq!(
                                    reported_window,
                                    Some(if index == 1 { 20000 } else { 100000 })
                                );
                                break;
                            }
                            _ => {}
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
