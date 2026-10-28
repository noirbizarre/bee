//! Cloudflare REST inference policy; routing context stays private to an account.

use super::Protocol;
use toml::{Table, Value};

pub(crate) fn validate_field(name: &str, text: &str) -> Result<(), &'static str> {
    match name {
        // Account IDs are URL segments, never arbitrary URL fragments or credentials.
        "account_id" if text.len() > 64 || text.trim() != text || matches!(text, "." | "..") => {
            Err("Cloudflare account_id must be an unpadded identifier of at most 64 bytes")
        }
        "gateway_id" if text.len() > 64 || text.trim() != text => {
            Err("Cloudflare gateway_id must be an unpadded identifier of 1–64 bytes")
        }
        _ => Ok(()),
    }
}

pub(crate) fn base_url(account_id: &str, protocol: Protocol) -> String {
    let root = format!("https://api.cloudflare.com/client/v4/accounts/{account_id}/ai");
    match protocol {
        // The Messages adapter already supplies /v1; OpenAI adapters do not.
        Protocol::AnthropicMessages => root,
        _ => format!("{root}/v1"),
    }
}

pub(crate) fn defaults(settings: &Table, mut defaults: Table) -> Table {
    let account = settings["account_id"].as_str().expect("validated settings");
    for (name, protocol) in [
        ("openai-chat", Protocol::OpenAiChat),
        ("openai-responses", Protocol::OpenAiResponses),
        ("anthropic-messages", Protocol::AnthropicMessages),
    ] {
        defaults
            .get_mut("protocols")
            .expect("registered protocols")
            .get_mut(name)
            .expect("registered protocol")
            .as_table_mut()
            .expect("endpoint table")
            .insert(
                "base_url".into(),
                Value::String(base_url(account, protocol)),
            );
    }
    defaults
}

pub(crate) fn reserved_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "x-api-key" | "cf-aig-authorization" | "cf-aig-gateway-id"
    )
}

pub(crate) fn validate_route(model: &str, protocol: Protocol) -> Result<(), &'static str> {
    if model.starts_with("@cf/") {
        match protocol {
            Protocol::AnthropicMessages => {
                return Err(
                    "Cloudflare Workers AI models require Chat Completions or supported Responses; change the model protocol",
                );
            }
            Protocol::OpenAiResponses
                if !matches!(model, "@cf/openai/gpt-oss-120b" | "@cf/openai/gpt-oss-20b") =>
            {
                return Err(
                    "Cloudflare Workers AI Responses requires a supported GPT-OSS model; use Chat Completions for other Workers AI models",
                );
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
