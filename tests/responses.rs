//! Native Responses serialization and streaming lifecycle over HTTP.

mod support;

use bee::llm::{Error, Message, Model, Stop, Stream};
use serde_json::{Value, json};
use support::{Echo, Response, Server, event, profile, tool};

fn message(text: &str) -> Value {
    json!({"id":"msg-1","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]})
}

fn completed(items: Vec<Value>) -> Value {
    json!({"id":"response-1","status":"completed","output":items,"usage":{"input_tokens":5,"output_tokens":2,"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":1,"cache_write_tokens_details":{"ttl_30m_tokens":1,"ttl_12h_tokens":0}}}})
}

fn stream(items: Vec<Value>, delta: bool) -> String {
    let mut wire = event(
        "response.created",
        json!({"type":"response.created","response":{"id":"response-1","status":"in_progress"}}),
    );
    for (index, item) in items.iter().enumerate() {
        wire += &event(
            "response.output_item.added",
            json!({"type":"response.output_item.added","output_index":index,"item":{"id":item["id"],"type":item["type"]}}),
        );
        match item["type"].as_str().unwrap() {
            "message" if delta => {
                wire += &event(
                    "response.output_text.delta",
                    json!({"type":"response.output_text.delta","output_index":index,"item_id":item["id"],"content_index":0,"delta":item["content"][0]["text"]}),
                );
                wire += &event(
                    "response.output_text.done",
                    json!({"type":"response.output_text.done","output_index":index,"item_id":item["id"],"content_index":0,"text":item["content"][0]["text"]}),
                );
            }
            "function_call" => {
                wire += &event(
                    "response.function_call_arguments.delta",
                    json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":item["id"],"delta":item["arguments"]}),
                );
                wire += &event(
                    "response.function_call_arguments.done",
                    json!({"type":"response.function_call_arguments.done","output_index":index,"item_id":item["id"],"arguments":item["arguments"]}),
                );
            }
            _ => {}
        }
        wire += &event(
            "response.output_item.done",
            json!({"type":"response.output_item.done","output_index":index,"item":item}),
        );
    }
    wire + &event(
        "response.completed",
        json!({"type":"response.completed","response":completed(items)}),
    )
}

fn reasoning() -> Value {
    json!({"id":"private-provider-id","type":"reasoning","summary":[
        {"type":"summary_text","text":"héllo"},{"type":"summary_text","text":"second"}
    ],"encrypted_content":"private-encrypted-sentinel"})
}

fn reasoning_stream() -> String {
    let wire = stream(vec![reasoning(), message("answer")], true);
    let mut summaries = String::new();
    for (part, full) in ["héllo", "second"].iter().enumerate() {
        for (kind, fields) in [
            (
                "part.added",
                json!({"part":{"type":"summary_text","text":""}}),
            ),
            ("text.delta", json!({"delta":full})),
            ("text.done", json!({"text":full})),
            (
                "part.done",
                json!({"part":{"type":"summary_text","text":full}}),
            ),
        ] {
            let kind = format!("response.reasoning_summary_{kind}");
            let mut value = json!({"type":kind,"output_index":0,"item_id":"private-provider-id","summary_index":part});
            value
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            summaries += &event(&kind, value);
        }
    }
    // Raw plaintext reasoning is deliberately a separate, non-display surface.
    summaries += &event(
        "response.reasoning_text.delta",
        json!({"type":"response.reasoning_text.delta","output_index":0,"item_id":"private-provider-id","delta":"private-raw-sentinel"}),
    );
    let position = wire.find("event: response.output_item.done").unwrap();
    format!("{}{}{}", &wire[..position], summaries, &wire[position..])
}

#[tokio::test]
async fn summaries_stream_once_with_part_identity_and_keep_continuation_private() {
    for response in [
        Response::sse(reasoning_stream()),
        Response::sse(stream(vec![reasoning(), message("answer")], false)),
        Response::json(completed(vec![reasoning(), message("answer")])),
    ] {
        let server = Server::start(vec![response]);
        let model = Model::new(
            profile("openai-responses", &server.url, ""),
            "opaque".into(),
            None,
        )
        .unwrap();
        let mut visible = Vec::new();
        let step = model
            .step_stream(&[], &[], &mut |event| match event {
                Stream::Text(text) => visible.push((None, text.to_owned())),
                Stream::Thought { item, part, text } => {
                    visible.push((Some((item, part)), text.to_owned()))
                }
            })
            .await
            .unwrap();
        assert_eq!(
            visible,
            vec![
                (Some((0, 0)), "héllo".into()),
                (Some((0, 1)), "second".into()),
                (None, "answer".into())
            ]
        );
        let Message::Assistant { text, native, .. } = step.message else {
            panic!("assistant expected")
        };
        assert_eq!(text, "answer");
        assert_eq!(native.unwrap().items[0], reasoning());
        assert!(server.request().body.get("reasoning").is_none());
    }
}

#[tokio::test]
async fn malformed_summary_identity_lifecycle_and_final_prefix_fail_without_exposing_private_text()
{
    let valid = reasoning_stream();
    let done = valid.find("event: response.output_item.done").unwrap();
    let changed_final = format!(
        "{}{}",
        &valid[..done],
        valid[done..].replace("\"text\":\"héllo\"", "\"text\":\"héllo suffix\"")
    );
    for wire in [
        changed_final,
        valid.replace("\"summary_index\":0", "\"summary_index\":128"),
        valid.replace(
            "\"item_id\":\"private-provider-id\"",
            "\"item_id\":\"wrong\"",
        ),
        valid.replace("\"text\":\"héllo\"", "\"text\":\"different\""),
        valid.replace(
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_text.delta",
        ),
        valid[..valid.find("event: response.completed").unwrap()].to_owned(),
    ] {
        let server = Server::start(vec![Response::sse(wire)]);
        let model = Model::new(
            profile("openai-responses", &server.url, ""),
            "opaque".into(),
            None,
        )
        .unwrap();
        let mut visible = String::new();
        let error = model
            .step_stream(&[], &[], &mut |event| match event {
                Stream::Text(text) | Stream::Thought { text, .. } => visible.push_str(text),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Malformed | Error::Limit));
        assert!(!visible.contains("private"));
        assert!(!format!("{error:?} {error}").contains("private"));
    }
}

#[tokio::test]
async fn reasoning_parts_and_answer_chunks_can_interleave_without_changing_entity_identity() {
    let added = event(
        "response.output_item.added",
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"msg-1","type":"message"}}),
    );
    let delta = event(
        "response.output_text.delta",
        json!({"type":"response.output_text.delta","output_index":1,"item_id":"msg-1","content_index":0,"delta":"answer"}),
    );
    let mut wire = reasoning_stream().replace(&added, "").replace(&delta, "");
    let position = wire
        .find("event: response.reasoning_summary_part.done")
        .unwrap();
    wire.insert_str(position, &(added + &delta));
    let server = Server::start(vec![Response::sse(wire)]);
    let model = Model::new(
        profile("openai-responses", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let mut visible = Vec::new();
    model
        .step_stream(&[], &[], &mut |event| match event {
            Stream::Text(text) => visible.push((None, text.to_owned())),
            Stream::Thought { item, part, text } => {
                visible.push((Some((item, part)), text.to_owned()))
            }
        })
        .await
        .unwrap();
    assert_eq!(
        visible,
        vec![
            (Some((0, 0)), "héllo".into()),
            (None, "answer".into()),
            (Some((0, 1)), "second".into())
        ]
    );
}

#[tokio::test]
async fn streamed_text_is_not_duplicated_by_the_terminal_response() {
    let server = Server::start(vec![Response::sse(stream(vec![message("héllo")], true))]);
    let model = Model::new(
        profile(
            "openai-responses",
            &server.url,
            "max_output_tokens=300\nheaders={x-team='custom'}",
        ),
        "private-model".into(),
        Some(bee::auth::Secret::new("key")),
    )
    .unwrap();
    let mut text = String::new();
    let step = model
        .step(&[Message::User("question".into())], &[], &mut |chunk| {
            text.push_str(chunk)
        })
        .await
        .unwrap();
    assert_eq!(text, "héllo");
    assert_eq!(step.stop, Stop::End);
    assert_eq!(step.usage.input, Some(5));
    let bee::llm::CacheUsage::OpenAi {
        cached,
        written,
        ttl_30m,
        ttl_12h,
    } = &step.usage.cache[0]
    else {
        panic!("native cache usage expected")
    };
    assert_eq!(cached.tokens, Some(3));
    assert_eq!(written.tokens, Some(1));
    assert_eq!(ttl_30m.tokens, Some(1));
    assert_eq!(ttl_12h.tokens, Some(0));
    let request = server.request();
    assert!(request.head.starts_with("POST /prefix/responses "));
    assert!(request.head.contains("authorization: Bearer key"));
    assert!(request.head.contains("x-team: custom"));
    assert_eq!(request.body["input"][0]["content"], "question");
    assert_eq!(request.body["max_output_tokens"], 300);
    assert_eq!(request.body["store"], false);
}

#[tokio::test]
async fn tool_results_use_call_ids_and_replay_encrypted_reasoning_items() {
    let reasoning =
        json!({"id":"reason-1","type":"reasoning","summary":[],"encrypted_content":"opaque-state"});
    let call = json!({"id":"item-1","type":"function_call","call_id":"call-1","status":"completed","name":"echo","arguments":"{\"text\":\"tool output\"}"});
    let server = Server::start(vec![
        Response::sse(stream(vec![reasoning.clone(), call], false)),
        Response::json(completed(vec![message("done")])),
    ]);
    let model = Model::new(
        profile("openai-responses", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let mut text = String::new();
    let outcome = bee::agent::run(
        &model,
        vec![Message::User("echo".into())],
        &[tool()],
        Some(&Echo),
        &mut |chunk| text.push_str(chunk),
    )
    .await
    .unwrap();
    assert_eq!(text, "done");
    assert_eq!(outcome.stop, Stop::End);
    assert_eq!(outcome.usage.input, Some(10));
    assert_eq!(server.request().body["tools"][0]["strict"], false);
    let request = server.request();
    assert_eq!(request.body["input"][1], reasoning);
    assert_eq!(request.body["input"][2]["call_id"], "call-1");
    assert_eq!(
        request.body["input"][3],
        json!({"type":"function_call_output","call_id":"call-1","output":"tool output"})
    );
}

#[tokio::test]
async fn incomplete_and_refused_responses_have_distinct_stop_reasons() {
    let mut limited = completed(vec![message("partial")]);
    limited["status"] = "incomplete".into();
    limited["incomplete_details"] = json!({"reason":"max_output_tokens"});
    let refusal = json!({"id":"msg-r","type":"message","role":"assistant","content":[{"type":"refusal","refusal":"declined"}]});
    for (response, expected) in [
        (limited, Stop::MaxTokens),
        (completed(vec![refusal]), Stop::Refusal),
    ] {
        let server = Server::start(vec![Response::json(response)]);
        let model = Model::new(
            profile("openai-responses", &server.url, ""),
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
async fn failed_unsupported_and_truncated_responses_are_not_successful_answers() {
    for (response, expected) in [
        (
            Response::sse(event(
                "response.failed",
                json!({"type":"response.failed","response":{"error":{"message":"sentinel"}}}),
            )),
            Error::Provider,
        ),
        (
            Response::sse(event(
                "response.created",
                json!({"type":"response.created"}),
            )),
            Error::Malformed,
        ),
        (
            Response::json(completed(vec![json!({"type":"web_search_call"})])),
            Error::Unsupported("Responses output item"),
        ),
        (
            Response::json(json!({"status":"failed","error":{"message":"sentinel"}})),
            Error::Provider,
        ),
    ] {
        let server = Server::start(vec![response]);
        let model = Model::new(
            profile("openai-responses", &server.url, ""),
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
async fn a_terminal_response_cannot_replace_the_streamed_tool_arguments() {
    let call = json!({"id":"item-1","type":"function_call","call_id":"call-1","name":"echo","arguments":"{}"});
    let wire = stream(vec![call], false).replace("\"delta\":\"{}\"", "\"delta\":\"[]\"");
    let server = Server::start(vec![Response::sse(wire)]);
    let model = Model::new(
        profile("openai-responses", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    assert_eq!(
        model.step(&[], &[tool()], &mut |_| {}).await.unwrap_err(),
        Error::Malformed
    );
}

#[tokio::test]
async fn cancelling_a_responses_request_drops_the_pending_exchange() {
    let mut response = Response::json(completed(vec![message("late")]));
    response.delay = std::time::Duration::from_millis(100);
    let server = Server::start(vec![response]);
    let model = Model::new(
        profile("openai-responses", &server.url, ""),
        "opaque".into(),
        None,
    )
    .unwrap();
    let (abort, registration) = futures::future::AbortHandle::new_pair();
    let mut text = String::new();
    let mut sink = |chunk: &str| text.push_str(chunk);
    let future = futures::future::Abortable::new(model.step(&[], &[], &mut sink), registration);
    let cancel = async {
        server.request_async().await;
        abort.abort();
    };
    let (result, _) = futures::join!(future, cancel);
    assert!(result.is_err());
    assert!(text.is_empty());
}

#[tokio::test]
async fn provider_errors_on_the_preferred_protocol_do_not_trigger_fallback() {
    let mut response = Response::json(json!({"error":"sentinel"}));
    response.status = 401;
    let server = Server::start(vec![response]);
    let profiles = support::providers(
        "openai-responses",
        &server.url,
        "prefer=['openai-responses','openai-chat']\n[accounts.test.protocols.openai-chat]\nbase_url='http://127.0.0.1:1'",
    );
    let model = Model::new(profiles.get("test").unwrap().clone(), "opaque".into(), None).unwrap();
    assert_eq!(
        model.step(&[], &[], &mut |_| {}).await.unwrap_err(),
        Error::Authentication
    );
    assert!(server.request().head.starts_with("POST /prefix/responses "));
}

#[tokio::test]
async fn cancellation_after_a_text_delta_drops_the_unfinished_response() {
    let wire = stream(vec![message("partial")], true);
    let pause_at = wire.find("event: response.output_text.done").unwrap();
    let mut response = Response::sse(wire);
    response.pause = Some((pause_at, std::time::Duration::from_millis(100)));
    let server = Server::start(vec![response]);
    let model = Model::new(
        profile("openai-responses", &server.url, ""),
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
