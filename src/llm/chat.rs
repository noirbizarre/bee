//! The practical text and function-calling Chat Completions subset.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{Call, Error, Message, Model, Native, Step, Stop, Tool, Usage, call, string};
use crate::provider::{MaxTokensField, Protocol, ProtocolOptions};

pub(super) async fn run(
    model: &Model,
    messages: &[Message],
    tools: &[Tool],
    text: &mut (dyn FnMut(&str) + Send),
) -> Result<Step, Error> {
    let body = request(model, messages, tools)?;
    let mut parser = Parser::default();
    model
        .http
        .post(
            &format!("{}/chat/completions", model.base_url()),
            model.headers(None)?,
            body,
            model.profile.timeout(),
            model.endpoint().protocol(),
            &mut |event, data| parser.event(event, data, text),
        )
        .await?;
    parser.finish()
}

pub(super) fn request(model: &Model, messages: &[Message], tools: &[Tool]) -> Result<Value, Error> {
    let mut messages = messages
        .iter()
        .map(|message| match message {
            Message::User(text) => Ok(json!({"role":"user", "content":text})),
            Message::Assistant {
                text,
                calls,
                native,
            } => assistant(text, calls, native.as_ref()),
            Message::Tool { id, output, .. } => {
                Ok(json!({"role":"tool", "tool_call_id":id,"content":output}))
            }
        })
        .collect::<Result<Vec<_>, Error>>()?;
    if !model.instructions.is_empty() {
        messages.insert(0, json!({"role":"system","content":model.instructions}));
    }
    let mut body = json!({"model":model.model,"messages":messages,"stream":true,"stream_options":{"include_usage":true}});
    let field = match model.endpoint().options() {
        ProtocolOptions::OpenAiChat {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..
        } => "max_completion_tokens",
        _ => "max_tokens",
    };
    body[field] = model.profile.max_output_tokens().into();
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.parameters}})).collect();
    }
    let ProtocolOptions::OpenAiChat { cache, .. } = model.endpoint().options() else {
        return Err(Error::Malformed);
    };
    let boundaries = super::cache::openai(model, &mut body, cache)?;
    if boundaries.instructions() {
        let block = super::cache::marked_text("text", &model.instructions)?;
        body["messages"][0]["content"] = json!([block]);
    }
    if boundaries.conversation() {
        let mut blocks = Vec::new();
        let mut items = body["messages"]
            .as_array_mut()
            .expect("messages array")
            .iter_mut()
            .peekable();
        while let Some(item) = items.next() {
            if item["role"] != "user" && item["role"] != "tool" {
                continue;
            }
            let text = item["content"].as_str().ok_or(Error::Malformed)?;
            if text.is_empty() {
                continue;
            }
            // Historical messages keep the same shape as the bounded marker window advances.
            item["content"] = json!([{"type":"text","text":text}]);
            // Model steps see a complete consecutive tool-result group, so its last
            // result is the reusable endpoint; parallel calls must not consume all slots.
            if item["role"] == "user" || items.peek().is_none_or(|next| next["role"] != "tool") {
                blocks.push(&mut item["content"][0]);
            }
        }
        super::cache::mark_conversation(blocks, &cache.options)?;
    }
    Ok(body)
}

fn assistant(text: &str, calls: &[Call], native: Option<&Native>) -> Result<Value, Error> {
    let mut value = json!({"role":"assistant", "content":text});
    if !calls.is_empty() {
        value["tool_calls"] = calls.iter().map(|call| json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}})).collect();
    }
    let Some(native) = native else {
        return Ok(value);
    };
    if native.protocol != Protocol::OpenAiChat {
        return Err(Error::Unsupported("continuation from another protocol"));
    }
    let [snapshot] = native.items.as_slice() else {
        return Err(Error::Malformed);
    };
    let saved = snapshot["tool_calls"].as_array().ok_or(Error::Malformed)?;
    if saved.len() != calls.len() || saved.is_empty() || saved.len() > 64 {
        return Err(Error::Malformed);
    }
    let mut signature_bytes = 0;
    for (index, (saved, call)) in saved.iter().zip(calls).enumerate() {
        let arguments = string(&saved["function"]["arguments"])?;
        if arguments.len() > crate::http::MAX_EVENT {
            return Err(Error::Limit);
        }
        if serde_json::from_str::<Value>(arguments).map_err(|_| Error::Malformed)? != call.arguments
        {
            return Err(Error::Malformed);
        }
        // Replay exact argument bytes alongside the signature, not a reserialized object.
        value["tool_calls"][index]["function"]["arguments"] = arguments.into();
        if let Some(signature) = signature(saved)? {
            signature_bytes += signature.len();
            value["tool_calls"][index]["extra_content"] =
                json!({"google":{"thought_signature":signature}});
        }
    }
    if signature_bytes > crate::http::MAX_EVENT {
        return Err(Error::Limit);
    }
    // Public Native values must not inject roles or arbitrary provider request fields.
    if signature_bytes == 0 || &value != snapshot {
        return Err(Error::Malformed);
    }
    Ok(value)
}

fn signature(value: &Value) -> Result<Option<&str>, Error> {
    let Some(extra) = value.get("extra_content") else {
        return Ok(None);
    };
    let extra = extra.as_object().ok_or(Error::Malformed)?;
    let Some(google) = extra.get("google") else {
        return Ok(None);
    };
    let google = google.as_object().ok_or(Error::Malformed)?;
    let Some(value) = google.get("thought_signature") else {
        return Ok(None);
    };
    let signature = string(value)?;
    if signature.is_empty() {
        return Err(Error::Malformed);
    }
    if signature.len() > crate::http::MAX_EVENT {
        return Err(Error::Limit);
    }
    Ok(Some(signature))
}

#[derive(Default)]
struct Fragments {
    id: String,
    name: String,
    arguments: String,
    signature: Option<String>,
}

#[derive(Default)]
struct Parser {
    text: String,
    calls: BTreeMap<u64, Fragments>,
    stop: Option<Stop>,
    usage: Usage,
    done: bool,
    refused: bool,
    signature_bytes: usize,
}

impl Parser {
    fn event(
        &mut self,
        event: Option<&str>,
        data: &str,
        text: &mut (dyn FnMut(&str) + Send),
    ) -> Result<(), Error> {
        if data == "[DONE]" {
            if self.done || self.stop.is_none() {
                return Err(Error::Malformed);
            }
            self.done = true;
            return Ok(());
        }
        if self.done {
            return Err(Error::Malformed);
        }
        let value: Value = serde_json::from_str(data).map_err(|_| Error::Malformed)?;
        if value.get("error").is_some() {
            return Err(super::provider_error(
                crate::provider::Protocol::OpenAiChat,
                &value["error"],
            ));
        }
        if let Some(usage) = value.get("usage").filter(|value| !value.is_null()) {
            self.usage.update_snapshot(Usage::openai(usage, false));
        }
        let choices = value["choices"].as_array().ok_or(Error::Malformed)?;
        if choices.len() > 1 {
            return Err(Error::Unsupported("multiple completion choices"));
        }
        for choice in choices {
            if choice["index"].as_u64() != Some(0) || self.stop.is_some() {
                return Err(Error::Malformed);
            }
            let delta = if event.is_none() {
                &choice["message"]
            } else {
                &choice["delta"]
            };
            if !delta.is_object() {
                return Err(Error::Malformed);
            }
            if delta.get("function_call").is_some() || delta.get("audio").is_some() {
                return Err(Error::Unsupported("legacy functions or audio"));
            }
            if let Some(content) = delta.get("content").filter(|value| !value.is_null()) {
                let chunk = string(content)?;
                self.text.push_str(chunk);
                text(chunk);
            }
            if let Some(refusal) = delta.get("refusal").filter(|value| !value.is_null()) {
                self.refused = true;
                let refusal = string(refusal)?;
                self.text.push_str(refusal);
                text(refusal);
            }
            if let Some(calls) = delta.get("tool_calls") {
                for (position, value) in
                    calls.as_array().ok_or(Error::Malformed)?.iter().enumerate()
                {
                    let index = if event.is_none() {
                        position as u64
                    } else {
                        value["index"].as_u64().ok_or(Error::Malformed)?
                    };
                    if index >= 64 {
                        return Err(Error::Limit);
                    }
                    if let Some(kind) = value.get("type")
                        && kind != "function"
                    {
                        return Err(Error::Unsupported("non-function tool calls"));
                    }
                    let call = self.calls.entry(index).or_default();
                    if let Some(signature) = signature(value)? {
                        // Signatures are opaque snapshots, never argument-style text deltas.
                        if let Some(previous) = &call.signature {
                            if previous != signature {
                                return Err(Error::Malformed);
                            }
                        } else {
                            self.signature_bytes += signature.len();
                            if self.signature_bytes > crate::http::MAX_EVENT {
                                return Err(Error::Limit);
                            }
                            call.signature = Some(signature.to_owned());
                        }
                    }
                    for (target, part) in [
                        (&mut call.id, &value["id"]),
                        (&mut call.name, &value["function"]["name"]),
                        (&mut call.arguments, &value["function"]["arguments"]),
                    ] {
                        if !part.is_null() {
                            target.push_str(string(part)?);
                        }
                        if target.len() > crate::http::MAX_EVENT {
                            return Err(Error::Limit);
                        }
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").filter(|value| !value.is_null()) {
                self.stop = Some(match string(reason)? {
                    "stop" => {
                        if self.refused {
                            Stop::Refusal
                        } else {
                            Stop::End
                        }
                    }
                    "tool_calls" => Stop::Tools,
                    "length" => Stop::MaxTokens,
                    "content_filter" => Stop::Refusal,
                    _ => return Err(Error::Unsupported("completion stop reason")),
                });
            }
        }
        if event.is_none() {
            self.done = true;
        }
        Ok(())
    }

    fn finish(self) -> Result<Step, Error> {
        if !self.done {
            return Err(Error::Malformed);
        }
        let mut stop = self.stop.ok_or(Error::Malformed)?;
        // Refusal content can arrive separately from the finish reason and must never authorize tools.
        if self.refused {
            if !self.calls.is_empty() {
                return Err(Error::Malformed);
            }
            stop = Stop::Refusal;
        }
        let calls: Vec<_> = self
            .calls
            .values()
            .map(|fragments| {
                call(
                    &fragments.id.clone().into(),
                    &fragments.name.clone().into(),
                    &fragments.arguments,
                )
            })
            .collect::<Result<_, _>>()?;
        if (stop == Stop::Tools) == calls.is_empty() {
            return Err(Error::Malformed);
        }
        let native = if self.signature_bytes == 0 {
            None
        } else {
            let mut snapshot = assistant(&self.text, &calls, None)?;
            for (index, fragments) in self.calls.into_values().enumerate() {
                snapshot["tool_calls"][index]["function"]["arguments"] = fragments.arguments.into();
                if let Some(signature) = fragments.signature {
                    snapshot["tool_calls"][index]["extra_content"] =
                        json!({"google":{"thought_signature":signature}});
                }
            }
            Some(Native {
                protocol: Protocol::OpenAiChat,
                items: vec![snapshot],
            })
        };
        Ok(Step {
            message: Message::Assistant {
                text: self.text,
                calls,
                native,
            },
            stop,
            usage: self.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_metadata_is_bounded_across_individually_valid_events() {
        let mut parser = Parser::default();
        let signature = "s".repeat(crate::http::MAX_EVENT / 2 + 1);
        let event = |index| {
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":index,"extra_content":{"google":{"thought_signature":signature}}}]},"finish_reason":null}]}).to_string()
        };
        parser.event(Some(""), &event(0), &mut |_| {}).unwrap();
        // Repeated snapshots retain no additional bytes.
        parser.event(Some(""), &event(0), &mut |_| {}).unwrap();
        assert_eq!(
            parser.event(Some(""), &event(1), &mut |_| {}).unwrap_err(),
            Error::Limit
        );
        let too_large = json!({"extra_content":{"google":{"thought_signature":"s".repeat(crate::http::MAX_EVENT + 1)}}});
        assert_eq!(super::signature(&too_large).unwrap_err(), Error::Limit);
    }
}
