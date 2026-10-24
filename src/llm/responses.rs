//! Native Responses items and event lifecycle, independent of Chat messages.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{Error, Message, Model, Native, Step, Stop, Tool, Usage, call, string};
use crate::provider::Protocol;

pub(super) async fn run(
    model: &Model,
    messages: &[Message],
    tools: &[Tool],
    text: &mut (dyn FnMut(&str) + Send),
) -> Result<Step, Error> {
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
                        input.push(json!({"type":"function_call","call_id":call.id,"name":call.name,"arguments":call.arguments.to_string()}));
                    }
                }
            }
            Message::Tool { id, output, .. } => {
                input.push(json!({"type":"function_call_output","call_id":id,"output":output}))
            }
        }
    }
    let mut body = json!({"model":model.model,"input":input,"stream":true,"store":false,"max_output_tokens":model.profile.max_output_tokens(),"include":["reasoning.encrypted_content"]});
    if !tools.is_empty() {
        // Responses defaults strictness differently from Chat; explicitly opt
        // out so the same caller-owned JSON schema retains its meaning.
        body["tools"] = tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters,"strict":false})).collect();
    }
    let mut parser = Parser::default();
    model
        .http
        .post(
            &format!("{}/responses", model.endpoint().base_url()),
            model.headers(None)?,
            body,
            model.profile.timeout(),
            &mut |event, data| parser.event(event, data, text),
        )
        .await?;
    parser.result.ok_or(Error::Malformed)
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
            "response.failed" | "error" => return Err(Error::Provider),
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
        let mut stop = match response["status"].as_str() {
            Some("completed") => Stop::End,
            Some("incomplete") => match response["incomplete_details"]["reason"].as_str() {
                Some("max_output_tokens") => Stop::MaxTokens,
                Some("content_filter") => Stop::Refusal,
                _ => return Err(Error::Unsupported("Responses incomplete reason")),
            },
            Some("failed" | "cancelled") => return Err(Error::Provider),
            _ => return Err(Error::Malformed),
        };
        if response.get("error").is_some_and(|error| !error.is_null()) {
            return Err(Error::Provider);
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
                Some("function_call") => calls.push(call(
                    &item["call_id"],
                    &item["name"],
                    string(&item["arguments"])?,
                )?),
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
            usage: Usage {
                input: response["usage"]["input_tokens"].as_u64(),
                output: response["usage"]["output_tokens"].as_u64(),
            },
        });
        Ok(())
    }
}
