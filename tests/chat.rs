//! Chat Completions through a real local HTTP connection.

mod support;

use bee::llm::{Error, Message, Model, Stop};
use serde_json::json;
use support::{Echo, Response, Server, chat_text, event, profile, tool};

#[tokio::test]
async fn signed_parallel_and_sequential_calls_replay_exact_metadata_after_streaming_and_serialization()
 {
    for streaming in [false, true] {
        let signed = json!({"id":"signed","type":"function","function":{"name":"echo","arguments":"{ \"text\": \"héllo\" }"},"extra_content":{"google":{"thought_signature":"opaque-signature-A","ignored":"not-replayed"}}});
        let unsigned = json!({"id":"unsigned","type":"function","function":{"name":"echo","arguments":"{\"text\":\"second\"}"}});
        let first = if streaming {
            let mut first = signed.clone();
            first["index"] = json!(0);
            first.as_object_mut().unwrap().remove("extra_content");
            first["function"]["arguments"] = json!("{ \"text\":");
            let mut second = unsigned.clone();
            second["index"] = json!(1);
            Response::sse(format!(
                "{}{}{}{}data: [DONE]\n\n",
                event(
                    "",
                    json!({"choices":[{"index":0,"delta":{"tool_calls":[first,second]},"finish_reason":null}]})
                ),
                event(
                    "",
                    json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"extra_content":signed["extra_content"]}]},"finish_reason":null}]})
                ),
                event(
                    "",
                    json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":" \"héllo\" }"},"extra_content":signed["extra_content"]}]},"finish_reason":"tool_calls"}]})
                ),
                event(
                    "",
                    json!({"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":3}})
                ),
            ))
        } else {
            Response::json(
                json!({"choices":[{"index":0,"message":{"tool_calls":[signed,unsigned]},"finish_reason":"tool_calls"}]}),
            )
        };
        let server = Server::start(vec![
            first,
            Response::json(
                json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"next","type":"function","function":{"name":"echo","arguments":"{\"text\":\"third\"}"},"extra_content":{"google":{"thought_signature":"opaque-signature-B"}}}]},"finish_reason":"tool_calls"}]}),
            ),
            Response::json(chat_text("done")),
            Response::json(chat_text("resumed")),
        ]);
        let model = Model::new(
            profile("openai-chat", &server.url, ""),
            "gemini-test".into(),
            Some(bee::auth::Secret::new("studio-key")),
        )
        .unwrap();
        let mut visible = String::new();
        let result = bee::agent::run(
            &model,
            vec![Message::User("echo".into())],
            &[tool()],
            Some(&Echo),
            &mut |text| visible.push_str(text),
        )
        .await
        .unwrap();
        assert_eq!(visible, "done");
        assert!(!format!("{:?}", result.messages).contains("opaque-signature"));
        let initial = server.request();
        assert!(
            initial
                .head
                .to_lowercase()
                .contains("authorization: bearer studio-key")
        );
        assert_eq!(initial.body["model"], "gemini-test");
        let second = server.request();
        let calls = &second.body["messages"][1]["tool_calls"];
        assert_eq!(
            calls[0]["extra_content"],
            json!({"google":{"thought_signature":"opaque-signature-A"}})
        );
        assert_eq!(calls[0]["function"]["arguments"], "{ \"text\": \"héllo\" }");
        assert!(calls[1].get("extra_content").is_none());
        assert_eq!(second.body["messages"][2]["tool_call_id"], "signed");
        assert_eq!(second.body["messages"][3]["tool_call_id"], "unsigned");
        let third = server.request();
        assert_eq!(
            third.body["messages"][4]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
            "opaque-signature-B"
        );
        assert_eq!(third.body["messages"][1], second.body["messages"][1]);
        // Native JSON is durable history's representation; replay must survive its round trip.
        let restored: Vec<Message> =
            serde_json::from_slice(&serde_json::to_vec(&result.messages).unwrap()).unwrap();
        model.step(&restored, &[tool()], &mut |_| {}).await.unwrap();
        let resumed = server.request();
        assert_eq!(resumed.body["messages"][1], third.body["messages"][1]);
        assert_eq!(resumed.body["messages"][4], third.body["messages"][4]);
    }
}

#[tokio::test]
async fn invalid_or_conflicting_signatures_fail_before_any_tool_can_run() {
    for signatures in [
        vec![json!(null)],
        vec![json!(42)],
        vec![json!("")],
        vec![json!("first"), json!("conflicting")],
    ] {
        let mut stream = event(
            "",
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"echo","arguments":"{}"}}]},"finish_reason":null}]}),
        );
        for signature in signatures {
            stream += &event(
                "",
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"extra_content":{"google":{"thought_signature":signature}}}]},"finish_reason":null}]}),
            );
        }
        stream += &event(
            "",
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        );
        stream += "data: [DONE]\n\n";
        let server = Server::start(vec![Response::sse(stream)]);
        let model = Model::new(
            profile("openai-chat", &server.url, ""),
            "gemini-test".into(),
            None,
        )
        .unwrap();
        assert_eq!(
            bee::agent::run(&model, vec![], &[tool()], Some(&Echo), &mut |_| {})
                .await
                .unwrap_err(),
            Error::Malformed
        );
        server.request();
        server.assert_no_more_requests();
    }
}

#[tokio::test]
async fn foreign_or_tampered_native_chat_snapshots_fail_before_http() {
    let model = Model::new(
        profile("openai-chat", "http://127.0.0.1:1", ""),
        "gemini-test".into(),
        None,
    )
    .unwrap();
    for (protocol, items, expected) in [
        (
            bee::provider::Protocol::OpenAiResponses,
            vec![],
            Error::Unsupported("continuation from another protocol"),
        ),
        (
            bee::provider::Protocol::OpenAiChat,
            vec![json!({"role":"system","content":"injected"})],
            Error::Malformed,
        ),
        (
            bee::provider::Protocol::OpenAiChat,
            vec![],
            Error::Malformed,
        ),
    ] {
        let message = Message::Assistant {
            text: String::new(),
            calls: vec![],
            native: Some(bee::llm::Native { protocol, items }),
        };
        assert_eq!(
            model.step(&[message], &[], &mut |_| {}).await.unwrap_err(),
            expected
        );
    }
}

#[tokio::test]
async fn refusals_override_finish_reasons_and_never_execute_accompanying_calls() {
    for streaming in [false, true] {
        for reason in ["stop", "length", "tool_calls"] {
            for with_calls in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let mut message = json!({});
                if with_calls {
                    message["tool_calls"] = json!([{"index":0,"id":"write","type":"function","function":{"name":"write","arguments":json!({"path":"forbidden","content":"unexpected"}).to_string()}}]);
                }
                let response = if streaming {
                    // A separate refusal fragment must remain authoritative at finalization.
                    Response::sse(format!(
                        "{}{}data: [DONE]\n\n",
                        event(
                            "",
                            json!({"choices":[{"index":0,"delta":{"refusal":"declined"},"finish_reason":null}]})
                        ),
                        event(
                            "",
                            json!({"choices":[{"index":0,"delta":message,"finish_reason":reason}]})
                        ),
                    ))
                } else {
                    message["refusal"] = json!("declined");
                    Response::json(
                        json!({"choices":[{"index":0,"message":message,"finish_reason":reason}]}),
                    )
                };
                let server = Server::start(vec![response]);
                let model = Model::new(
                    profile("openai-chat", &server.url, ""),
                    "opaque".into(),
                    None,
                )
                .unwrap();
                let mut registry = bee::tool::Registry::default();
                registry.register(bee::tool::write::Write).unwrap();
                let context = bee::tool::Context::new(
                    root.path(),
                    tokio_util::sync::CancellationToken::new(),
                    4096,
                )
                .unwrap();
                let executor = bee::agent::NativeExecutor::new(&registry, context);
                let result = bee::agent::run(
                    &model,
                    vec![],
                    &executor.definitions(),
                    Some(&executor),
                    &mut |_| {},
                )
                .await;
                if with_calls {
                    assert_eq!(result.unwrap_err(), Error::Malformed);
                } else {
                    assert_eq!(result.unwrap().stop, Stop::Refusal);
                }
                assert!(!root.path().join("forbidden").exists());
                server.request();
                server.assert_no_more_requests();
            }
        }
    }
}

#[tokio::test]
async fn a_model_creates_reads_edits_and_verifies_a_file_through_native_tools() {
    let root = tempfile::tempdir().unwrap();
    let call = |id: &str, name: &str, arguments: serde_json::Value| {
        Response::json(json!({"choices":[{"index":0,"message":{"tool_calls":[{
            "id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}
        }]},"finish_reason":"tool_calls"}]}))
    };
    let server = Server::start(vec![
        call(
            "create",
            "write",
            json!({"path":"text","content":"héllo\r\nlast"}),
        ),
        call("inspect", "read", json!({"path":"text"})),
        call(
            "replace",
            "edit",
            json!({"path":"text","old_text":"héllo","new_text":"goodbye"}),
        ),
        call("verify", "read", json!({"path":"text"})),
        Response::json(chat_text("done")),
    ]);
    let model = Model::new(
        profile("openai-chat", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let mut registry = bee::tool::Registry::default();
    registry.register(bee::tool::write::Write).unwrap();
    registry.register(bee::tool::read::Read).unwrap();
    registry.register(bee::tool::edit::Edit).unwrap();
    let context = bee::tool::Context::new(
        root.path(),
        tokio_util::sync::CancellationToken::new(),
        4096,
    )
    .unwrap();
    let executor = bee::agent::NativeExecutor::new(&registry, context);
    let result = bee::agent::run(
        &model,
        vec![Message::User("create and edit text".into())],
        &executor.definitions(),
        Some(&executor),
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.stop, Stop::End);
    let first = server.request();
    let names: Vec<_> = first.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["edit", "read", "write"]);
    for (index, id, expected) in [
        (2, "create", json!({"bytes_written":12})),
        (
            4,
            "inspect",
            json!({"content":"1: héllo\n2: last\n","truncated":false,"next_line":null}),
        ),
        (6, "replace", json!({"bytes_written":13})),
        (
            8,
            "verify",
            json!({"content":"1: goodbye\n2: last\n","truncated":false,"next_line":null}),
        ),
    ] {
        let request = server.request();
        let message = &request.body["messages"][index];
        assert_eq!(message["tool_call_id"], id);
        let output: serde_json::Value =
            serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
        assert_eq!(output, json!({"data":expected}));
    }
    assert_eq!(
        std::fs::read(root.path().join("text")).unwrap(),
        b"goodbye\r\nlast"
    );
}

#[tokio::test]
async fn a_model_discovers_lists_and_reads_project_files() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "// project source").unwrap();
    let call = |id: &str, name: &str, arguments: &str| {
        Response::json(
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":arguments}}]},"finish_reason":"tool_calls"}]}),
        )
    };
    let server = Server::start(vec![
        call("find", "glob", r#"{"pattern":"**/*.rs"}"#),
        call("inspect", "list", r#"{"path":"src"}"#),
        call("read", "read", r#"{"path":"src/lib.rs"}"#),
        Response::json(chat_text("done")),
    ]);
    let model = Model::new(
        profile("openai-chat", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let mut registry = bee::tool::Registry::default();
    registry.register(bee::tool::glob::Glob).unwrap();
    registry.register(bee::tool::list::List).unwrap();
    registry.register(bee::tool::read::Read).unwrap();
    let context = bee::tool::Context::new(
        root.path(),
        tokio_util::sync::CancellationToken::new(),
        4096,
    )
    .unwrap();
    let executor = bee::agent::NativeExecutor::new(&registry, context);
    let result = bee::agent::run(
        &model,
        vec![Message::User("inspect the project".into())],
        &executor.definitions(),
        Some(&executor),
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.stop, Stop::End);
    assert_eq!(server.request().body["tools"].as_array().unwrap().len(), 3);
    for (index, id, field, expected) in [
        (
            2,
            "find",
            "entries",
            json!([{"path":"src/lib.rs","kind":"file"}]),
        ),
        (
            4,
            "inspect",
            "entries",
            json!([{"path":"lib.rs","kind":"file"}]),
        ),
        (6, "read", "content", json!("1: // project source\n")),
    ] {
        let request = server.request();
        let message = &request.body["messages"][index];
        assert_eq!(message["tool_call_id"], id);
        let output: serde_json::Value =
            serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
        assert_eq!(output["data"][field], expected);
    }
}

#[tokio::test]
async fn a_model_reads_a_real_file_through_the_native_executor() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("hello.txt"), "hello from disk").unwrap();
    let server = Server::start(vec![
        Response::json(
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"read-file","type":"function","function":{"name":"read","arguments":"{\"path\":\"hello.txt\"}"}}]},"finish_reason":"tool_calls"}]}),
        ),
        Response::json(chat_text("done")),
    ]);
    let model = Model::new(
        profile("openai-chat", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let mut registry = bee::tool::Registry::default();
    registry.register(bee::tool::read::Read).unwrap();
    let context = bee::tool::Context::new(
        root.path(),
        tokio_util::sync::CancellationToken::new(),
        4096,
    )
    .unwrap();
    let executor = bee::agent::NativeExecutor::new(&registry, context);
    bee::agent::run(
        &model,
        vec![Message::User("read hello.txt".into())],
        &executor.definitions(),
        Some(&executor),
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(
        server.request().body["tools"][0]["function"]["name"],
        "read"
    );
    let request = server.request();
    assert_eq!(request.body["messages"][2]["tool_call_id"], "read-file");
    let result: serde_json::Value =
        serde_json::from_str(request.body["messages"][2]["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["data"]["content"], "1: hello from disk\n");
}

#[tokio::test]
async fn a_registered_native_tool_returns_a_schema_error_then_executes_the_corrected_call() {
    let call = |id: &str, arguments: &str| {
        Response::json(
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":id,"type":"function","function":{"name":"echo","arguments":arguments}}]},"finish_reason":"tool_calls"}]}),
        )
    };
    let server = Server::start(vec![
        call("invalid", r#"{"text":""}"#),
        call("corrected", r#"{"text":"héllo"}"#),
        Response::json(chat_text("done")),
    ]);
    let model = Model::new(
        profile("openai-chat", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let registry = support::native_registry();
    let executor = bee::agent::NativeExecutor::new(&registry, support::native_context());
    let result = bee::agent::run(
        &model,
        vec![Message::User("echo".into())],
        &executor.definitions(),
        Some(&executor),
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.stop, Stop::End);
    let first = server.request();
    assert_eq!(
        first.body["tools"][0]["function"]["parameters"],
        registry.definitions().next().unwrap().input_schema
    );
    let second = server.request();
    let error = &second.body["messages"][2];
    assert_eq!(error["tool_call_id"], "invalid");
    let value: serde_json::Value =
        serde_json::from_str(error["content"].as_str().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "invalid_arguments");
    let third = server.request();
    assert_eq!(third.body["messages"][4]["tool_call_id"], "corrected");
    let value: serde_json::Value =
        serde_json::from_str(third.body["messages"][4]["content"].as_str().unwrap()).unwrap();
    assert_eq!(value, json!({"data":"héllo"}));
}

#[tokio::test]
async fn a_custom_endpoint_receives_the_selected_model_limits_and_safe_headers() {
    let server = Server::start(vec![Response::json(chat_text("answer"))]);
    let model = Model::new(
        profile(
            "openai-chat",
            &server.url,
            "max_output_tokens=123\n[accounts.test.headers]\nx-custom='private'",
        ),
        "unlisted-model".into(),
        Some(bee::auth::Secret::new("secret")),
    )
    .unwrap();
    let mut text = String::new();
    let outcome = bee::agent::run(
        &model,
        vec![Message::User("hello".into())],
        &[],
        None,
        &mut |chunk| text.push_str(chunk),
    )
    .await
    .unwrap();
    assert_eq!(text, "answer");
    assert_eq!(outcome.stop, Stop::End);
    assert_eq!(outcome.usage.input, Some(3));
    let request = server.request();
    assert!(request.head.starts_with("POST /prefix/chat/completions "));
    assert!(
        request
            .head
            .to_lowercase()
            .contains("authorization: bearer secret")
    );
    assert!(request.head.contains("x-custom: private"));
    assert_eq!(request.body["model"], "unlisted-model");
    assert_eq!(request.body["max_tokens"], 123);
    assert!(request.body.get("tools").is_none());
}

#[tokio::test]
async fn fragmented_tool_deltas_complete_a_model_tool_result_model_cycle() {
    let mut stream = event(
        "",
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"echo","arguments":"{\"text\":"}}]},"finish_reason":null}]}),
    );
    stream += &event(
        "",
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"héllo\"}"}}]},"finish_reason":"tool_calls"}]}),
    );
    stream += "data: [DONE]\r\n\r\n";
    let server = Server::start(vec![
        Response::sse(stream),
        Response::json(chat_text("done")),
    ]);
    let model = Model::new(
        profile("openai-chat", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let result = bee::agent::run(
        &model,
        vec![Message::User("echo".into())],
        &[tool()],
        Some(&Echo),
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.messages.len(), 4);
    let first = server.request();
    assert_eq!(first.body["tools"][0]["function"]["name"], "echo");
    let second = server.request();
    assert_eq!(second.body["messages"][1]["tool_calls"][0]["id"], "call-1");
    assert_eq!(second.body["messages"][2]["tool_call_id"], "call-1");
    assert_eq!(second.body["messages"][2]["content"], "héllo");
}

#[tokio::test]
async fn streaming_text_and_usage_are_assembled_without_a_json_fallback() {
    let stream = format!(
        ": keepalive\n\n{}{}{}data: [DONE]\n\n",
        event(
            "",
            json!({"choices":[{"index":0,"delta":{"content":"héllo"},"finish_reason":null}]})
        ),
        event(
            "",
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]})
        ),
        event(
            "",
            json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":0}}})
        )
    );
    let server = Server::start(vec![Response::sse(stream)]);
    let model = Model::new(
        profile("openai-chat", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let mut text = String::new();
    let step = model
        .step(&[Message::User("hi".into())], &[], &mut |chunk| {
            text.push_str(chunk)
        })
        .await
        .unwrap();
    assert_eq!(text, "héllo");
    assert_eq!(step.stop, Stop::MaxTokens);
    assert_eq!(step.usage.output, Some(4));
    let bee::llm::CacheUsage::OpenAi {
        cached, written, ..
    } = &step.usage.cache[0]
    else {
        panic!("native cache usage expected")
    };
    assert_eq!(cached.tokens, Some(3));
    assert_eq!(written.tokens, Some(0));
}

#[tokio::test]
async fn failures_are_classified_without_echoing_the_server_body() {
    for (status, expected) in [
        (401, Error::Authentication),
        (403, Error::Authentication),
        (429, Error::RateLimit),
        (500, Error::Http(500)),
    ] {
        for (repeats, chunked) in [(1, false), (10000, false), (10000, true)] {
            let mut response = Response::json(json!({"secret":"sentinel".repeat(repeats)}));
            response.status = status;
            if chunked {
                // Force inspection to accumulate a prefix before crossing the bound on later bytes.
                response.pause = Some((32 * 1024, std::time::Duration::from_millis(20)));
            }
            let server = Server::start(vec![response]);
            let model = Model::new(
                profile("openai-chat", &server.url, ""),
                "opaque".into(),
                None,
            )
            .unwrap();
            let error = model.step(&[], &[], &mut |_| {}).await.unwrap_err();
            assert_eq!(error, expected);
            assert!(!format!("{error:?} {error}").contains("sentinel"));
        }
    }
}

#[tokio::test]
async fn malformed_and_unterminated_responses_fail_clearly() {
    for response in [
        Response::json(json!({"error":{"message":"sentinel"}})),
        Response::sse(event(
            "",
            json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]}),
        )),
        Response::json(json!({"choices":[]})),
    ] {
        let server = Server::start(vec![response]);
        let model = Model::new(
            profile("openai-chat", &server.url, ""),
            "opaque".into(),
            None,
        )
        .unwrap();
        assert!(model.step(&[], &[], &mut |_| {}).await.is_err());
    }
}

#[tokio::test]
async fn a_stalled_request_obeys_the_configured_deadline() {
    let mut response = Response::json(chat_text("late"));
    response.delay = std::time::Duration::from_millis(100);
    let server = Server::start(vec![response]);
    let model = Model::new(
        profile("openai-chat", &server.url, "timeout_ms=10"),
        "opaque".into(),
        None,
    )
    .unwrap();
    assert_eq!(
        model.step(&[], &[], &mut |_| {}).await.unwrap_err(),
        Error::Timeout
    );
}

#[tokio::test]
async fn an_unsolicited_tool_call_cannot_execute_with_an_empty_catalog() {
    let server = Server::start(vec![Response::json(
        json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"x","type":"function","function":{"name":"echo","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
    )]);
    let model = Model::new(
        profile("openai-chat", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    assert_eq!(
        bee::agent::run(&model, vec![], &[], None, &mut |_| {})
            .await
            .unwrap_err(),
        Error::Tool
    );
}

#[tokio::test]
async fn conflicting_generated_headers_fail_before_sending_credentials() {
    let model = Model::new(
        profile(
            "openai-chat",
            "http://127.0.0.1:1",
            "headers={authorization='sentinel'}",
        ),
        "opaque".into(),
        Some(bee::auth::Secret::new("key")),
    )
    .unwrap();
    assert_eq!(
        model.step(&[], &[], &mut |_| {}).await.unwrap_err(),
        Error::Headers
    );
}

#[tokio::test]
async fn invalid_tool_arguments_never_reach_an_executor() {
    let server = Server::start(vec![Response::json(
        json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"x","type":"function","function":{"name":"echo","arguments":"not JSON sentinel"}}]},"finish_reason":"tool_calls"}]}),
    )]);
    let model = Model::new(
        profile("openai-chat", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    assert_eq!(
        bee::agent::run(&model, vec![], &[tool()], Some(&Echo), &mut |_| {})
            .await
            .unwrap_err(),
        Error::Malformed
    );
}

#[tokio::test]
async fn a_failure_on_the_preferred_protocol_does_not_fall_back_to_chat() {
    let mut response = Response::json(json!({"type":"error"}));
    response.status = 401;
    let server = Server::start(vec![response]);
    let profiles = support::providers(
        "anthropic-messages",
        &server.url,
        "prefer=['anthropic-messages','openai-chat']\n[accounts.test.protocols.openai-chat]\nbase_url='http://127.0.0.1:1'",
    );
    let model = Model::new(profiles.get("test").unwrap().clone(), "opaque".into(), None).unwrap();
    assert_eq!(
        model.step(&[], &[], &mut |_| {}).await.unwrap_err(),
        Error::Authentication
    );
    assert!(
        server
            .request()
            .head
            .starts_with("POST /prefix/v1/messages ")
    );
}
