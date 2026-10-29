use super::*;
use crate::llm::{Message, Native, Tool, anthropic, chat, responses};
use crate::provider::ProtocolOptions;

fn model(protocol: Protocol, url: &str, name: &str, options: &str) -> Model {
    let root = tempfile::tempdir().unwrap();
    let path = crate::config::project_config_path(root.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!(
        "model='work/{name}'\n[accounts.work]\nprovider='custom'\nmodels=['{name}']\n[accounts.work.protocols.{}]\nbase_url = '{url}'\n[accounts.work.protocols.{}.options]\n{options}",
        protocol.name(), protocol.name(),
    )).unwrap();
    let config =
        crate::config::Config::load(&crate::config::Environment::default(), root.path()).unwrap();
    let providers = crate::account::Accounts::from_config(&config).unwrap();
    Model::new(providers.get("work").unwrap().clone(), name.into(), None)
        .unwrap()
        .with_instructions("Stable instructions".into())
}

fn request(model: &Model, messages: &[Message], tools: &[Tool]) -> Result<Value, Error> {
    match model.endpoint().protocol() {
        Protocol::OpenAiChat => chat::request(model, messages, tools),
        Protocol::OpenAiResponses => responses::request(model, messages, tools),
        Protocol::AnthropicMessages => anthropic::request(model, messages, tools),
    }
}

fn tools() -> Vec<Tool> {
    vec![Tool {
        name: "read".into(),
        description: "Read".into(),
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
    }]
}

#[test]
fn automatic_keys_survive_model_reconstruction_and_ignore_credentials_and_instructions() {
    for protocol in [Protocol::OpenAiChat, Protocol::OpenAiResponses] {
        let first = model(protocol, OPENAI, "gpt-4.1", "");
        let mut rebuilt = model(protocol, OPENAI, "gpt-4.1", "");
        rebuilt.credential = Some(crate::auth::Secret::new("different-credential"));
        rebuilt.instructions = "new scoped guidance".into();
        let messages = [Message::User("first".into())];
        let original = request(&first, &messages, &tools()).unwrap();
        let key = &original["prompt_cache_key"];
        assert!(key.as_str().unwrap().starts_with("bee-v1-"));
        assert!(key.as_str().unwrap().len() <= 64);
        assert_eq!(
            key,
            &request(&rebuilt, &messages, &tools()).unwrap()["prompt_cache_key"]
        );
        assert!(original.get("prompt_cache_options").is_none());
        assert!(original.get("prompt_cache_retention").is_none());
        for different in [
            first.clone().with_cache_scope("other-group".into()),
            model(protocol, OPENAI, "gpt-5", ""),
            model(protocol, "https://api.x.ai/v1", "grok-4.7", ""),
        ] {
            assert_ne!(
                key,
                &request(&different, &messages, &tools()).unwrap()["prompt_cache_key"]
            );
        }
    }
}

#[test]
fn keys_are_added_only_on_documented_endpoints_and_explicit_unsupported_controls_fail() {
    for (protocol, url, default_key) in [
        (Protocol::OpenAiChat, OPENAI, true),
        (Protocol::OpenAiResponses, OPENAI, true),
        (Protocol::OpenAiChat, "https://api.mistral.ai/v1", true),
        (Protocol::OpenAiResponses, "https://api.x.ai/v1", true),
        (Protocol::OpenAiChat, "https://api.x.ai/v1", false),
        (Protocol::OpenAiChat, OPENROUTER, false),
        (
            Protocol::OpenAiResponses,
            "https://api.openai.com.evil/v1",
            false,
        ),
        (Protocol::OpenAiChat, "https://api.deepseek.com", false),
    ] {
        let messages = [Message::User("hello".into())];
        let default = model(protocol, url, "opaque-model", "");
        assert_eq!(
            request(&default, &messages, &[])
                .unwrap()
                .get("prompt_cache_key")
                .is_some(),
            default_key
        );
        let explicit = model(
            protocol,
            url,
            "opaque-model",
            "prompt_cache_key = {mode='explicit', value='application-key'}",
        );
        let result = request(&explicit, &messages, &[]);
        if default_key || url == OPENROUTER {
            assert_eq!(result.unwrap()["prompt_cache_key"], "application-key");
        } else {
            assert_eq!(result.unwrap_err(), Error::Cache("prompt_cache_key"));
        }
        let disabled = model(
            protocol,
            url,
            "opaque-model",
            "prompt_cache_key = {mode='disabled'}",
        );
        assert!(
            request(&disabled, &messages, &[])
                .unwrap()
                .get("prompt_cache_key")
                .is_none()
        );
    }
}

#[test]
fn retention_is_model_specific_and_never_implied_by_wire_compatibility() {
    let messages = [Message::User("hello".into())];
    for protocol in [Protocol::OpenAiChat, Protocol::OpenAiResponses] {
        for (name, retention, allowed) in [
            ("gpt-4.1", "24h", true),
            ("gpt-5.5", "24h", true),
            ("gpt-5.5", "in_memory", false),
            ("gpt-4o-mini", "in_memory", true),
            ("gpt-4o-mini", "24h", false),
            ("gpt-5.6", "24h", false),
            ("gpt-private", "24h", false),
        ] {
            let model = model(
                protocol,
                OPENAI,
                name,
                &format!("prompt_cache_retention='{retention}'"),
            );
            let result = request(&model, &messages, &[]);
            assert_eq!(result.is_ok(), allowed);
            if allowed {
                assert_eq!(result.unwrap()["prompt_cache_retention"], retention);
            }
        }
        let custom = model(
            protocol,
            "https://proxy.example/v1",
            "gpt-4.1",
            "prompt_cache_retention='24h'",
        );
        assert!(matches!(
            request(&custom, &messages, &[]),
            Err(Error::Cache(_))
        ));
    }
}

#[test]
fn responses_breakpoints_preserve_native_items_and_send_instructions_exactly_once() {
    let model = model(
        Protocol::OpenAiResponses,
        OPENAI,
        "gpt-6.1-sol",
        "prompt_cache_options={enabled=true,mode='explicit',ttl='12h',breakpoints='instructions-and-conversation'}",
    );
    let native_item =
        json!({"type":"reasoning","id":"r1","encrypted_content":"opaque","summary":[]});
    let messages = vec![
        Message::User("question".into()),
        Message::Assistant {
            text: String::new(),
            calls: vec![],
            native: Some(Native {
                protocol: Protocol::OpenAiResponses,
                items: vec![native_item.clone()],
            }),
        },
        Message::Tool {
            id: "call-1".into(),
            output: "result".into(),
            is_error: false,
        },
    ];
    let original = serde_json::to_value(&messages).unwrap();
    let body = request(&model, &messages, &tools()).unwrap();
    assert!(body.get("instructions").is_none());
    assert_eq!(body["input"][0]["role"], "developer");
    assert_eq!(
        body["input"][0]["content"][0]["text"],
        "Stable instructions"
    );
    assert_eq!(
        body["input"][0]["content"][0]["prompt_cache_breakpoint"]["mode"],
        "explicit"
    );
    assert_eq!(body["input"][2], native_item);
    assert_eq!(body["input"][3]["call_id"], "call-1");
    assert_eq!(body["input"][3]["output"][0]["text"], "result");
    assert_eq!(
        body["input"][3]["output"][0]["prompt_cache_breakpoint"]["mode"],
        "explicit"
    );
    assert_eq!(
        body["prompt_cache_options"],
        json!({"mode":"explicit","ttl":"12h"})
    );
    assert_eq!(body["store"], false);
    assert_eq!(request(&model, &messages, &tools()).unwrap(), body);
    assert_eq!(serde_json::to_value(&messages).unwrap(), original);
    assert!(model.context(&messages, &tools()).unwrap().estimated_input > 0);
}

#[test]
fn chat_and_responses_enforce_distinct_lifetimes_and_model_capabilities() {
    let messages = [Message::User("question".into())];
    for (protocol, url, name, ttl, allowed) in [
        (Protocol::OpenAiChat, OPENAI, "gpt-5.6", "30m", true),
        (Protocol::OpenAiChat, OPENAI, "gpt-5.6", "12h", false),
        (Protocol::OpenAiResponses, OPENAI, "gpt-5.6", "12h", true),
        (Protocol::OpenAiResponses, OPENAI, "gpt-4.1", "30m", false),
        (
            Protocol::OpenAiResponses,
            OPENAI,
            "gpt-5.6-2026-05-01",
            "30m",
            true,
        ),
        (
            Protocol::OpenAiResponses,
            OPENAI,
            "gpt-5.6-unknown",
            "30m",
            false,
        ),
        (
            Protocol::OpenAiChat,
            OPENROUTER,
            "openai/gpt-5.6",
            "12h",
            true,
        ),
        (
            Protocol::OpenAiChat,
            OPENROUTER,
            "anthropic/claude-sonnet-4-6",
            "30m",
            false,
        ),
    ] {
        let model = model(
            protocol,
            url,
            name,
            &format!(
                "prompt_cache_options={{enabled=true,mode='implicit',ttl='{ttl}',breakpoints='instructions-and-conversation'}}"
            ),
        );
        let body = request(&model, &messages, &[]);
        assert_eq!(body.is_ok(), allowed);
        if allowed && protocol == Protocol::OpenAiChat {
            let body = body.unwrap();
            assert_eq!(body["messages"][0]["content"][0]["type"], "text");
            assert_eq!(body["messages"][1]["content"][0]["text"], "question");
        }
    }
    let disabled = model(
        Protocol::OpenAiResponses,
        OPENAI,
        "gpt-5.6",
        "prompt_cache_options={enabled=true,mode='explicit'}",
    );
    let body = request(&disabled, &messages, &[]).unwrap();
    assert_eq!(body["prompt_cache_options"], json!({"mode":"explicit"}));
    assert!(body["input"][0]["content"].is_string());
}

#[test]
fn anthropic_writes_require_opt_in_and_use_native_ephemeral_policies() {
    let messages = [Message::User("question".into())];
    for ttl in ["5m", "1h"] {
        for policy in ["automatic", "stable-prefix", "stable-prefix-and-automatic"] {
            let model = model(
                Protocol::AnthropicMessages,
                "https://api.anthropic.com",
                "claude-sonnet-4-6",
                &format!("cache_control={{policy='{policy}',ttl='{ttl}'}}"),
            );
            let body = request(&model, &messages, &tools()).unwrap();
            let marker = json!({"type":"ephemeral","ttl":ttl});
            if policy.contains("automatic") {
                assert_eq!(body["cache_control"], marker);
            } else {
                assert!(body.get("cache_control").is_none());
            }
            if policy.contains("stable-prefix") {
                assert_eq!(body["system"][0]["cache_control"], marker);
            } else {
                assert_eq!(body["system"], "Stable instructions");
            }
            let no_system = model.clone().with_instructions(String::new());
            let body = request(&no_system, &messages, &tools()).unwrap();
            if policy.contains("stable-prefix") {
                assert_eq!(body["tools"][0]["cache_control"], marker);
            }
        }
    }
    let default = model(
        Protocol::AnthropicMessages,
        "https://api.anthropic.com",
        "opaque-model",
        "",
    );
    let body = request(&default, &messages, &tools()).unwrap();
    assert!(body.get("cache_control").is_none());
    assert!(body["tools"][0].get("cache_control").is_none());
    for (url, name) in [
        ("https://proxy.example", "claude-sonnet-4-6"),
        ("https://api.anthropic.com", "private-claude"),
    ] {
        let model = model(
            Protocol::AnthropicMessages,
            url,
            name,
            "cache_control={policy='automatic'}",
        );
        assert!(matches!(
            request(&model, &messages, &[]),
            Err(Error::Cache(_))
        ));
    }
}

#[test]
fn openrouter_claude_versions_are_validated_without_rewriting_gateway_model_ids() {
    for (name, allowed) in [
        ("anthropic/claude-sonnet-4.6", true),
        ("anthropic/claude-opus-4.1", true),
        ("anthropic/claude-haiku-4.5", true),
        ("anthropic/private-claude", false),
        ("openai/gpt-5.6", false),
    ] {
        let model = model(
            Protocol::AnthropicMessages,
            "https://openrouter.ai/api",
            name,
            "cache_control={policy='stable-prefix-and-automatic',ttl='1h'}",
        );
        let body = request(&model, &[Message::User("hello".into())], &tools());
        assert_eq!(body.is_ok(), allowed);
        if allowed {
            let body = body.unwrap();
            assert_eq!(body["model"], name);
            assert_eq!(body["cache_control"]["ttl"], "1h");
            assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
        }
    }
    let direct = model(
        Protocol::AnthropicMessages,
        "https://api.anthropic.com",
        "claude-sonnet-4.6",
        "cache_control={policy='automatic'}",
    );
    assert!(matches!(
        request(&direct, &[Message::User("hello".into())], &[]),
        Err(Error::Cache(_))
    ));
}

fn marked_texts(body: &Value, protocol: Protocol) -> Vec<String> {
    let field = if protocol == Protocol::OpenAiResponses {
        "input"
    } else {
        "messages"
    };
    body[field]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| {
            ["content", "output"]
                .into_iter()
                .filter_map(|field| item[field].as_array())
                .flatten()
                .filter(|block| block.get("prompt_cache_breakpoint").is_some())
                .map(|block| block["text"].as_str().unwrap().to_owned())
        })
        .collect()
}

#[test]
fn explicit_conversation_writes_remain_lookup_boundaries_across_tool_steps_and_replay() {
    for protocol in [Protocol::OpenAiChat, Protocol::OpenAiResponses] {
        for mode in ["implicit", "explicit"] {
            for policy in ["conversation", "instructions-and-conversation"] {
                let model = model(
                    protocol,
                    OPENAI,
                    "gpt-5.6",
                    &format!(
                        "prompt_cache_options={{enabled=true,mode='{mode}',breakpoints='{policy}'}}"
                    ),
                );
                let mut messages = vec![Message::User("first question".into())];
                let first = request(&model, &messages, &tools()).unwrap();
                assert!(marked_texts(&first, protocol).contains(&"first question".into()));
                // A parallel tool batch is one request boundary, not six competing markers.
                messages.push(Message::Assistant {
                    text: String::new(),
                    calls: (0..6)
                        .map(|i| crate::llm::Call {
                            id: format!("call-{i}"),
                            name: "read".into(),
                            arguments: json!({}),
                        })
                        .collect(),
                    native: None,
                });
                for i in 0..6 {
                    messages.push(Message::Tool {
                        id: format!("call-{i}"),
                        output: format!("result-{i}"),
                        is_error: false,
                    });
                }
                let second = request(&model, &messages, &tools()).unwrap();
                let markers = marked_texts(&second, protocol);
                assert!(markers.contains(&"first question".into()));
                assert!(markers.contains(&"result-5".into()));
                assert!(!markers.contains(&"result-0".into()));
                messages.push(Message::Assistant {
                    text: "answer".into(),
                    calls: vec![],
                    native: None,
                });
                messages.push(Message::User("follow-up".into()));
                let replayed: Vec<Message> =
                    serde_json::from_value(serde_json::to_value(&messages).unwrap()).unwrap();
                let third = request(&model, &replayed, &tools()).unwrap();
                let markers = marked_texts(&third, protocol);
                assert!(markers.contains(&"result-5".into()));
                assert!(markers.contains(&"follow-up".into()));
                for i in 0..8 {
                    messages.push(Message::Assistant {
                        text: "answer".into(),
                        calls: vec![],
                        native: None,
                    });
                    messages.push(Message::User(format!("later-{i}")));
                }
                let later = request(&model, &messages, &tools()).unwrap();
                let markers = marked_texts(&later, protocol);
                assert_eq!(markers.len(), if mode == "implicit" { 3 } else { 4 });
                assert!(markers.contains(&"later-7".into()));
                assert!(markers.contains(&"later-6".into()));
                assert_eq!(
                    markers.contains(&"Stable instructions".into()),
                    policy == "instructions-and-conversation"
                );
            }
        }
    }
}

#[test]
fn missing_explicit_boundaries_fail_without_inserting_empty_or_duplicate_instructions() {
    let messages = [Message::User("hello".into())];
    for protocol in [Protocol::OpenAiChat, Protocol::OpenAiResponses] {
        let model = model(
            protocol,
            OPENAI,
            "gpt-5.6",
            "prompt_cache_options={enabled=true,breakpoints='instructions'}",
        )
        .with_instructions(String::new());
        assert!(matches!(
            request(&model, &messages, &[]),
            Err(Error::Cache(_))
        ));
    }
    let model = model(
        Protocol::AnthropicMessages,
        "https://api.anthropic.com",
        "claude-sonnet-4-6",
        "cache_control={policy='stable-prefix'}",
    )
    .with_instructions(String::new());
    assert!(matches!(
        request(&model, &messages, &[]),
        Err(Error::Cache(_))
    ));
}

#[test]
fn unchanged_multi_turn_prefixes_and_tools_remain_identical_in_every_adapter() {
    for protocol in Protocol::ALL {
        let url = protocol.default_base_url();
        let model = model(protocol, url, "ordinary-model", "");
        let mut messages = vec![Message::User("first question".into())];
        let first = request(&model, &messages, &tools()).unwrap();
        messages.push(Message::Assistant {
            text: "answer".into(),
            calls: vec![],
            native: None,
        });
        messages.push(Message::User("follow-up".into()));
        // Durable replay uses the same semantic messages, not freshly generated IDs.
        let replayed: Vec<Message> =
            serde_json::from_value(serde_json::to_value(&messages).unwrap()).unwrap();
        let later = request(&model, &replayed, &tools()).unwrap();
        let field = if protocol == Protocol::OpenAiResponses {
            "input"
        } else {
            "messages"
        };
        let prefix = first[field].as_array().unwrap();
        assert_eq!(&later[field].as_array().unwrap()[..prefix.len()], prefix);
        assert_eq!(first["tools"], later["tools"]);
        for key in ["instructions", "system", "prompt_cache_key"] {
            assert_eq!(first.get(key), later.get(key));
        }
    }
}

#[test]
fn explicit_key_debug_never_exposes_the_configured_value() {
    let model = model(
        Protocol::OpenAiResponses,
        OPENAI,
        "gpt-4.1",
        "prompt_cache_key={mode='explicit',value='private-application-key'}",
    );
    assert!(!format!("{:?}", model.profile).contains("private-application-key"));
    let ProtocolOptions::OpenAiResponses { cache, .. } = model.endpoint().options() else {
        unreachable!()
    };
    assert!(!format!("{cache:?}").contains("private-application-key"));
}

#[tokio::test]
async fn cached_usage_survives_real_adapters_and_multi_step_tool_loops() {
    use crate::test_support::{Response, Server};
    struct Execute;
    impl crate::agent::Executor for Execute {
        fn execute<'a>(
            &'a self,
            _: &'a crate::llm::Call,
        ) -> futures::future::BoxFuture<'a, Result<crate::agent::ToolResult, Error>> {
            Box::pin(async {
                Ok(crate::agent::ToolResult {
                    output: "file contents".into(),
                    is_error: false,
                })
            })
        }
    }
    let mut index = 0;
    let server = Server::respond(3, move |_| {
        index += 1;
        let output = if index == 1 {
            json!([{"type":"function_call","id":"item1","call_id":"read1","name":"read","arguments":"{}"}])
        } else {
            json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}])
        };
        Response::json(
            json!({"status":"completed","output":output,"usage":{"input_tokens":1000,"output_tokens":10,
            "input_tokens_details":{"cached_tokens":if index == 1 { 0 } else { 700 },"cache_write_tokens":200}}}),
        )
    });
    let model = model(
        Protocol::OpenAiResponses,
        OPENAI,
        "gpt-5.6",
        "prompt_cache_options={enabled=true,mode='implicit',ttl='30m'}",
    )
    .with_test_http(crate::http::InferenceHttp::test_routed(server.url.clone()));
    let outcome = crate::agent::run(
        &model,
        vec![Message::User("read the file".into())],
        &tools(),
        Some(&Execute),
        &mut |_| {},
    )
    .await
    .unwrap();
    let crate::llm::CacheUsage::OpenAi {
        cached, written, ..
    } = &outcome.usage.cache[0]
    else {
        panic!("OpenAI usage expected")
    };
    assert_eq!(cached.tokens, Some(700));
    assert_eq!(cached.reports, 2);
    assert_eq!(written.tokens, Some(400));
    assert_eq!(outcome.usage.input, Some(2000));
    let mut resumed = outcome.messages;
    resumed.push(Message::User("follow-up".into()));
    crate::agent::run(&model, resumed, &tools(), Some(&Execute), &mut |_| {})
        .await
        .unwrap();
    let first = server.request();
    let second = server.request();
    let third = server.request();
    assert_eq!(first.body["input"][0], second.body["input"][0]);
    assert_eq!(
        second.body["input"],
        json!(
            third.body["input"].as_array().unwrap()
                [..second.body["input"].as_array().unwrap().len()]
        )
    );
    assert_eq!(
        first.body["prompt_cache_key"],
        third.body["prompt_cache_key"]
    );
    assert_eq!(first.body["tools"], third.body["tools"]);
    assert_eq!(first.body["instructions"], third.body["instructions"]);
}

#[tokio::test]
async fn unsupported_explicit_controls_fail_before_any_http_request() {
    use crate::test_support::{Response, Server};
    let server = Server::start(vec![Response::json(json!({}))]);
    let model = model(
        Protocol::OpenAiResponses,
        OPENAI,
        "gpt-4.1",
        "prompt_cache_options={enabled=true}",
    )
    .with_test_http(crate::http::InferenceHttp::test_routed(server.url.clone()));
    assert_eq!(
        model
            .step(&[Message::User("hello".into())], &[], &mut |_| {})
            .await
            .unwrap_err(),
        Error::Cache("prompt_cache_options")
    );
    server.assert_no_more_requests();
}
