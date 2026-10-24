//! Context budgets preserve whole turns and distinguish estimates from actual usage.

mod support;

use bee::agent::{Limits, run_with_limits};
use bee::llm::{Call, Error, Message, Model, Native};
use bee::provider::Protocol;
use serde_json::{Value, json};
use support::{Echo, Response, Server, chat_text, event, profile, tool};

fn assistant(text: &str) -> Message {
    Message::Assistant {
        text: text.into(),
        calls: vec![],
        native: None,
    }
}

fn tool_batch(protocol: &str, ids: &[&str]) -> Message {
    let calls: Vec<_> = ids
        .iter()
        .map(|id| Call {
            id: (*id).into(),
            name: "echo".into(),
            arguments: json!({"text":"hi"}),
        })
        .collect();
    let native = match protocol {
        "openai-responses" => {
            let mut items = vec![json!({"id":"reasoning","type":"reasoning","encrypted_content":"retained-opaque-reasoning"})];
            items.extend(calls.iter().map(|call| json!({"type":"function_call","id":format!("item-{}", call.id),"call_id":call.id,"name":call.name,"arguments":call.arguments.to_string()})));
            Some(Native { protocol: Protocol::OpenAiResponses, items })
        }
        "anthropic-messages" => Some(Native { protocol: Protocol::AnthropicMessages, items: calls.iter().map(|call| json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments})).collect() }),
        _ => None,
    };
    Message::Assistant {
        text: String::new(),
        calls,
        native,
    }
}

fn answer(protocol: &str) -> Value {
    match protocol {
        "openai-chat" => chat_text("done"),
        "openai-responses" => {
            json!({"status":"completed","output":[{"id":"answer","type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]})
        }
        _ => {
            json!({"type":"message","role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"})
        }
    }
}

fn model(server: &Server, protocol: &str, settings: &str) -> Model {
    Model::new(
        profile(protocol, &server.url, settings),
        "private-model".into(),
        None,
    )
    .unwrap()
}

#[rstest::rstest]
#[case("openai-chat")]
#[case("openai-responses")]
#[case("anthropic-messages")]
#[tokio::test]
async fn persistent_instructions_are_counted_once_and_cannot_be_evicted(#[case] protocol: &str) {
    let server = Server::start(vec![Response::json(answer(protocol))]);
    let instructions = "persistent-guidance ".repeat(500);
    let messages = vec![Message::User("question".into())];
    let report = model(&server, protocol, "max_output_tokens=10")
        .with_instructions(instructions.clone())
        .context(&messages, &[])
        .unwrap();
    let window = report.estimated_input + report.reserved_output;
    model(
        &server,
        protocol,
        &format!("max_output_tokens=10\ncontext_window={window}"),
    )
    .with_instructions(instructions.clone())
    .step(&messages, &[], &mut |_| {})
    .await
    .unwrap();
    let body = server.request().body;
    let bytes: usize = ["messages", "input", "tools", "system", "instructions"]
        .into_iter()
        .filter_map(|key| body.get(key))
        .map(|value| value.to_string().len())
        .sum();
    assert_eq!(report.estimated_input, (bytes as u64).div_ceil(3) + 256);
    assert_eq!(body.to_string().matches("persistent-guidance").count(), 500);
    let error = run_with_limits(
        &model(
            &server,
            protocol,
            &format!("max_output_tokens=10\ncontext_window={}", window - 1),
        )
        .with_instructions(instructions),
        messages,
        &[],
        None,
        &mut |_| {},
        Limits::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, Error::ContextLimit);
    server.assert_no_more_requests();
}

#[rstest::rstest]
#[case("openai-chat")]
#[case("openai-responses")]
#[case("anthropic-messages")]
#[tokio::test]
async fn only_oldest_complete_turns_are_removed_and_tool_pairs_remain(#[case] protocol: &str) {
    let server = Server::start(vec![Response::json(answer(protocol))]);
    let recent = vec![
        Message::User("recent".into()),
        tool_batch(protocol, &["paired", "paired-two"]),
        Message::Tool {
            id: "paired".into(),
            output: "hi".into(),
            is_error: false,
        },
        Message::Tool {
            id: "paired-two".into(),
            output: "failed".into(),
            is_error: true,
        },
        tool_batch(protocol, &["next-round"]),
        Message::Tool {
            id: "next-round".into(),
            output: "round-two".into(),
            is_error: false,
        },
        assistant("tool answer"),
        Message::User("current".into()),
    ];
    let unbounded = model(&server, protocol, "max_output_tokens=20");
    let estimate = unbounded.context(&recent, &[tool()]).unwrap();
    let window = estimate.estimated_input + estimate.reserved_output;
    let bounded = model(
        &server,
        protocol,
        &format!("max_output_tokens=20\ncontext_window={window}"),
    );
    let mut history = vec![
        Message::User("old".repeat(1000)),
        tool_batch(protocol, &["old-call"]),
        Message::Tool {
            id: "old-call".into(),
            output: "old-result".into(),
            is_error: false,
        },
        assistant("old answer"),
    ];
    history.extend(recent.clone());
    let outcome = run_with_limits(
        &bounded,
        history,
        &[tool()],
        Some(&Echo),
        &mut |_| {},
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.context.removed_turns, 1);
    assert_eq!(&outcome.messages[..recent.len()], &recent);
    let wire = server.request().body.to_string();
    assert!(!wire.contains("old answer"));
    assert!(!wire.contains("old-call"));
    assert!(!wire.contains("old-result"));
    assert!(wire.contains("paired"));
    assert!(wire.contains("tool answer"));
    assert!(wire.contains("next-round"));
    if protocol == "openai-responses" {
        assert!(wire.contains("retained-opaque-reasoning"));
    }
    assert_eq!(outcome.context.steps, 1);
    assert_eq!(
        outcome.context.input_steps,
        usize::from(protocol == "openai-chat")
    );
}

#[tokio::test]
async fn exact_fit_is_sent_but_one_token_less_is_rejected_without_http() {
    let server = Server::start(vec![Response::json(chat_text("done"))]);
    let messages = vec![Message::User("日本語 and JSON: {\"key\":\"value\"}".into())];
    let report = model(&server, "openai-chat", "max_output_tokens=10")
        .context(&messages, &[tool()])
        .unwrap();
    let window = report.estimated_input + report.reserved_output;
    let bounded = model(
        &server,
        "openai-chat",
        &format!("max_output_tokens=10\ncontext_window={window}"),
    );
    bounded
        .step(&messages, &[tool()], &mut |_| {})
        .await
        .unwrap();
    server.request();
    let smaller = model(
        &server,
        "openai-chat",
        &format!("max_output_tokens=10\ncontext_window={}", window - 1),
    );
    assert_eq!(
        smaller
            .step(&messages, &[tool()], &mut |_| {})
            .await
            .unwrap_err(),
        Error::ContextLimit
    );
    server.assert_no_more_requests();
}

#[tokio::test]
async fn growing_tool_results_cannot_evict_or_split_the_active_turn() {
    let calls = json!({"choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call","type":"function","function":{"name":"echo","arguments":json!({"text":"large".repeat(1000)}).to_string()}}]},"finish_reason":"tool_calls"}]});
    let server = Server::start(vec![Response::json(calls)]);
    let bounded = model(
        &server,
        "openai-chat",
        "max_output_tokens=10\ncontext_window=1000",
    );
    let error = run_with_limits(
        &bounded,
        vec![Message::User("go".into())],
        &[tool()],
        Some(&Echo),
        &mut |_| {},
        Limits::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, Error::ContextLimit);
    server.request();
    server.assert_no_more_requests();
}

#[tokio::test]
async fn malformed_historical_tool_batches_are_never_silently_dropped() {
    let server = Server::start(vec![]);
    let bounded = model(
        &server,
        "openai-chat",
        "max_output_tokens=10\ncontext_window=400",
    );
    let history = vec![
        Message::User("old".repeat(1000)),
        Message::Tool {
            id: "orphan".into(),
            output: "result".into(),
            is_error: false,
        },
        Message::User("current".into()),
    ];
    assert_eq!(
        run_with_limits(&bounded, history, &[], None, &mut |_| {}, Limits::default())
            .await
            .unwrap_err(),
        Error::Malformed
    );
    server.assert_no_more_requests();
}

#[test]
fn native_replay_is_counted_once_and_anthropic_output_overrides_are_reserved() {
    let server = Server::start(vec![]);
    for (protocol, native_protocol, items) in [
        (
            "openai-responses",
            Protocol::OpenAiResponses,
            vec![json!({"id":"r","type":"reasoning","encrypted_content":"opaque"})],
        ),
        (
            "anthropic-messages",
            Protocol::AnthropicMessages,
            vec![json!({"type":"text","text":"native"})],
        ),
    ] {
        let model = model(&server, protocol, "max_output_tokens=10");
        let mut messages = vec![
            Message::User("hi".into()),
            Message::Assistant {
                text: "ignored".repeat(1000),
                calls: vec![],
                native: Some(Native {
                    protocol: native_protocol,
                    items,
                }),
            },
        ];
        let native = model.context(&messages, &[]).unwrap();
        let Message::Assistant {
            native: continuation,
            ..
        } = &mut messages[1]
        else {
            unreachable!()
        };
        *continuation = None;
        assert!(model.context(&messages, &[]).unwrap().estimated_input > native.estimated_input);
    }
    let profiles = support::configured_providers(&format!(
        "[providers.test]\nmodel='m'\nmax_output_tokens=100\n[providers.test.protocols.anthropic-messages]\nbase_url='{}'\noptions={{max_tokens=7}}",
        server.url
    ));
    let model = Model::new(profiles.get("test").unwrap().clone(), "m".into(), None).unwrap();
    assert_eq!(model.context(&[], &[]).unwrap().reserved_output, 7);
}

#[tokio::test]
async fn tools_output_reservations_and_unpartitioned_library_inputs_cannot_bypass_limits() {
    let server = Server::start(vec![]);
    let bounded = model(
        &server,
        "openai-chat",
        "max_output_tokens=10\ncontext_window=400",
    );
    let mut large_tool = tool();
    large_tool.parameters = json!({"enum":["schema-value".repeat(1000)]});
    assert_eq!(
        run_with_limits(
            &bounded,
            vec![Message::User("go".into())],
            &[large_tool],
            Some(&Echo),
            &mut |_| {},
            Limits::default()
        )
        .await
        .unwrap_err(),
        Error::ContextLimit
    );
    assert_eq!(
        run_with_limits(
            &bounded,
            vec![assistant(&"unpartitioned".repeat(1000))],
            &[],
            None,
            &mut |_| {},
            Limits::default()
        )
        .await
        .unwrap_err(),
        Error::ContextLimit
    );
    let reserved = model(
        &server,
        "openai-chat",
        "max_output_tokens=500\ncontext_window=400",
    );
    assert_eq!(
        reserved.step(&[], &[], &mut |_| {}).await.unwrap_err(),
        Error::ContextLimit
    );
    server.assert_no_more_requests();
    let report = bee::context::Report {
        estimated_input: u64::MAX,
        reserved_output: u64::MAX,
        window: Some(u32::MAX),
        ..Default::default()
    };
    assert_eq!(report.check(), Err(Error::ContextLimit));
}

#[rstest::rstest]
#[case("openai-chat", json!({"error":{"code":"context_length_exceeded","message":"secret"}}))]
#[case("openai-responses", json!({"error":{"code":"context_length_exceeded","message":"secret"}}))]
#[case("anthropic-messages", json!({"error":{"type":"invalid_request_error","message":"prompt is too long: secret"}}))]
#[tokio::test]
async fn provider_http_context_errors_are_explicit_and_redacted(
    #[case] protocol: &str,
    #[case] body: Value,
) {
    let mut response = Response::json(body);
    response.status = 400;
    let server = Server::start(vec![response]);
    let error = model(&server, protocol, "")
        .step(&[Message::User("hi".into())], &[], &mut |_| {})
        .await
        .unwrap_err();
    assert_eq!(error, Error::ContextLimit);
    assert!(!format!("{error:?} {error}").contains("secret"));
    server.request();
    server.assert_no_more_requests();
}

#[rstest::rstest]
#[case("openai-chat", Response::sse(event("", json!({"error":{"code":"context_length_exceeded"}}))))]
#[case("openai-responses", Response::sse(event("response.failed", json!({"type":"response.failed","response":{"error":{"code":"context_length_exceeded"}}}))))]
#[case("openai-responses", Response::json(json!({"status":"failed","error":{"code":"context_length_exceeded"}})))]
#[case("anthropic-messages", Response::sse(event("error", json!({"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: secret"}}))))]
#[case("anthropic-messages", Response::json(json!({"type":"message","role":"assistant","content":[],"stop_reason":"model_context_window_exceeded"})))]
#[tokio::test]
async fn streamed_and_terminal_context_exhaustion_is_not_an_output_limit(
    #[case] protocol: &str,
    #[case] response: Response,
) {
    let server = Server::start(vec![response]);
    assert_eq!(
        model(&server, protocol, "")
            .step(&[Message::User("hi".into())], &[], &mut |_| {})
            .await
            .unwrap_err(),
        Error::ContextLimit
    );
    server.request();
    server.assert_no_more_requests();
}

#[rstest::rstest]
#[case(
    400,
    "{\"error\":{\"type\":\"invalid_request_error\",\"message\":\"unrelated secret\"}}"
)]
#[case(413, "secret")]
#[case(400, "{invalid secret")]
#[case(400, "oversized")]
#[tokio::test]
async fn unrelated_or_unparsable_provider_errors_keep_the_http_status(
    #[case] status: u16,
    #[case] body: &str,
) {
    let mut response = Response::json(Value::Null);
    response.status = status;
    response.body = if body == "oversized" {
        "secret".repeat(12000)
    } else {
        body.into()
    };
    let server = Server::start(vec![response]);
    let error = model(&server, "anthropic-messages", "")
        .step(&[], &[], &mut |_| {})
        .await
        .unwrap_err();
    assert_eq!(error, Error::Http(status));
    assert!(!format!("{error:?} {error}").contains("secret"));
}

#[tokio::test]
async fn actual_usage_totals_identify_missing_steps_without_substituting_estimates() {
    let calls = json!({"choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"echo","arguments":"{\"text\":\"hi\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":0}});
    let server = Server::start(vec![
        Response::json(calls),
        Response::json(chat_text("done")),
    ]);
    let outcome = run_with_limits(
        &model(&server, "openai-chat", ""),
        vec![Message::User("go".into())],
        &[tool()],
        Some(&Echo),
        &mut |_| {},
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.usage.input, Some(3));
    assert_eq!(outcome.usage.output, Some(2));
    assert_eq!(outcome.context.input_steps, 2);
    assert_eq!(outcome.context.output_steps, 1);
    assert_eq!(outcome.context.steps, 2);
    assert_eq!(outcome.context.window, None);
    assert!(outcome.context.estimated_input > 3);
    assert_eq!(
        server.request().body["stream_options"]["include_usage"],
        true
    );
    server.request();
}
