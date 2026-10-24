//! Chat Completions through a real local HTTP connection.

mod support;

use bee::llm::{Error, Message, Model, Stop};
use serde_json::json;
use support::{Echo, Response, Server, chat_text, event, profile, tool};

#[tokio::test]
async fn a_custom_endpoint_receives_the_selected_model_limits_and_safe_headers() {
    let server = Server::start(vec![Response::json(chat_text("answer"))]);
    let model = Model::new(
        profile(
            "openai-chat",
            &server.url,
            "max_output_tokens=123\n[providers.test.headers]\nx-custom='private'",
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
            json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":4}})
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
}

#[tokio::test]
async fn failures_are_classified_without_echoing_the_server_body() {
    for (status, expected) in [
        (401, Error::Authentication),
        (429, Error::RateLimit),
        (500, Error::Http(500)),
    ] {
        let mut response = Response::json(json!({"secret":"sentinel"}));
        response.status = status;
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
async fn an_unsolicited_tool_call_cannot_execute_in_the_production_scope() {
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
        "prefer=['anthropic-messages','openai-chat']\n[providers.test.protocols.openai-chat]\nbase_url='http://127.0.0.1:1'",
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
