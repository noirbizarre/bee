//! Anthropic's native blocks, headers and function-result continuation.

mod support;

use bee::llm::{Error, Message, Model, Stop};
use serde_json::{Value, json};
use support::{Echo, Response, Server, event, profile, tool};

fn answer(text: &str) -> Value {
    json!({"id":"msg-1","type":"message","role":"assistant","content":[{"type":"text","text":text}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":2}})
}

fn stream(blocks: Vec<Value>, reason: &str) -> String {
    let mut wire = event(
        "message_start",
        json!({"type":"message_start","message":{"id":"msg-1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":5,"cache_read_input_tokens":3,"output_tokens":1}}}),
    );
    for (index, block) in blocks.iter().enumerate() {
        let mut start = block.clone();
        if block["type"] == "text" {
            start["text"] = "".into();
        } else {
            start["input"] = json!({});
        }
        wire += &event(
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":start}),
        );
        let delta = if block["type"] == "text" {
            json!({"type":"text_delta","text":block["text"]})
        } else {
            json!({"type":"input_json_delta","partial_json":block["input"].to_string()})
        };
        wire += &event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":delta}),
        );
        wire += &event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":index}),
        );
    }
    wire += &event(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":4}}),
    );
    wire + &event("message_stop", json!({"type":"message_stop"}))
}

#[tokio::test]
async fn streamed_cache_creation_updates_are_cumulative_and_keep_start_only_counters() {
    let wire = event(
        "message_start",
        json!({"type":"message_start","message":{"type":"message","role":"assistant","content":[],
        "usage":{"input_tokens":5,"output_tokens":1,"cache_read_input_tokens":30,"cache_creation_input_tokens":10,
            "cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":0}}}}),
    ) + &event(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
            "usage":{"output_tokens":4,"cache_creation_input_tokens":20,"cache_creation":{"ephemeral_5m_input_tokens":20}}}),
    ) + &event("message_stop", json!({"type":"message_stop"}));
    let mut ordinary = answer("done");
    ordinary["usage"] = json!({"input_tokens":5,"output_tokens":4,"cache_read_input_tokens":30,"cache_creation_input_tokens":20,
        "cache_creation":{"ephemeral_5m_input_tokens":20,"ephemeral_1h_input_tokens":0}});
    let server = Server::start(vec![Response::sse(wire), Response::json(ordinary)]);
    let model = Model::new(
        profile("anthropic-messages", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let streamed = model
        .step(&[Message::User("question".into())], &[], &mut |_| {})
        .await
        .unwrap();
    let ordinary = model
        .step(&[Message::User("question".into())], &[], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(streamed.usage, ordinary.usage);
    assert_eq!(streamed.usage.input, Some(55));
    let bee::llm::CacheUsage::Anthropic {
        read,
        created,
        ephemeral_5m,
        ephemeral_1h,
    } = &streamed.usage.cache[0]
    else {
        panic!("native cache usage expected")
    };
    assert_eq!(read.tokens, Some(30));
    assert_eq!(created.tokens, Some(20));
    assert_eq!(created.reports, 1);
    assert_eq!(ephemeral_5m.tokens, Some(20));
    assert_eq!(ephemeral_1h.tokens, Some(0));
}

#[tokio::test]
async fn native_tool_errors_and_success_preserve_anthropic_flags_and_call_ids() {
    let blocks = vec![
        json!({"type":"tool_use","id":"failure","name":"echo","input":{"text":"fail"}}),
        json!({"type":"tool_use","id":"success","name":"echo","input":{"text":"yes"}}),
    ];
    let server = Server::start(vec![
        Response::sse(stream(blocks.clone(), "tool_use")),
        Response::json(answer("done")),
    ]);
    let model = Model::new(
        profile("anthropic-messages", &server.url, ""),
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
    server.request();
    let request = server.request();
    assert_eq!(request.body["messages"][1]["content"], json!(blocks));
    let results = &request.body["messages"][2]["content"];
    assert_eq!(results[0]["tool_use_id"], "failure");
    assert_eq!(results[0]["is_error"], true);
    let failure: Value = serde_json::from_str(results[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(failure["error"]["code"], "execution");
    assert_eq!(results[1]["tool_use_id"], "success");
    assert_eq!(results[1]["is_error"], false);
    let success: Value = serde_json::from_str(results[1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(success, json!({"data":"yes"}));
}

#[tokio::test]
async fn native_text_streaming_sets_authentication_version_limits_and_usage() {
    let server = Server::start(vec![Response::sse(stream(
        vec![json!({"type":"text","text":"héllo"})],
        "end_turn",
    ))]);
    let profile = profile(
        "anthropic-messages",
        &server.url,
        "max_output_tokens=321\nheaders={x-team='custom'}",
    );
    let model = Model::new(
        profile,
        "private-model".into(),
        Some(bee::auth::Secret::new("key")),
    )
    .unwrap();
    let mut text = String::new();
    let step = model
        .step(&[Message::User("hello".into())], &[], &mut |chunk| {
            text.push_str(chunk)
        })
        .await
        .unwrap();
    assert_eq!(text, "héllo");
    assert_eq!(step.stop, Stop::End);
    assert_eq!(step.usage.input, Some(8));
    assert_eq!(step.usage.output, Some(4));
    let bee::llm::CacheUsage::Anthropic { read, created, .. } = &step.usage.cache[0] else {
        panic!("native cache usage expected")
    };
    assert_eq!(read.tokens, Some(3));
    assert_eq!(read.reports, 1);
    assert_eq!(created.tokens, None);
    let request = server.request();
    assert!(request.head.starts_with("POST /prefix/v1/messages "));
    assert!(request.head.contains("x-api-key: key"));
    assert!(!request.head.contains("authorization:"));
    assert!(request.head.contains("anthropic-version: 2023-06-01"));
    assert!(request.head.contains("x-team: custom"));
    assert_eq!(request.body["model"], "private-model");
    assert_eq!(request.body["max_tokens"], 321);
    assert_eq!(request.body["messages"][0]["content"][0]["text"], "hello");
}

#[tokio::test]
async fn multiple_tool_results_follow_the_original_native_blocks_in_one_user_message() {
    let blocks = vec![
        json!({"type":"text","text":"using tools"}),
        json!({"type":"tool_use","id":"call-a","name":"echo","input":{"text":"one"}}),
        json!({"type":"tool_use","id":"call-b","name":"echo","input":{"text":"two"}}),
    ];
    let server = Server::start(vec![
        Response::sse(stream(blocks.clone(), "tool_use")),
        Response::json(answer("done")),
    ]);
    let model = Model::new(
        profile("anthropic-messages", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let mut text = String::new();
    let result = bee::agent::run(
        &model,
        vec![Message::User("echo".into())],
        &[tool()],
        Some(&Echo),
        &mut |chunk| text.push_str(chunk),
    )
    .await
    .unwrap();
    assert_eq!(text, "using toolsdone");
    assert_eq!(result.messages.len(), 5);
    assert_eq!(
        server.request().body["tools"][0]["input_schema"],
        tool().parameters
    );
    let request = server.request();
    assert_eq!(request.body["messages"][1]["content"], json!(blocks));
    assert_eq!(request.body["messages"][2]["role"], "user");
    assert_eq!(
        request.body["messages"][2]["content"],
        json!([
            {"type":"tool_result","tool_use_id":"call-a","content":"one","is_error":false},
            {"type":"tool_result","tool_use_id":"call-b","content":"two","is_error":false}
        ])
    );
}

#[tokio::test]
async fn native_options_override_the_common_limit_and_default_version() {
    let root = tempfile::tempdir().unwrap();
    let server = Server::start(vec![Response::json(answer("okay"))]);
    let path = bee::config::project_config_path(root.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("[providers.test]\nmodel='opaque'\nmax_output_tokens=100\n[providers.test.protocols.anthropic-messages]\nbase_url='{}'\noptions={{version='2023-06-01',max_tokens=55}}", server.url)).unwrap();
    let env = bee::config::Environment::new(None, None, None);
    let profiles = bee::provider::Providers::from_config(
        &bee::config::Config::load(&env, root.path()).unwrap(),
    )
    .unwrap();
    let model = Model::new(profiles.get("test").unwrap().clone(), "opaque".into(), None).unwrap();
    model.step(&[], &[], &mut |_| {}).await.unwrap();
    let request = server.request();
    assert_eq!(request.body["max_tokens"], 55);
    assert!(request.head.contains("anthropic-version: 2023-06-01"));
}

#[tokio::test]
async fn token_limits_and_refusal_are_not_ordinary_end_turns() {
    for (reason, expected) in [
        ("max_tokens", Stop::MaxTokens),
        ("refusal", Stop::Refusal),
        ("stop_sequence", Stop::End),
    ] {
        let mut response = answer("partial");
        response["stop_reason"] = reason.into();
        let server = Server::start(vec![Response::json(response)]);
        let model = Model::new(
            profile("anthropic-messages", &server.url, ""),
            "opaque".into(),
            None,
        )
        .unwrap();
        assert_eq!(
            model.step(&[], &[], &mut |_| {}).await.unwrap().stop,
            expected
        );
    }
}

#[tokio::test]
async fn native_provider_errors_are_classified_without_echoing_their_messages() {
    for (kind, expected) in [
        ("authentication_error", Error::Authentication),
        ("rate_limit_error", Error::RateLimit),
        ("overloaded_error", Error::Provider),
    ] {
        let server = Server::start(vec![Response::sse(event(
            "error",
            json!({"type":"error","error":{"type":kind,"message":"sentinel"}}),
        ))]);
        let model = Model::new(
            profile("anthropic-messages", &server.url, ""),
            "opaque".into(),
            None,
        )
        .unwrap();
        let error = model.step(&[], &[], &mut |_| {}).await.unwrap_err();
        assert_eq!(error, expected);
        assert!(!format!("{error:?} {error}").contains("sentinel"));
    }
}

#[tokio::test]
async fn thinking_unknown_blocks_and_truncated_streams_fail_explicitly() {
    let mut thinking = answer("");
    thinking["content"] = json!([{"type":"thinking","thinking":"sentinel","signature":"secret"}]);
    let truncated = stream(vec![json!({"type":"text","text":"partial"})], "end_turn").replace(
        "event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n",
        "",
    );
    for (response, expected) in [
        (
            Response::json(thinking),
            Error::Unsupported("Anthropic content block"),
        ),
        (Response::sse(truncated), Error::Malformed),
    ] {
        let server = Server::start(vec![response]);
        let model = Model::new(
            profile("anthropic-messages", &server.url, ""),
            "opaque".into(),
            None,
        )
        .unwrap();
        assert_eq!(
            model.step(&[], &[], &mut |_| {}).await.unwrap_err(),
            expected
        );
    }
}

#[tokio::test]
async fn an_open_tool_block_is_not_executed_even_if_a_message_stop_arrives() {
    let wire = stream(
        vec![json!({"type":"tool_use","id":"call-1","name":"echo","input":{"text":"never"}})],
        "tool_use",
    )
    .replace(
        &event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        "",
    );
    assert!(!wire.contains("content_block_stop"));
    let server = Server::start(vec![Response::sse(wire)]);
    let model = Model::new(
        profile("anthropic-messages", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let error = bee::agent::run(
        &model,
        vec![Message::User("echo".into())],
        &[tool()],
        Some(&Echo),
        &mut |_| {},
    )
    .await
    .unwrap_err();
    let request = server.request();
    assert_eq!(request.body["messages"][0]["role"], "user");
    assert_eq!(error, Error::Malformed);
}

#[tokio::test]
async fn cancellation_after_an_anthropic_delta_drops_the_unfinished_message() {
    let wire = stream(vec![json!({"type":"text","text":"partial"})], "end_turn");
    let pause_at = wire.find("event: content_block_stop").unwrap();
    let mut response = Response::sse(wire);
    response.pause = Some((pause_at, std::time::Duration::from_millis(100)));
    let server = Server::start(vec![response]);
    let model = Model::new(
        profile("anthropic-messages", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let (abort, registration) = futures::future::AbortHandle::new_pair();
    let mut text = String::new();
    let mut sink = |chunk: &str| {
        text.push_str(chunk);
        abort.abort();
    };
    let result =
        futures::future::Abortable::new(model.step(&[], &[], &mut sink), registration).await;
    assert!(result.is_err());
    assert_eq!(text, "partial");
}

#[tokio::test]
async fn a_model_that_keeps_requesting_tools_hits_the_bounded_request_budget() {
    let responses = (0..16).map(|index| Response::json(json!({"type":"message","role":"assistant","content":[{"type":"tool_use","id":format!("call-{index}"),"name":"echo","input":{"text":"again"}}],"stop_reason":"tool_use"}))).collect();
    let server = Server::start(responses);
    let model = Model::new(
        profile("anthropic-messages", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    assert_eq!(
        bee::agent::run(
            &model,
            vec![Message::User("echo".into())],
            &[tool()],
            Some(&Echo),
            &mut |_| {}
        )
        .await
        .unwrap_err(),
        Error::Limit
    );
    for _ in 0..16 {
        server.request();
    }
}

#[tokio::test]
async fn cancelling_during_injected_tool_execution_drops_the_executor_future() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    struct Waiting {
        started: tokio::sync::mpsc::UnboundedSender<()>,
        dropped: Arc<AtomicBool>,
    }
    impl bee::agent::Executor for Waiting {
        fn execute<'a>(
            &'a self,
            _call: &'a bee::llm::Call,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<bee::agent::ToolResult, Error>> + Send + 'a,
            >,
        > {
            Box::pin(async move {
                let _drop = Dropped(Arc::clone(&self.dropped));
                self.started.send(()).unwrap();
                std::future::pending().await
            })
        }
    }
    let mut response = answer("");
    response["content"] = json!([{"type":"tool_use","id":"call-1","name":"echo","input":{}}]);
    response["stop_reason"] = "tool_use".into();
    let server = Server::start(vec![Response::json(response)]);
    let model = Model::new(
        profile("anthropic-messages", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let (started, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let executor = Waiting {
        started,
        dropped: Arc::clone(&dropped),
    };
    let (abort, registration) = futures::future::AbortHandle::new_pair();
    let tools = [tool()];
    let mut sink = |_: &str| {};
    let run = futures::future::Abortable::new(
        bee::agent::run(
            &model,
            vec![Message::User("echo".into())],
            &tools,
            Some(&executor),
            &mut sink,
        ),
        registration,
    );
    let cancel = async {
        rx.recv().await.unwrap();
        abort.abort();
    };
    let (result, _) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        futures::join!(run, cancel)
    })
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_tool_failure_is_returned_to_the_model_as_a_native_error_result() {
    struct Failing;
    impl bee::agent::Executor for Failing {
        fn execute<'a>(
            &'a self,
            _call: &'a bee::llm::Call,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<bee::agent::ToolResult, Error>> + Send + 'a,
            >,
        > {
            Box::pin(async {
                Ok(bee::agent::ToolResult {
                    output: "tool could not finish".into(),
                    is_error: true,
                })
            })
        }
    }
    let response = json!({"type":"message","role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"echo","input":{}}],"stop_reason":"tool_use"});
    let server = Server::start(vec![
        Response::json(response),
        Response::json(answer("handled")),
    ]);
    let model = Model::new(
        profile("anthropic-messages", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    bee::agent::run(
        &model,
        vec![Message::User("echo".into())],
        &[tool()],
        Some(&Failing),
        &mut |_| {},
    )
    .await
    .unwrap();
    server.request();
    assert_eq!(
        server.request().body["messages"][2]["content"][0]["is_error"],
        true
    );
}
