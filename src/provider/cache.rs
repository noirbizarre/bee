//! Typed native cache settings. These describe wire policies, not a response cache.

use super::{Reader, Result, Table};

/// Key routing policy, distinct from enabling or disabling provider caching.
#[derive(Clone, PartialEq, Eq, Default)]
pub enum CacheKey {
    /// Bee's default: derive a key only on documented supporting endpoints.
    #[default]
    Default,
    /// Explicitly require a supporting endpoint and derive a stable key.
    Auto,
    /// Omit the key; this does not disable provider caching.
    Disabled,
    /// Caller-controlled stable key, never included in diagnostics.
    Explicit(String),
}

impl std::fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Default => "Default",
            Self::Auto => "Auto",
            Self::Disabled => "Disabled",
            Self::Explicit(_) => "Explicit([redacted])",
        })
    }
}

/// Legacy OpenAI retention, independent of response storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Retention {
    /// Preserve the provider default and omit the field.
    #[default]
    Default,
    /// In-memory retention on supporting older models.
    InMemory,
    /// Extended retention on explicitly documented models.
    Hours24,
}

/// OpenAI's native breakpoint mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Permit the provider's implicit breakpoint.
    #[default]
    Implicit,
    /// Write only selected explicit breakpoints.
    Explicit,
}

/// OpenAI's minimum write lifetime, not an eviction deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiTtl {
    /// The short native lifetime.
    Minutes30,
    /// Responses-only extended lifetime with a higher write rate.
    Hours12,
}

/// Bee's placement policy for eligible OpenAI content blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Breakpoints {
    /// No explicit content annotations.
    #[default]
    None,
    /// End of the instruction payload.
    Instructions,
    /// Latest eligible user or tool-result boundary.
    Conversation,
    /// Both boundaries, within the native write-slot limit.
    InstructionsAndConversation,
}

impl Breakpoints {
    pub(crate) fn instructions(self) -> bool {
        matches!(self, Self::Instructions | Self::InstructionsAndConversation)
    }
    pub(crate) fn conversation(self) -> bool {
        matches!(self, Self::Conversation | Self::InstructionsAndConversation)
    }
}

/// Explicit opt-in to newer OpenAI cache-write controls.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenAiCacheOptions {
    /// Disabled settings serialize no new options or markers.
    pub enabled: bool,
    /// Native implicit or explicit-only mode.
    pub mode: CacheMode,
    /// Omission retains the provider's short lifetime.
    pub ttl: Option<OpenAiTtl>,
    /// Placement policy; never forwarded as an API option.
    pub breakpoints: Breakpoints,
}

/// Controls shared by the two OpenAI wire APIs, with separate route validation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenAiCache {
    /// Stable routing/accounting key.
    pub key: CacheKey,
    /// Older-model retention.
    pub retention: Retention,
    /// Newer-model mode, lifetime and explicit boundaries.
    pub options: OpenAiCacheOptions,
}

/// Anthropic cache-write placement, disabled unless explicitly selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnthropicPolicy {
    /// No cache-write controls.
    #[default]
    Disabled,
    /// Let Anthropic advance the conversation boundary.
    Automatic,
    /// Mark only the stable system/tools prefix.
    StablePrefix,
    /// Mark the stable prefix and advance the conversation boundary.
    StablePrefixAndAutomatic,
}

/// Anthropic's native ephemeral lifetimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnthropicTtl {
    /// Short lifetime after explicit write-policy opt-in.
    #[default]
    Minutes5,
    /// Higher-priced extended lifetime.
    Hour1,
}

/// Anthropic settings deliberately do not reuse OpenAI cache semantics.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnthropicCache {
    /// Explicit write-placement policy.
    pub policy: AnthropicPolicy,
    /// One lifetime applies to both selected boundaries.
    pub ttl: AnthropicTtl,
}

impl Reader<'_> {
    pub(super) fn openai_cache(&self, key: &str, table: &Table) -> Result<OpenAiCache> {
        let mut cache = OpenAiCache::default();
        if let Some(value) = table.get("prompt_cache_key") {
            let key = format!("{key}.prompt_cache_key");
            let table = self.table(&key, value)?;
            self.only_keys(&key, table, &["mode", "value"])?;
            let mode = self.cache_enum(&key, table, "mode", "auto")?;
            // Validate even inactive inherited values, without revealing their contents.
            let explicit = table
                .get("value")
                .map(|v| self.string(&format!("{key}.value"), v))
                .transpose()?;
            if explicit
                .is_some_and(|v| v.is_empty() || v.len() > 64 || v.chars().any(char::is_control))
            {
                return Err(self.invalid(
                    &format!("{key}.value"),
                    "must contain 1 to 64 bytes without control characters",
                ));
            }
            cache.key = match mode {
                "auto" => CacheKey::Auto,
                "disabled" => CacheKey::Disabled,
                "explicit" => CacheKey::Explicit(
                    explicit
                        .ok_or_else(|| self.invalid(&key, "explicit mode requires value"))?
                        .into(),
                ),
                _ => {
                    return Err(self.invalid(
                        &format!("{key}.mode"),
                        "must be `auto`, `disabled` or `explicit`",
                    ));
                }
            };
        }
        if let Some(value) = table.get("prompt_cache_retention") {
            let key = format!("{key}.prompt_cache_retention");
            cache.retention = match self.string(&key, value)? {
                "default" => Retention::Default,
                "in_memory" => Retention::InMemory,
                "24h" => Retention::Hours24,
                _ => return Err(self.invalid(&key, "must be `default`, `in_memory` or `24h`")),
            };
        }
        if let Some(value) = table.get("prompt_cache_options") {
            let key = format!("{key}.prompt_cache_options");
            let table = self.table(&key, value)?;
            self.only_keys(&key, table, &["enabled", "mode", "ttl", "breakpoints"])?;
            if let Some(value) = table.get("enabled") {
                cache.options.enabled = value.as_bool().ok_or_else(|| {
                    self.wrong_type(&format!("{key}.enabled"), "a boolean", value)
                })?;
            }
            cache.options.mode = match self.cache_enum(&key, table, "mode", "implicit")? {
                "implicit" => CacheMode::Implicit,
                "explicit" => CacheMode::Explicit,
                _ => {
                    return Err(
                        self.invalid(&format!("{key}.mode"), "must be `implicit` or `explicit`")
                    );
                }
            };
            cache.options.ttl = match self.cache_enum(&key, table, "ttl", "default")? {
                "default" => None,
                "30m" => Some(OpenAiTtl::Minutes30),
                "12h" => Some(OpenAiTtl::Hours12),
                _ => {
                    return Err(
                        self.invalid(&format!("{key}.ttl"), "must be `default`, `30m` or `12h`")
                    );
                }
            };
            cache.options.breakpoints = match self.cache_enum(&key, table, "breakpoints", "none")? {
                "none" => Breakpoints::None,
                "instructions" => Breakpoints::Instructions,
                "conversation" => Breakpoints::Conversation,
                "instructions-and-conversation" => Breakpoints::InstructionsAndConversation,
                _ => return Err(self.invalid(&format!("{key}.breakpoints"), "must be `none`, `instructions`, `conversation` or `instructions-and-conversation`")),
            };
        }
        if cache.options.enabled && cache.retention != Retention::Default {
            return Err(self.invalid(
                key,
                "choose prompt_cache_retention or enabled prompt_cache_options, not both",
            ));
        }
        Ok(cache)
    }

    pub(super) fn anthropic_cache(&self, key: &str, table: &Table) -> Result<AnthropicCache> {
        let mut cache = AnthropicCache::default();
        if let Some(value) = table.get("cache_control") {
            let key = format!("{key}.cache_control");
            let table = self.table(&key, value)?;
            self.only_keys(&key, table, &["policy", "ttl"])?;
            cache.policy = match self.cache_enum(&key, table, "policy", "disabled")? {
                "disabled" => AnthropicPolicy::Disabled,
                "automatic" => AnthropicPolicy::Automatic,
                "stable-prefix" => AnthropicPolicy::StablePrefix,
                "stable-prefix-and-automatic" => AnthropicPolicy::StablePrefixAndAutomatic,
                _ => return Err(self.invalid(&format!("{key}.policy"), "must be `disabled`, `automatic`, `stable-prefix` or `stable-prefix-and-automatic`")),
            };
            cache.ttl = match self.cache_enum(&key, table, "ttl", "5m")? {
                "5m" => AnthropicTtl::Minutes5,
                "1h" => AnthropicTtl::Hour1,
                _ => return Err(self.invalid(&format!("{key}.ttl"), "must be `5m` or `1h`")),
            };
        }
        Ok(cache)
    }

    fn cache_enum<'a>(
        &self,
        key: &str,
        table: &'a Table,
        field: &str,
        default: &'a str,
    ) -> Result<&'a str> {
        table
            .get(field)
            .map_or(Ok(default), |v| self.string(&format!("{key}.{field}"), v))
    }
}
