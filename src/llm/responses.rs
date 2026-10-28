//! Native Responses items and event lifecycle, independent of Chat messages.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{Error, Message, Model, Native, Step, Stop, Tool, Usage, call, string};
use crate::provider::{Protocol, ProtocolOptions};

pub(super) async fn run(
    model: &Model,
    messages: &[Message],
    tools: &[Tool],
    text: &mut (dyn FnMut(&str) + Send),
) -> Result<Step, Error> {
    let body = request(model, messages, tools)?;
    let mut parser = Parser {
        chatgpt: model.is_chatgpt(),
        ..Parser::default()
    };
    model
        .http
        .post(
            &format!("{}/responses", model.base_url()),
            model.headers(None)?,
            body,
            model.profile.timeout(),
            model.endpoint().protocol(),
            &mut |event, data| parser.event(event, data, text),
        )
        .await?;
    parser.result.ok_or(Error::Malformed)
}

pub(super) fn request(model: &Model, messages: &[Message], tools: &[Tool]) -> Result<Value, Error> {
    let mut input = Vec::new();
    for message in messages {
        match message {
            Message::User(text) => input.push(json!({"role":"user","content":text})),
            Message::Assistant {
                text,
                calls,
                native,
            } => {
                if let Some(native) = native {
                    if native.protocol != Protocol::OpenAiResponses {
                        return Err(Error::Unsupported("continuation from another protocol"));
                    }
                    // Stateless continuation must include reasoning as well as
                    // calls: replaying only visible text breaks reasoning tools.
                    input.extend(native.items.clone());
                } else {
                    if !text.is_empty() {
                        input.push(json!({"role":"assistant","content":text}));
                    }
                    for call in calls {
                        let mut item = json!({"type":"function_call","call_id":call.id,"name":call.name,"arguments":call.arguments.to_string()});
                        // Portable history has no native namespace metadata after a route change.
                        if model.is_chatgpt() {
                            item["namespace"] = "bee".into();
                        }
                        input.push(item);
                    }
                }
            }
            Message::Tool { id, output, .. } => {
                input.push(json!({"type":"function_call_output","call_id":id,"output":output}))
            }
        }
    }
    let mut body = json!({"model":model.model,"input":input,"stream":true,"store":false,"max_output_tokens":model.profile.max_output_tokens(),"include":["reasoning.encrypted_content"]});
    if model.is_chatgpt() {
        body.as_object_mut()
            .expect("request object")
            .remove("max_output_tokens");
    }
    if !model.instructions.is_empty() {
        body["instructions"] = model.instructions.clone().into();
    }
    if !tools.is_empty() {
        // Responses defaults strictness differently from Chat; explicitly opt
        // out so the same caller-owned JSON schema retains its meaning.
        body["tools"] = tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters,"strict":false})).collect();
        if model.is_chatgpt() {
            body["tools"] = json!([{"type":"namespace","name":"bee","description":"Bee client-side tools","tools":body["tools"].clone()}]);
        }
    }
    let ProtocolOptions::OpenAiResponses { cache } = model.endpoint().options() else {
        return Err(Error::Malformed);
    };
    let boundaries = super::cache::openai(model, &mut body, cache)?;
    if boundaries.instructions() {
        let block = super::cache::marked_text("input_text", &model.instructions)?;
        body.as_object_mut()
            .expect("request object")
            .remove("instructions");
        body["input"]
            .as_array_mut()
            .expect("input array")
            .insert(0, json!({"role":"developer","content":[block]}));
    }
    if boundaries.conversation() {
        let mut blocks = Vec::new();
        let mut items = body["input"]
            .as_array_mut()
            .expect("input array")
            .iter_mut()
            .peekable();
        while let Some(item) = items.next() {
            let field = if item["type"] == "function_call_output" {
                "output"
            } else if item["role"] == "user" {
                "content"
            } else {
                continue;
            };
            let text = item[field].as_str().ok_or(Error::Malformed)?;
            if text.is_empty() {
                continue;
            }
            // Keep the same block representation on replay as the bounded marker window grows.
            item[field] = json!([{"type":"input_text","text":text}]);
            // Consecutive results form one completed step boundary, preserving slots
            // for previous writes even when the model made many parallel calls.
            if field == "content"
                || items
                    .peek()
                    .is_none_or(|next| next["type"] != "function_call_output")
            {
                blocks.push(&mut item[field][0]);
            }
        }
        super::cache::mark_conversation(blocks, &cache.options)?;
    }
    Ok(body)
}

#[derive(Default)]
struct Item {
    id: String,
    kind: String,
    arguments: String,
    parts: BTreeMap<u64, String>,
    done: Option<Value>,
}

#[derive(Default)]
struct Parser {
    chatgpt: bool,
    items: BTreeMap<u64, Item>,
    streamed: String,
    result: Option<Step>,
}

impl Parser {
    fn event(
        &mut self,
        event: Option<&str>,
        data: &str,
        text: &mut (dyn FnMut(&str) + Send),
    ) -> Result<(), Error> {
        if self.result.is_some() {
            return Err(Error::Malformed);
        }
        let value: Value = serde_json::from_str(data).map_err(|_| Error::Malformed)?;
        if event.is_none() {
            if self.chatgpt {
                return Err(Error::Malformed);
            }
            return self.complete(&value, false, text);
        }
        let kind = string(&value["type"])?;
        if let Some(event) = event
            && !event.is_empty()
            && event != kind
        {
            return Err(Error::Malformed);
        }
        match kind {
            "response.created" | "response.in_progress" | "response.queued" => {}
            "response.output_item.added" => {
                let index = value["output_index"].as_u64().ok_or(Error::Malformed)?;
                if index >= 128 || self.items.contains_key(&index) {
                    return Err(Error::Malformed);
                }
                let item = &value["item"];
                let kind = string(&item["type"])?;
                if !matches!(kind, "message" | "function_call" | "reasoning") {
                    return Err(Error::Unsupported("Responses output item"));
                }
                self.items.insert(
                    index,
                    Item {
                        id: string(&item["id"])?.into(),
                        kind: kind.into(),
                        ..Item::default()
                    },
                );
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let item = self.item(&value)?;
                if item.kind != "message" {
                    return Err(Error::Malformed);
                }
                let index = value["content_index"].as_u64().ok_or(Error::Malformed)?;
                if index >= 128 {
                    return Err(Error::Limit);
                }
                let delta = string(&value["delta"])?;
                item.parts.entry(index).or_default().push_str(delta);
                self.streamed.push_str(delta);
                text(delta);
            }
            "response.function_call_arguments.delta" => {
                let item = self.item(&value)?;
                if item.kind != "function_call" {
                    return Err(Error::Malformed);
                }
                item.arguments.push_str(string(&value["delta"])?);
                if item.arguments.len() > crate::http::MAX_EVENT {
                    return Err(Error::Limit);
                }
            }
            "response.function_call_arguments.done" => {
                let item = self.item(&value)?;
                if item.kind != "function_call" || item.arguments != string(&value["arguments"])? {
                    return Err(Error::Malformed);
                }
            }
            "response.output_text.done" | "response.refusal.done" => {
                let item = self.item(&value)?;
                let index = value["content_index"].as_u64().ok_or(Error::Malformed)?;
                let full = if kind == "response.refusal.done" {
                    &value["refusal"]
                } else {
                    &value["text"]
                };
                if item.parts.get(&index).map(String::as_str).unwrap_or("") != string(full)? {
                    return Err(Error::Malformed);
                }
            }
            "response.content_part.added" | "response.content_part.done" => {
                self.item(&value)?;
                if !matches!(
                    value["part"]["type"].as_str(),
                    Some("output_text" | "refusal")
                ) {
                    return Err(Error::Unsupported("Responses content part"));
                }
            }
            "response.output_item.done" => {
                let item = self.item(&value)?;
                let full = &value["item"];
                if full["id"] != item.id || full["type"] != item.kind {
                    return Err(Error::Malformed);
                }
                if item.kind == "function_call" && full["arguments"] != item.arguments {
                    return Err(Error::Malformed);
                }
                item.done = Some(full.clone());
            }
            "response.completed" | "response.incomplete" => {
                let status = if kind == "response.completed" {
                    "completed"
                } else {
                    "incomplete"
                };
                if value["response"]["status"] != status {
                    return Err(Error::Malformed);
                }
                self.complete(&value["response"], true, text)?;
            }
            "response.failed" => {
                return Err(super::provider_error(
                    Protocol::OpenAiResponses,
                    &value["response"]["error"],
                ));
            }
            "error" => {
                return Err(super::provider_error(
                    Protocol::OpenAiResponses,
                    value.get("error").unwrap_or(&value),
                ));
            }
            // Reasoning is never presented as visible assistant text. Its final
            // replayable item is retained verbatim by response.completed.
            "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done" => {
                self.item(&value)?;
            }
            _ => return Err(Error::Unsupported("Responses streaming event")),
        }
        Ok(())
    }

    fn item(&mut self, value: &Value) -> Result<&mut Item, Error> {
        let index = value["output_index"].as_u64().ok_or(Error::Malformed)?;
        let item = self.items.get_mut(&index).ok_or(Error::Malformed)?;
        if item.done.is_some() || value.get("item_id").is_some_and(|id| id != &item.id) {
            return Err(Error::Malformed);
        }
        Ok(item)
    }

    fn complete(
        &mut self,
        response: &Value,
        streaming: bool,
        text: &mut (dyn FnMut(&str) + Send),
    ) -> Result<(), Error> {
        if self.chatgpt && response["status"] != "completed" {
            return Err(Error::Malformed);
        }
        let mut stop = match response["status"].as_str() {
            Some("completed") => Stop::End,
            Some("incomplete") => match response["incomplete_details"]["reason"].as_str() {
                Some("max_output_tokens") => Stop::MaxTokens,
                Some("content_filter") => Stop::Refusal,
                _ => return Err(Error::Unsupported("Responses incomplete reason")),
            },
            Some("failed" | "cancelled") => {
                return Err(super::provider_error(
                    Protocol::OpenAiResponses,
                    &response["error"],
                ));
            }
            _ => return Err(Error::Malformed),
        };
        if response.get("error").is_some_and(|error| !error.is_null()) {
            return Err(super::provider_error(
                Protocol::OpenAiResponses,
                &response["error"],
            ));
        }
        let output = response["output"].as_array().ok_or(Error::Malformed)?;
        if streaming
            && (self.items.len() != output.len()
                || self.items.values().any(|item| item.done.is_none()))
        {
            return Err(Error::Malformed);
        }
        let mut answer = String::new();
        let mut calls = Vec::new();
        for (index, item) in output.iter().enumerate() {
            if streaming
                && self
                    .items
                    .get(&(index as u64))
                    .and_then(|item| item.done.as_ref())
                    != Some(item)
            {
                return Err(Error::Malformed);
            }
            match item["type"].as_str() {
                Some("message") => {
                    if item["role"] != "assistant" {
                        return Err(Error::Malformed);
                    }
                    for part in item["content"].as_array().ok_or(Error::Malformed)? {
                        match part["type"].as_str() {
                            Some("output_text") => answer.push_str(string(&part["text"])?),
                            Some("refusal") => {
                                answer.push_str(string(&part["refusal"])?);
                                stop = Stop::Refusal;
                            }
                            _ => return Err(Error::Unsupported("Responses message content")),
                        }
                    }
                }
                Some("function_call") => {
                    if self.chatgpt && item["namespace"] != "bee" {
                        return Err(Error::Unsupported("Responses tool namespace"));
                    }
                    calls.push(call(
                        &item["call_id"],
                        &item["name"],
                        string(&item["arguments"])?,
                    )?);
                }
                Some("reasoning") => {
                    // A reasoning item with no replayable encrypted state cannot
                    // safely continue in stateless mode after tools or prompts.
                    if item.get("encrypted_content").is_none_or(Value::is_null) {
                        return Err(Error::Unsupported(
                            "unencrypted Responses reasoning continuation",
                        ));
                    }
                }
                _ => return Err(Error::Unsupported("Responses output item")),
            }
        }
        if !calls.is_empty() {
            if stop != Stop::End {
                return Err(Error::Malformed);
            }
            stop = Stop::Tools;
        }
        let suffix = answer
            .strip_prefix(&self.streamed)
            .ok_or(Error::Malformed)?;
        if !suffix.is_empty() {
            text(suffix);
        }
        self.result = Some(Step {
            message: Message::Assistant {
                text: answer,
                calls,
                native: Some(Native {
                    protocol: Protocol::OpenAiResponses,
                    items: output.clone(),
                }),
            },
            stop,
            usage: Usage::openai(&response["usage"], true),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        model_with_options("")
    }

    fn model_with_options(options: &str) -> Model {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let path = crate::config::project_config_path(&project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("[providers.subscription]\ntype='chatgpt'\nmodel='account-model'\ncontext_window=10000\nmax_output_tokens=2000\n[providers.subscription.protocols.openai-responses.options]\n{options}")).unwrap();
        let config =
            crate::config::Config::load(&crate::config::Environment::default(), &project).unwrap();
        let providers = crate::provider::Providers::from_config(&config).unwrap();
        Model::new(
            providers.get("subscription").unwrap().clone(),
            "account-model".into(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn chatgpt_reserves_output_locally_without_sending_forbidden_fields() {
        let model = model();
        let messages = [Message::User("hello".into())];
        let tools = [Tool {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: json!({"type":"object"}),
        }];
        let body = request(&model, &messages, &tools).unwrap();
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(body["tools"][0]["name"], "bee");
        assert_eq!(body["tools"][0]["tools"][0]["name"], "read");
        assert_eq!(
            model.context(&messages, &tools).unwrap().reserved_output,
            2000
        );
    }

    #[test]
    fn chatgpt_allows_keys_but_rejects_retention_and_unverified_write_policies() {
        let messages = [Message::User("hello".into())];
        let body = request(&model(), &messages, &[]).unwrap();
        assert!(
            body["prompt_cache_key"]
                .as_str()
                .unwrap()
                .starts_with("bee-v1-")
        );
        let explicit =
            model_with_options("prompt_cache_key={mode='explicit',value='application-key'}");
        assert_eq!(
            request(&explicit, &messages, &[]).unwrap()["prompt_cache_key"],
            "application-key"
        );
        for options in [
            "prompt_cache_retention='24h'",
            "prompt_cache_options={enabled=true}",
        ] {
            assert!(matches!(
                request(&model_with_options(options), &messages, &[]),
                Err(Error::Cache(_))
            ));
        }
    }

    #[test]
    fn namespaced_calls_keep_executor_names_and_native_continuation() {
        let item = json!({"type":"function_call","namespace":"bee","name":"read","call_id":"call-1","arguments":"{}"});
        let response = json!({"status":"completed","output":[item.clone()]});
        let mut parser = Parser {
            chatgpt: true,
            ..Parser::default()
        };
        parser.complete(&response, false, &mut |_| {}).unwrap();
        let message = parser.result.unwrap().message;
        let Message::Assistant { calls, .. } = &message else {
            panic!("assistant expected")
        };
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].id, "call-1");
        let portable = Message::Assistant {
            text: String::new(),
            calls: calls.clone(),
            native: None,
        };
        let replay = request(
            &model(),
            &[
                Message::User("question".into()),
                message,
                Message::Tool {
                    id: "call-1".into(),
                    is_error: false,
                    output: "content".into(),
                },
            ],
            &[],
        )
        .unwrap();
        assert_eq!(replay["input"][1], item);
        assert_eq!(
            request(&model(), &[portable], &[]).unwrap()["input"][0]["namespace"],
            "bee"
        );
        let mut invalid = response;
        invalid["output"][0]["namespace"] = "another-client".into();
        assert!(
            Parser {
                chatgpt: true,
                ..Parser::default()
            }
            .complete(&invalid, false, &mut |_| {})
            .is_err()
        );
    }

    #[test]
    fn chatgpt_requires_completed_streams_and_retains_safe_usage_failure_guidance() {
        let mut parser = Parser {
            chatgpt: true,
            ..Parser::default()
        };
        assert!(
            parser
                .event(
                    None,
                    &json!({"status":"completed","output":[]}).to_string(),
                    &mut |_| {}
                )
                .is_err()
        );
        let incomplete = json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]}});
        assert!(
            parser
                .event(
                    Some("response.incomplete"),
                    &incomplete.to_string(),
                    &mut |_| {}
                )
                .is_err()
        );
        let failed = json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"secret-do-not-show"}}});
        let error = parser
            .event(Some("response.failed"), &failed.to_string(), &mut |_| {})
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("https://chatgpt.com/settings/usage")
        );
        assert!(!format!("{error:?} {error}").contains("secret-do-not-show"));
    }
}
