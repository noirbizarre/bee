//! Multimodal input reaches native inference without losing portable history.

mod support;

use base64::Engine;
use bee::llm::{Message, Model, UserPart};
use serde_json::{Value, json};
use support::{Echo, Response, Server, profile, tool};

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=";

fn answer(protocol: &str) -> Value {
    match protocol {
        "openai-chat" => support::chat_text("done"),
        "openai-responses" => {
            json!({"status":"completed","output":[{"id":"m1","type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]})
        }
        _ => {
            json!({"type":"message","role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"})
        }
    }
}

fn call(protocol: &str) -> Value {
    match protocol {
        "openai-chat" => {
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"c1","type":"function","function":{"name":"echo","arguments":"{\"text\":\"ok\"}"}}]},"finish_reason":"tool_calls"}]})
        }
        "openai-responses" => {
            json!({"status":"completed","output":[{"type":"function_call","id":"f1","call_id":"c1","name":"echo","arguments":"{\"text\":\"ok\"}"}]})
        }
        _ => {
            json!({"type":"message","role":"assistant","content":[{"type":"tool_use","id":"c1","name":"echo","input":{"text":"ok"}}],"stop_reason":"tool_use"})
        }
    }
}

fn content(body: &Value, protocol: &str) -> Value {
    body[if protocol == "openai-responses" {
        "input"
    } else {
        "messages"
    }][0]["content"]
        .clone()
}

#[rstest::rstest]
#[case("openai-chat")]
#[case("openai-responses")]
#[case("anthropic-messages")]
#[tokio::test]
async fn ordered_images_and_pdfs_survive_tools_serialization_and_later_prompts(
    #[case] protocol: &str,
) {
    let server = Server::start(vec![
        Response::json(call(protocol)),
        Response::json(answer(protocol)),
        Response::json(answer(protocol)),
    ]);
    let model = Model::new(
        profile(
            protocol,
            &server.url,
            "metadata={input_modalities=['text','image','pdf']}",
        ),
        "private-model".into(),
        None,
    )
    .unwrap();
    let pdf = base64::engine::general_purpose::STANDARD
        .encode(b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n%%EOF\n");
    let input = Message::UserParts(vec![
        UserPart::Text("before".into()),
        UserPart::media("image/png".into(), PNG.into()).unwrap(),
        UserPart::Text("between".into()),
        UserPart::media("application/pdf".into(), pdf.clone()).unwrap(),
        UserPart::Text("after".into()),
    ]);
    let result = bee::agent::run(&model, vec![input], &[tool()], Some(&Echo), &mut |_| {})
        .await
        .unwrap();
    let first = server.request();
    let parts = content(&first.body, protocol);
    let (text, image, document) = match protocol {
        "openai-chat" => (
            "text",
            json!({"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG}")}}),
            json!({"type":"file","file":{"filename":"context.pdf","file_data":format!("data:application/pdf;base64,{pdf}")}}),
        ),
        "openai-responses" => (
            "input_text",
            json!({"type":"input_image","image_url":format!("data:image/png;base64,{PNG}")}),
            json!({"type":"input_file","filename":"context.pdf","file_data":format!("data:application/pdf;base64,{pdf}")}),
        ),
        _ => (
            "text",
            json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}}),
            json!({"type":"document","source":{"type":"base64","media_type":"application/pdf","data":pdf}}),
        ),
    };
    assert_eq!(
        parts,
        json!([{"type":text,"text":"before"},image,{"type":text,"text":"between"},document,{"type":text,"text":"after"}])
    );
    assert_eq!(content(&server.request().body, protocol), parts);
    let mut restored: Vec<Message> =
        serde_json::from_slice(&serde_json::to_vec(&result.messages).unwrap()).unwrap();
    assert!(!format!("{restored:?}").contains(PNG));
    restored.push(Message::User("follow up".into()));
    model.step(&restored, &[tool()], &mut |_| {}).await.unwrap();
    assert_eq!(content(&server.request().body, protocol), parts);
}

#[tokio::test]
async fn audio_uses_chat_input_audio_without_enabling_audio_output() {
    let server = Server::start(vec![Response::json(answer("openai-chat"))]);
    let model = Model::new(
        profile(
            "openai-chat",
            &server.url,
            "metadata={input_modalities=['audio']}",
        ),
        "private-model".into(),
        None,
    )
    .unwrap();
    let audio = UserPart::media("audio/wav".into(), "UklGRg==".into()).unwrap();
    model
        .step(&[Message::UserParts(vec![audio])], &[], &mut |_| {})
        .await
        .unwrap();
    let body = server.request().body;
    assert_eq!(
        body["messages"][0]["content"],
        json!([{"type":"input_audio","input_audio":{"data":"UklGRg==","format":"wav"}}])
    );
    assert!(body.get("audio").is_none());
    assert!(body.get("modalities").is_none());
}

#[test]
fn unsupported_routes_and_undeclared_modalities_fail_before_http_even_in_retained_history() {
    for (protocol, metadata, mime) in [
        ("openai-chat", "", "image/png"),
        (
            "openai-chat",
            "metadata={input_modalities=['text']}",
            "image/png",
        ),
        (
            "openai-chat",
            "metadata={input_modalities=['image']}",
            "application/pdf",
        ),
        (
            "openai-responses",
            "metadata={input_modalities=['audio']}",
            "audio/wav",
        ),
        (
            "anthropic-messages",
            "metadata={input_modalities=['audio']}",
            "audio/wav",
        ),
    ] {
        let model = Model::new(
            profile(protocol, "http://127.0.0.1:1", metadata),
            "private-model".into(),
            None,
        )
        .unwrap();
        let messages = [
            Message::UserParts(vec![
                UserPart::media(mime.into(), "cHJpdmF0ZS1jb250ZW50".into()).unwrap(),
            ]),
            Message::User("new text turn".into()),
        ];
        let error = model.context(&messages, &[]).unwrap_err();
        assert!(matches!(error, bee::llm::Error::Content(_)));
        assert!(!format!("{error} {error:?}").contains("cHJpdmF0"));
    }
}

#[test]
fn explicit_model_modalities_replace_account_metadata_including_empty_lists() {
    let accounts = support::providers(
        "openai-chat",
        "http://127.0.0.1:1",
        "metadata={input_modalities=['image','audio']}\nmodel_metadata.private-model={input_modalities=[]}",
    );
    let snapshot = bee::models::Snapshot::default();
    let selection = snapshot.resolve(accounts.get("test").unwrap(), "private-model");
    assert!(selection.input_modalities.is_empty());
    assert_eq!(
        selection.input_modalities_fact.unwrap().source,
        "configuration"
    );
    assert_eq!(
        snapshot
            .info(accounts.get("test").unwrap(), "private-model")
            .facts["input_modalities"]
            .value,
        json!([])
    );
}

struct CopilotTarget(String);

impl bee::provider::runtime::Prepared for CopilotTarget {
    fn credential<'a>(
        &'a self,
        _: &'a bee::account::Account,
        previous: Option<&'a bee::auth::Secret>,
    ) -> futures::future::BoxFuture<'a, Result<Option<bee::auth::Secret>, bee::llm::Error>> {
        Box::pin(async move { Ok(previous.cloned()) })
    }

    fn base_url(&self) -> Option<&str> {
        Some(&self.0)
    }
}

#[tokio::test]
async fn copilot_vision_headers_cover_retained_images_and_tool_continuations_only() {
    let server = Server::start(vec![
        Response::json(call("openai-chat")),
        Response::json(answer("openai-chat")),
        Response::json(answer("openai-chat")),
    ]);
    let accounts = support::configured_providers(
        "[accounts.work]\nprovider='github'\nmodels=['vision']\ndiscovery=false\nprefer=['openai-chat']\nmetadata={input_modalities=['image']}",
    );
    let model = Model::new(
        accounts.get("work").unwrap().clone(),
        "vision".into(),
        Some(bee::auth::Secret::new("fixture-token")),
    )
    .unwrap()
    .with_runtime(std::sync::Arc::new(CopilotTarget(server.url.clone())));
    bee::agent::run(
        &model,
        vec![Message::UserParts(vec![
            UserPart::media("image/png".into(), PNG.into()).unwrap(),
        ])],
        &[tool()],
        Some(&Echo),
        &mut |_| {},
    )
    .await
    .unwrap();
    for _ in 0..2 {
        assert!(
            server
                .request()
                .head
                .to_lowercase()
                .contains("copilot-vision-request: true")
        );
    }
    model
        .step(&[Message::User("text only".into())], &[], &mut |_| {})
        .await
        .unwrap();
    assert!(
        !server
            .request()
            .head
            .to_lowercase()
            .contains("copilot-vision-request")
    );
}

#[rstest::rstest]
#[case("openai-chat")]
#[case("openai-responses")]
#[case("anthropic-messages")]
#[tokio::test]
async fn multipart_history_is_evicted_only_as_a_whole_turn(#[case] protocol: &str) {
    let server = Server::start(vec![Response::json(answer(protocol))]);
    let current = Message::UserParts(vec![
        UserPart::Text("current".into()),
        UserPart::media("image/png".into(), PNG.into()).unwrap(),
    ]);
    let settings = "max_output_tokens=10\nmetadata={input_modalities=['image']}";
    let unbounded = Model::new(
        profile(protocol, &server.url, settings),
        "private-model".into(),
        None,
    )
    .unwrap();
    let report = unbounded
        .context(std::slice::from_ref(&current), &[])
        .unwrap();
    let model = Model::new(
        profile(
            protocol,
            &server.url,
            &format!(
                "{settings}\ncontext_window={}",
                report.estimated_input + report.reserved_output
            ),
        ),
        "private-model".into(),
        None,
    )
    .unwrap();
    let messages = vec![
        Message::UserParts(vec![
            UserPart::Text("historical".repeat(100)),
            UserPart::media("image/png".into(), PNG.into()).unwrap(),
        ]),
        Message::Assistant {
            text: "old answer".into(),
            calls: vec![],
            native: None,
        },
        current,
    ];
    let result = bee::agent::run(&model, messages, &[], None, &mut |_| {})
        .await
        .unwrap();
    assert_eq!(result.context.removed_turns, 1);
    let body = server.request().body;
    let field = if protocol == "openai-responses" {
        "input"
    } else {
        "messages"
    };
    assert_eq!(body[field].as_array().unwrap().len(), 1);
    assert_eq!(content(&body, protocol)[0]["text"], "current");
    assert!(!body.to_string().contains("historical"));
    assert!(!body.to_string().contains("old answer"));
    assert_eq!(content(&body, protocol).as_array().unwrap().len(), 2);
}

#[test]
fn mistral_pdfs_require_a_native_document_mapping_even_when_declared() {
    for settings in ["provider='mistral'", "provider='custom'"] {
        let model = Model::new(
            profile(
                "openai-chat",
                "https://api.mistral.ai/v1",
                &format!("{settings}\nmetadata={{input_modalities=['pdf']}}"),
            ),
            "private-model".into(),
            None,
        )
        .unwrap();
        let error = model
            .context(
                &[Message::UserParts(vec![
                    UserPart::media("application/pdf".into(), "aGk=".into()).unwrap(),
                ])],
                &[],
            )
            .unwrap_err();
        assert!(matches!(error, bee::llm::Error::Content(_)));
        assert!(error.to_string().contains("provider route"));
    }
}
