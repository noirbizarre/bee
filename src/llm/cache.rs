//! Narrow native request policies, checked against effective routes rather than preset names.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Error, Model};
use crate::provider::Protocol;
use crate::provider::cache::{
    AnthropicCache, AnthropicPolicy, AnthropicTtl, Breakpoints, CacheKey, CacheMode, OpenAiCache,
    OpenAiCacheOptions, OpenAiTtl, Retention,
};

#[cfg(test)]
mod tests;

const OPENAI: &str = "https://api.openai.com/v1";
const OPENROUTER: &str = "https://openrouter.ai/api/v1";

// Only known aliases and their date snapshots inherit model-specific controls.
// An unknown future/private model still infers normally with provider defaults.
fn named_model(model: &str, names: &[&str]) -> bool {
    names.iter().any(|name| {
        model == *name
            || model.strip_prefix(name).is_some_and(|suffix| {
                let bytes = suffix.as_bytes();
                bytes.len() == 11
                    && bytes[0] == b'-'
                    && bytes[5] == b'-'
                    && bytes[8] == b'-'
                    && bytes
                        .iter()
                        .enumerate()
                        .all(|(i, c)| matches!(i, 0 | 5 | 8) || c.is_ascii_digit())
            })
    })
}

fn new_openai(model: &str) -> bool {
    named_model(
        model,
        &[
            "gpt-5.6",
            "gpt-5.6-pro",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-6",
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-6.1-sol",
        ],
    )
}

fn extended_openai(model: &str) -> bool {
    named_model(
        model,
        &[
            "gpt-5.5",
            "gpt-5.5-pro",
            "gpt-5.4",
            "gpt-5.2",
            "gpt-5.1-codex-max",
            "gpt-5.1",
            "gpt-5.1-codex",
            "gpt-5.1-codex-mini",
            "gpt-5.1-chat-latest",
            "gpt-5",
            "gpt-5-codex",
            "gpt-4.1",
        ],
    )
}

/// Apply only OpenAI-native root fields; adapters own their differing content shapes.
pub(super) fn openai(
    model: &Model,
    body: &mut Value,
    settings: &OpenAiCache,
) -> Result<Breakpoints, Error> {
    let endpoint = model.endpoint();
    let url = endpoint.base_url();
    let responses = endpoint.protocol() == Protocol::OpenAiResponses;
    let router = url == OPENROUTER;
    let key_supported = matches!(url, OPENAI | OPENROUTER)
        || (url == "https://api.x.ai/v1" && responses)
        || (url == "https://api.mistral.ai/v1" && !responses);
    let default_key = key_supported && !router;
    match &settings.key {
        CacheKey::Disabled => {}
        CacheKey::Default if !default_key => {}
        CacheKey::Default | CacheKey::Auto | CacheKey::Explicit(_) => {
            if !key_supported {
                return Err(Error::Cache("prompt_cache_key"));
            }
            let key = if let CacheKey::Explicit(value) = &settings.key {
                value.clone()
            } else {
                // JSON arrays length-delimit fields; no credentials, paths, prompts or volatile IDs.
                let identity = json!([
                    "bee-cache-v1",
                    model.cache_scope,
                    model.profile.name(),
                    url,
                    endpoint.protocol().name(),
                    model.is_chatgpt(),
                    model.model
                ]);
                let digest =
                    Sha256::digest(serde_json::to_vec(&identity).expect("identity is JSON"));
                // 56 hex digits keep the versioned key within the documented 64-byte bound.
                let hex: String = digest
                    .iter()
                    .take(28)
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                format!("bee-v1-{hex}")
            };
            body["prompt_cache_key"] = key.into();
        }
    }
    let native_model = if router {
        model.model.strip_prefix("openai/").unwrap_or("")
    } else {
        &model.model
    };
    if settings.retention != Retention::Default {
        let legacy = !new_openai(native_model);
        let supported = url == OPENAI
            && !model.is_chatgpt()
            && legacy
            && match settings.retention {
                Retention::InMemory => {
                    !named_model(native_model, &["gpt-5.5", "gpt-5.5-pro"])
                        && (extended_openai(native_model)
                            || named_model(
                                native_model,
                                &[
                                    "gpt-4o",
                                    "gpt-4o-mini",
                                    "gpt-4.1-mini",
                                    "gpt-4.1-nano",
                                    "gpt-5-mini",
                                    "gpt-5-nano",
                                    "o1",
                                    "o3",
                                    "o3-mini",
                                    "o4-mini",
                                ],
                            ))
                }
                Retention::Hours24 => extended_openai(native_model),
                Retention::Default => true,
            };
        if !supported {
            return Err(Error::Cache("prompt_cache_retention"));
        }
        body["prompt_cache_retention"] = match settings.retention {
            Retention::InMemory => "in_memory",
            Retention::Hours24 => "24h",
            Retention::Default => unreachable!(),
        }
        .into();
    }
    if !settings.options.enabled {
        return Ok(Breakpoints::None);
    }
    // OAuth's preview contract is separate from API-key Responses, even at the same URL.
    if !(url == OPENAI || router) || model.is_chatgpt() || !new_openai(native_model) {
        return Err(Error::Cache("prompt_cache_options"));
    }
    if settings.retention != Retention::Default {
        return Err(Error::Cache(
            "prompt_cache_retention with prompt_cache_options",
        ));
    }
    if settings.options.ttl == Some(OpenAiTtl::Hours12) && !responses && !router {
        return Err(Error::Cache(
            "prompt_cache_options.ttl (12h requires Responses on OpenAI)",
        ));
    }
    body["prompt_cache_options"] = json!({"mode": match settings.options.mode {
        CacheMode::Implicit => "implicit", CacheMode::Explicit => "explicit",
    }});
    if let Some(ttl) = settings.options.ttl {
        body["prompt_cache_options"]["ttl"] = match ttl {
            OpenAiTtl::Minutes30 => "30m",
            OpenAiTtl::Hours12 => "12h",
        }
        .into();
    }
    Ok(settings.options.breakpoints)
}

/// Anthropic's hierarchy and ephemeral policy are independent of OpenAI routing keys.
pub(super) fn anthropic(
    model: &Model,
    body: &mut Value,
    settings: &AnthropicCache,
) -> Result<(), Error> {
    if settings.policy == AnthropicPolicy::Disabled {
        return Ok(());
    }
    let url = model.endpoint().base_url();
    let name = if url == "https://openrouter.ai/api" {
        model.model.strip_prefix("anthropic/").unwrap_or("")
    } else if url == "https://api.anthropic.com" {
        &model.model
    } else {
        return Err(Error::Cache("cache_control endpoint"));
    };
    // Gateway IDs use dotted versions, unlike Anthropic's native aliases.
    // Normalize only the capability lookup; the configured wire model stays untouched.
    let name = if url == "https://openrouter.ai/api" {
        name.replace('.', "-")
    } else {
        name.to_owned()
    };
    // Claude uses YYYYMMDD snapshots rather than OpenAI's hyphenated dates.
    let name = name
        .rsplit_once('-')
        .filter(|(_, date)| date.len() == 8 && date.bytes().all(|c| c.is_ascii_digit()))
        .map_or(name.as_str(), |(alias, _)| alias);
    if ![
        "claude-sonnet-4",
        "claude-sonnet-4-5",
        "claude-sonnet-4-6",
        "claude-sonnet-5",
        "claude-sonnet-5-5",
        "claude-opus-4",
        "claude-opus-4-1",
        "claude-opus-4-5",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-opus-5-5",
        "claude-haiku-4-5",
        "claude-haiku-5-5",
        "claude-fable-5",
        "claude-fable-5-1",
        "claude-mythos-5",
        "claude-mythos-5-1",
    ]
    .contains(&name)
    {
        return Err(Error::Cache("cache_control model"));
    }
    let marker = json!({"type":"ephemeral", "ttl":match settings.ttl {
        AnthropicTtl::Minutes5 => "5m", AnthropicTtl::Hour1 => "1h",
    }});
    if matches!(
        settings.policy,
        AnthropicPolicy::StablePrefix | AnthropicPolicy::StablePrefixAndAutomatic
    ) {
        if !model.instructions.is_empty() {
            body["system"] =
                json!([{"type":"text","text":model.instructions,"cache_control":marker}]);
        } else if let Some(tool) = body
            .get_mut("tools")
            .and_then(Value::as_array_mut)
            .and_then(|t| t.last_mut())
        {
            tool["cache_control"] = marker.clone();
        } else {
            return Err(Error::Cache(
                "cache_control stable-prefix requires instructions or tools",
            ));
        }
    }
    if matches!(
        settings.policy,
        AnthropicPolicy::Automatic | AnthropicPolicy::StablePrefixAndAutomatic
    ) {
        body["cache_control"] = marker;
    }
    Ok(())
}

/// A request-only annotation: never persist markers in native continuation.
pub(super) fn marked_text(kind: &str, text: &str) -> Result<Value, Error> {
    if text.is_empty() {
        return Err(Error::Cache("breakpoints require nonempty eligible text"));
    }
    Ok(json!({"type":kind,"text":text,"prompt_cache_breakpoint":{"mode":"explicit"}}))
}

/// Keep recent lookup boundaries as well as the new write endpoint. Explicit-only
/// mode cannot find a previous write when its boundary disappears from the request.
pub(super) fn mark_conversation(
    blocks: Vec<&mut Value>,
    options: &OpenAiCacheOptions,
) -> Result<(), Error> {
    if blocks.is_empty() {
        return Err(Error::Cache(
            "conversation breakpoint requires nonempty user text or a tool result",
        ));
    }
    // Implicit caching and the stable instruction boundary each reserve a write slot.
    let slots = 4
        - usize::from(options.mode == CacheMode::Implicit)
        - usize::from(options.breakpoints.instructions());
    for block in blocks.into_iter().rev().take(slots) {
        block["prompt_cache_breakpoint"] = json!({"mode":"explicit"});
    }
    Ok(())
}
