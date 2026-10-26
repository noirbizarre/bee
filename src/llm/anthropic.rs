//! Native Anthropic Messages, with ordered content blocks and tool results.

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
    let ProtocolOptions::AnthropicMessages { version, .. } = model.endpoint().options() else {
        return Err(Error::Malformed);
    };
    let mut parser = Parser::default();
    model
        .http
        .post(
            &format!("{}/v1/messages", model.base_url()),
            model.headers(Some(version.as_deref().unwrap_or("2023-06-01")))?,
            body,
            model.profile.timeout(),
            model.endpoint().protocol(),
            &mut |event, data| parser.event(event, data, text),
        )
        .await?;
    parser.result.ok_or(Error::Malformed)
}

pub(super) fn request(model: &Model, messages: &[Message], tools: &[Tool]) -> Result<Value, Error> {
    let mut input: Vec<Value> = Vec::new();
    for message in messages {
        let (role, blocks) = match message {
            Message::User(text) => ("user", vec![json!({"type":"text","text":text})]),
            Message::Assistant {
                text,
                calls,
                native,
            } => {
                let blocks = if let Some(native) = native {
                    if native.protocol != Protocol::AnthropicMessages {
                        return Err(Error::Unsupported("continuation from another protocol"));
                    }
                    native.items.clone()
                } else {
                    let mut blocks = Vec::new();
                    if !text.is_empty() {
                        blocks.push(json!({"type":"text","text":text}));
                    }
                    for call in calls {
                        blocks.push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments}));
                    }
                    blocks
                };
                ("assistant", blocks)
            }
            Message::Tool {
                id,
                output,
                is_error,
            } => (
                "user",
                vec![
                    json!({"type":"tool_result","tool_use_id":id,"content":output,"is_error":is_error}),
                ],
            ),
        };
        // Anthropic expects every result of a multi-call assistant message in
        // the immediately following user message, not one message per result.
        if let Some(last) = input.last_mut().filter(|last| last["role"] == role) {
            last["content"]
                .as_array_mut()
                .ok_or(Error::Malformed)?
                .extend(blocks);
        } else {
            input.push(json!({"role":role,"content":blocks}));
        }
    }
    let ProtocolOptions::AnthropicMessages { max_tokens, .. } = model.endpoint().options() else {
        return Err(Error::Malformed);
    };
    let mut body = json!({"model":model.model,"messages":input,"stream":true,"max_tokens":max_tokens.unwrap_or(model.profile.max_output_tokens())});
    if !model.instructions.is_empty() {
        body["system"] = model.instructions.clone().into();
    }
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(|tool| json!({"name":tool.name,"description":tool.description,"input_schema":tool.parameters})).collect();
    }
    let ProtocolOptions::AnthropicMessages { cache, .. } = model.endpoint().options() else {
        return Err(Error::Malformed);
    };
    super::cache::anthropic(model, &mut body, cache)?;
    Ok(body)
}

struct Block {
    value: Value,
    arguments: String,
    closed: bool,
}

#[derive(Default)]
struct Parser {
    started: bool,
    blocks: Vec<Block>,
    stop: Option<Stop>,
    usage: Usage,
    raw_usage: Value,
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
        let kind = string(&value["type"])?;
        if kind == "error" {
            return Err(provider_error(&value));
        }
        if event.is_none() {
            if kind != "message" || value["role"] != "assistant" {
                return Err(Error::Malformed);
            }
            let blocks = value["content"].as_array().ok_or(Error::Malformed)?;
            self.usage = usage(&value["usage"]);
            let step = complete(
                blocks.clone(),
                reason(&value["stop_reason"])?,
                self.usage.clone(),
            )?;
            if let Message::Assistant { text: answer, .. } = &step.message {
                text(answer);
            }
            self.result = Some(step);
            return Ok(());
        }
        if let Some(event) = event
            && !event.is_empty()
            && event != kind
        {
            return Err(Error::Malformed);
        }
        match kind {
            "ping" => {}
            "message_start" => {
                let message = &value["message"];
                if self.started
                    || message["type"] != "message"
                    || message["role"] != "assistant"
                    || message["content"]
                        .as_array()
                        .is_none_or(|blocks| !blocks.is_empty())
                {
                    return Err(Error::Malformed);
                }
                self.started = true;
                self.raw_usage = message["usage"].clone();
                self.usage = usage(&message["usage"]);
            }
            "content_block_start" => {
                if !self.started
                    || self.stop.is_some()
                    || value["index"].as_u64() != Some(self.blocks.len() as u64)
                    || self.blocks.iter().any(|block| !block.closed)
                {
                    return Err(Error::Malformed);
                }
                if self.blocks.len() >= 128 {
                    return Err(Error::Limit);
                }
                let block = value["content_block"].clone();
                match block["type"].as_str() {
                    Some("text") => {
                        let initial = string(&block["text"])?;
                        if !initial.is_empty() {
                            text(initial);
                        }
                    }
                    Some("tool_use") => {
                        if !block["input"].is_object() {
                            return Err(Error::Malformed);
                        }
                    }
                    _ => return Err(Error::Unsupported("Anthropic content block")),
                }
                self.blocks.push(Block {
                    value: block,
                    arguments: String::new(),
                    closed: false,
                });
            }
            "content_block_delta" => {
                let block = self.block(&value)?;
                let delta = &value["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") if block.value["type"] == "text" => {
                        let chunk = string(&delta["text"])?;
                        let Value::String(existing) = &mut block.value["text"] else {
                            return Err(Error::Malformed);
                        };
                        existing.push_str(chunk);
                        text(chunk);
                    }
                    Some("input_json_delta") if block.value["type"] == "tool_use" => {
                        block.arguments.push_str(string(&delta["partial_json"])?);
                        if block.arguments.len() > crate::http::MAX_EVENT {
                            return Err(Error::Limit);
                        }
                    }
                    _ => return Err(Error::Unsupported("Anthropic content delta")),
                }
            }
            "content_block_stop" => {
                let block = self.block(&value)?;
                if block.value["type"] == "tool_use" && !block.arguments.is_empty() {
                    let call = call(&block.value["id"], &block.value["name"], &block.arguments)?;
                    block.value["input"] = call.arguments;
                }
                block.closed = true;
            }
            "message_delta" => {
                if !self.started
                    || self.stop.is_some()
                    || self.blocks.iter().any(|block| !block.closed)
                {
                    return Err(Error::Malformed);
                }
                if let Some(value) = value["delta"]
                    .get("stop_reason")
                    .filter(|value| !value.is_null())
                {
                    self.stop = Some(reason(value)?);
                }
                // All delta counters are cumulative. Overlay reports on the start
                // snapshot so omitted fields survive without double-counting writes.
                if let Some(delta) = value["usage"].as_object() {
                    if !self.raw_usage.is_object() {
                        self.raw_usage = json!({});
                    }
                    for (key, count) in delta {
                        if let (Some(existing), Some(nested)) = (
                            self.raw_usage.get_mut(key).and_then(Value::as_object_mut),
                            count.as_object(),
                        ) {
                            existing.extend(nested.clone());
                        } else {
                            self.raw_usage[key] = count.clone();
                        }
                    }
                    self.usage = usage(&self.raw_usage);
                }
            }
            "message_stop" => {
                if !self.started || self.blocks.iter().any(|block| !block.closed) {
                    return Err(Error::Malformed);
                }
                let blocks = self
                    .blocks
                    .iter()
                    .map(|block| block.value.clone())
                    .collect();
                self.result = Some(complete(
                    blocks,
                    self.stop.ok_or(Error::Malformed)?,
                    self.usage.clone(),
                )?);
            }
            _ => return Err(Error::Unsupported("Anthropic streaming event")),
        }
        Ok(())
    }

    fn block(&mut self, value: &Value) -> Result<&mut Block, Error> {
        if !self.started || self.stop.is_some() {
            return Err(Error::Malformed);
        }
        let index = value["index"]
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .ok_or(Error::Malformed)?;
        self.blocks
            .get_mut(index)
            .filter(|block| !block.closed)
            .ok_or(Error::Malformed)
    }
}

fn reason(value: &Value) -> Result<Stop, Error> {
    match string(value)? {
        "end_turn" | "stop_sequence" => Ok(Stop::End),
        "tool_use" => Ok(Stop::Tools),
        "max_tokens" => Ok(Stop::MaxTokens),
        "model_context_window_exceeded" => Err(Error::ContextLimit),
        "refusal" => Ok(Stop::Refusal),
        _ => Err(Error::Unsupported("Anthropic stop reason")),
    }
}

fn usage(value: &Value) -> Usage {
    Usage::anthropic(value)
}

fn complete(blocks: Vec<Value>, stop: Stop, usage: Usage) -> Result<Step, Error> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in &blocks {
        match block["type"].as_str() {
            Some("text") => text.push_str(string(&block["text"])?),
            Some("tool_use") => calls.push(call(
                &block["id"],
                &block["name"],
                &block["input"].to_string(),
            )?),
            _ => return Err(Error::Unsupported("Anthropic content block")),
        }
    }
    if (stop == Stop::Tools) == calls.is_empty() {
        return Err(Error::Malformed);
    }
    Ok(Step {
        message: Message::Assistant {
            text,
            calls,
            native: Some(Native {
                protocol: Protocol::AnthropicMessages,
                items: blocks,
            }),
        },
        stop,
        usage,
    })
}

fn provider_error(value: &Value) -> Error {
    super::provider_error(Protocol::AnthropicMessages, &value["error"])
}
