//! Credential-free tests of native targets, protocol adapters and account isolation.

use crate::auth::Secret;
use crate::llm::{Error, Message, Model, Stop};
use crate::test_support::{Response, Server, configured_providers};
use serde_json::json;

fn model(account: &str, gateway: &str, protocol: &str, raw_model: &str, server: &Server) -> Model {
    let providers = configured_providers(&format!(
        "[accounts.work]\nprovider='cloudflare'\nsettings={{account_id='{account}',gateway_id='{gateway}'}}\nprotocol='{protocol}'\nmodels=['{raw_model}']\ncontext_window=32000"
    ));
    Model::new(
        providers.get("work").unwrap().clone(),
        raw_model.into(),
        Some(Secret::new(format!("token-{account}"))),
    )
    .unwrap()
    .with_test_http(crate::http::InferenceHttp::test_routed(server.url.clone()))
}

fn chat(text: &str) -> Response {
    Response::sse(format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}]})
    ))
}

#[tokio::test]
async fn all_cloudflare_protocols_use_the_selected_account_gateway_and_bearer_token() {
    let responses = vec![
        chat("chat answer"),
        Response::json(
            json!({"status":"completed","output":[{"id":"message-1","type":"message","role":"assistant","content":[{"type":"output_text","text":"responses answer"}]}]}),
        ),
        Response::json(
            json!({"id":"message-2","type":"message","role":"assistant","content":[{"type":"text","text":"messages answer"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":2}}),
        ),
    ];
    let server = Server::start(responses);
    for (account, gateway, protocol, suffix, expected) in [
        (
            "work-account",
            "work-gateway",
            "openai-chat",
            "chat/completions",
            "chat answer",
        ),
        (
            "personal-account",
            "personal-gateway",
            "openai-responses",
            "responses",
            "responses answer",
        ),
        (
            "third-account",
            "third-gateway",
            "anthropic-messages",
            "messages",
            "messages answer",
        ),
    ] {
        let model = model(
            account,
            gateway,
            protocol,
            "anthropic/private-model",
            &server,
        );
        let mut text = String::new();
        let step = model
            .step(&[Message::User("hello".into())], &[], &mut |part| {
                text.push_str(part)
            })
            .await
            .unwrap();
        assert_eq!(step.stop, Stop::End);
        assert_eq!(text, expected);
        let request = server.request();
        assert!(request.head.starts_with(&format!(
            "POST /prefix/client/v4/accounts/{account}/ai/v1/{suffix} "
        )));
        assert!(
            request
                .head
                .contains(&format!("authorization: Bearer token-{account}"))
        );
        assert!(
            request
                .head
                .contains(&format!("cf-aig-gateway-id: {gateway}"))
        );
        assert!(!request.head.contains("x-api-key"));
        assert_eq!(request.body["model"], "anthropic/private-model");
        assert!(!request.body.to_string().contains(account));
        assert!(!request.body.to_string().contains(gateway));
        let headers = model.headers(None).unwrap();
        assert!(headers["authorization"].is_sensitive());
        assert!(headers["cf-aig-gateway-id"].is_sensitive());
    }
    server.assert_no_more_requests();
}

#[tokio::test]
async fn workers_ai_responses_uses_complete_json_and_rejects_incompatible_routes_before_http() {
    let server = Server::start(vec![Response::json(
        json!({"status":"completed","output":[{"id":"message-1","type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}),
    )]);
    let valid = model(
        "account",
        "gateway",
        "openai-responses",
        "@cf/openai/gpt-oss-120b",
        &server,
    );
    valid
        .step(&[Message::User("hello".into())], &[], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(server.request().body["stream"], false);
    for protocol in ["anthropic-messages", "openai-responses"] {
        let invalid = model("account", "gateway", protocol, "@cf/meta/llama", &server);
        assert!(matches!(
            invalid.step(&[], &[], &mut |_| {}).await,
            Err(Error::Unsupported(_))
        ));
    }
    server.assert_no_more_requests();
}

#[tokio::test]
async fn cloudflare_status_failures_never_echo_response_bodies_or_retry_another_account() {
    for (status, expected) in [
        (401, Error::Authentication),
        (403, Error::Authentication),
        (429, Error::RateLimit),
    ] {
        let mut response = Response::json(
            json!({"errors":[{"code":10000,"message":"token-private-account private-gateway"}]}),
        );
        response.status = status;
        let server = Server::start(vec![response]);
        let model = model(
            "private-account",
            "private-gateway",
            "openai-chat",
            "vendor/model",
            &server,
        );
        let error = model
            .step(&[Message::User("hello".into())], &[], &mut |_| {})
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        assert!(!format!("{error:?} {error}").contains("private-account"));
        server.request();
        server.assert_no_more_requests();
    }
}

#[tokio::test]
async fn cloudflare_chat_retains_raw_tool_calls_and_results_across_steps() {
    let server = Server::start(vec![
        Response::sse(format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"read-1","type":"function","function":{"name":"read","arguments":"{\"path\":\"file.txt\"}"}}]},"finish_reason":"tool_calls"}]})
        )),
        chat("done"),
    ]);
    let model = model("account", "gateway", "openai-chat", "vendor/model", &server);
    let mut messages = vec![Message::User("read a file".into())];
    let step = model.step(&messages, &[], &mut |_| {}).await.unwrap();
    assert_eq!(step.stop, Stop::Tools);
    messages.push(step.message);
    messages.push(Message::Tool {
        id: "read-1".into(),
        output: "file payload".into(),
        is_error: false,
    });
    assert_eq!(
        model.step(&messages, &[], &mut |_| {}).await.unwrap().stop,
        Stop::End
    );
    server.request();
    let body = server.request().body;
    assert!(body["messages"].to_string().contains("file payload"));
    assert_eq!(body["messages"][2]["tool_call_id"], "read-1");
    server.assert_no_more_requests();
}
