//! Exercise the real Chat adapter against local fixtures, without live keys or billing.

use crate::auth::Secret;
use crate::llm::{Error, Message, Model, Stop};
use crate::test_support::{Response, Server, configured_providers};
use serde_json::json;

fn model(settings: &str, options: &str, key: &str, server: &Server) -> Model {
    let accounts = configured_providers(&format!(
        "[accounts.work]\nprovider='scaleway'\nsettings={{{settings}}}\nprotocol='openai-chat'\nmodels=['raw/model']\ncontext_window=32000\n{options}"
    ));
    Model::new(
        accounts.get("work").unwrap().clone(),
        "raw/model".into(),
        Some(Secret::new(key)),
    )
    .unwrap()
    .with_test_http(crate::http::InferenceHttp::test_routed(server.url.clone()))
}

fn chat(text: &str) -> Response {
    Response::sse(format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":3,"total_tokens":11}})
    ))
}

#[tokio::test]
async fn scaleway_chat_preserves_project_scope_and_bearer_authority_without_private_body_fields() {
    let server = Server::start(vec![chat("first"), chat("second"), chat("default")]);
    for (project, key, answer) in [
        (
            Some("78e655b5-feb0-417c-bb3f-8c448bd0e8da"),
            "private-work-key",
            "first",
        ),
        (
            Some("78e655b5-feb0-417c-bb3f-8c448bd0e8db"),
            "private-personal-key",
            "second",
        ),
        (None, "private-default-key", "default"),
    ] {
        let settings = project
            .map(|p| format!("project_id='{p}',region='fr-par'"))
            .unwrap_or_default();
        let model = model(&settings, "", key, &server);
        let mut text = String::new();
        let step = model
            .step(&[Message::User("hello".into())], &[], &mut |part| {
                text.push_str(part)
            })
            .await
            .unwrap();
        assert_eq!(step.stop, Stop::End);
        assert_eq!(step.usage.input, Some(8));
        assert_eq!(step.usage.output, Some(3));
        assert_eq!(text, answer);
        let request = server.request();
        let prefix = project.map(|p| format!("/{p}")).unwrap_or_default();
        assert!(
            request
                .head
                .starts_with(&format!("POST /prefix{prefix}/v1/chat/completions "))
        );
        assert!(
            request
                .head
                .contains(&format!("authorization: Bearer {key}"))
        );
        assert!(model.headers(None).unwrap()["authorization"].is_sensitive());
        assert_eq!(request.body["model"], "raw/model");
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["stream_options"]["include_usage"], true);
        for field in [
            "project_id",
            "region",
            "prompt_cache_key",
            "prompt_cache_retention",
            "prompt_cache_options",
            "store",
            "previous_response_id",
        ] {
            assert!(request.body.get(field).is_none(), "{field}");
        }
        assert!(!request.body.to_string().contains(key));
        if let Some(project) = project {
            assert!(!request.body.to_string().contains(project));
        }
    }
    server.assert_no_more_requests();
}

#[tokio::test]
async fn scaleway_failures_never_echo_sensitive_bodies_or_retry_other_projects() {
    for (status, expected) in [
        (401, Error::Authentication),
        (403, Error::Authentication),
        (429, Error::RateLimit),
    ] {
        let mut response =
            Response::json(json!({"error":{"message":"private-key private-project"}}));
        response.status = status;
        let server = Server::start(vec![response]);
        let model = model("", "", "private-key", &server);
        let error = model
            .step(&[Message::User("hello".into())], &[], &mut |_| {})
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        assert!(!format!("{error:?} {error}").contains("private-"));
        server.request();
        server.assert_no_more_requests();
    }
}

#[tokio::test]
async fn scaleway_rejects_unsupported_cache_controls_before_http() {
    let server = Server::start(vec![]);
    let model = model(
        "",
        "options={prompt_cache_key={mode='explicit',value='private-cache'}}",
        "private-key",
        &server,
    );
    assert!(matches!(
        model.step(&[], &[], &mut |_| {}).await,
        Err(Error::Cache(_))
    ));
    server.assert_no_more_requests();
}

#[tokio::test]
async fn scaleway_function_calls_round_trip_as_chat_tool_results() {
    let server = Server::start(vec![
        Response::sse(format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"lookup","arguments":"{\"query\":\"hello\"}"}}]},"finish_reason":"tool_calls"}]})
        )),
        chat("answer"),
    ]);
    let model = model(
        "project_id='78e655b5-feb0-417c-bb3f-8c448bd0e8da'",
        "",
        "private-key",
        &server,
    );
    let tools = vec![crate::llm::Tool {
        name: "lookup".into(),
        description: "Find a value".into(),
        parameters: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
    }];
    let mut messages = vec![Message::User("hello".into())];
    let step = model.step(&messages, &tools, &mut |_| {}).await.unwrap();
    assert_eq!(step.stop, Stop::Tools);
    let Message::Assistant { calls, .. } = &step.message else {
        panic!("expected assistant tool call")
    };
    assert_eq!(calls[0].name, "lookup");
    assert_eq!(calls[0].arguments, json!({"query":"hello"}));
    messages.push(step.message);
    messages.push(Message::Tool {
        id: "call-1".into(),
        output: "found".into(),
        is_error: false,
    });
    assert_eq!(
        model
            .step(&messages, &tools, &mut |_| {})
            .await
            .unwrap()
            .stop,
        Stop::End
    );
    let first = server.request();
    let second = server.request();
    assert_eq!(first.body["tools"][0]["type"], "function");
    assert_eq!(second.body["messages"][1]["tool_calls"][0]["id"], "call-1");
    assert_eq!(second.body["messages"][2]["role"], "tool");
    assert_eq!(second.body["messages"][2]["tool_call_id"], "call-1");
    assert_eq!(second.body["messages"][2]["content"], "found");
    assert_eq!(first.head.lines().next(), second.head.lines().next());
    assert!(second.head.contains("authorization: Bearer private-key"));
    server.assert_no_more_requests();
}
