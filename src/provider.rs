//! Provider connections, free of the ACPv2 SDK.
//!
//! The historical Rust type `Profile` is a named connection: which protocols
//! the endpoint speaks and where, with which headers, and which model to ask
//! for by default. Protocols are properties of the connection, never inferred from its name, and
//! nothing here knows a commercial provider: any name and any model id is
//! accepted. One provider can serve several protocols, each at its own URL
//! and with its own options; exact `model_protocols` mappings override `prefer`.
//! See ADR-007, ADR-010 and ADR-013.
//!
//! Connections are read from the merged [`Config`] (ADR-006) under
//! `providers.<name>`, optionally laid over a preset (ADR-009). Named grouping
//! profiles scope the returned collection and its default connection (ADR-022).
//! Nothing is sent anywhere yet:
//! this module only describes endpoints. A credential is a *reference* to where
//! the secret lives; resolving it is `auth` (ADR-008).
//!
//! The validation is written by hand over the generic TOML value instead of
//! derived with serde: serde's error text quotes the offending value, and
//! configuration files are where users keep API keys. Every diagnostic here
//! names a key and a file, never a value.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use toml::{Table, Value};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::preset::{Catalog, LoginSpec, SpecError, deep_merge};

pub mod cache;
use cache::{AnthropicCache, OpenAiCache};

/// The keys a profile table accepts.
const PROFILE_KEYS: &[&str] = &[
    "auth",
    "preset",
    "protocols",
    "prefer",
    "model",
    "models",
    "model_protocols",
    "credential",
    "headers",
    "timeout_ms",
    "max_output_tokens",
    "context_window",
    "model_context_windows",
    "catalog_provider",
    "catalog_protocol_hints",
    "catalog_context_window",
    "discover_models",
    "discovery",
    "metadata",
    "model_metadata",
];

/// The keys a table under `protocols` accepts.
const ENDPOINT_KEYS: &[&str] = &["base_url", "options"];

/// The keys a preset table accepts: a profile's, but a preset cannot be built
/// on another, so chains and cycles cannot exist.
const PRESET_KEYS: &[&str] = &[
    "auth",
    "protocols",
    "prefer",
    "model",
    "models",
    "model_protocols",
    "credential",
    "headers",
    "timeout_ms",
    "max_output_tokens",
    "context_window",
    "model_context_windows",
    "catalog_provider",
    "catalog_protocol_hints",
    "catalog_context_window",
    "discover_models",
    "discovery",
    "metadata",
    "model_metadata",
];

/// How Bee talks to an endpoint.
///
/// A closed set on purpose: each variant is code that exists. New endpoints
/// need no variant, only a profile that picks one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum Protocol {
    /// OpenAI Chat Completions, and the many servers that copy it.
    OpenAiChat,
    /// OpenAI Responses, the newer OpenAI API. Another endpoint with other
    /// request and event shapes, so it is a protocol of its own and not an
    /// option of `OpenAiChat` (ADR-011).
    OpenAiResponses,
    /// Anthropic Messages.
    AnthropicMessages,
}

impl Protocol {
    /// Every protocol, in the order diagnostics list them.
    // Appended to, never reordered: `prefer` puts the protocols it leaves out in
    // this order, so inserting before an existing one would change which of them
    // a configuration that omits it ends up preferring.
    pub const ALL: [Self; 3] = [
        Self::OpenAiChat,
        Self::OpenAiResponses,
        Self::AnthropicMessages,
    ];

    /// The name used in configuration.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
        }
    }

    /// The base URL used when a profile gives none.
    ///
    /// Per protocol, not per provider: a profile pointed anywhere else
    /// overrides it, and no provider name is ever looked up.
    #[must_use]
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAiChat | Self::OpenAiResponses => "https://api.openai.com/v1",
            Self::AnthropicMessages => "https://api.anthropic.com",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|protocol| protocol.name() == name)
    }

    fn supported() -> String {
        Self::ALL.map(Self::name).join(", ")
    }
}

/// Where a profile's credential comes from.
///
/// Only a reference: resolving it, and any secret handling, is `auth`'s
/// (ADR-008). It says *where* the secret is, never how a request presents it:
/// that is the protocol's business. Adding a source kind later is a new variant
/// and a new accepted key.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialSource {
    /// The environment variable with this name.
    Env(String),
    /// Bee's credential store, written by `bee auth login`.
    Store {
        /// The name of the browser sign-in `bee auth login` may run, a
        /// `login_flows` entry (ADR-009). Without it, only a pasted token can
        /// be stored.
        login: Option<String>,
    },
}

impl fmt::Debug for CredentialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Even references are user-configured strings, not safe diagnostic material.
        f.debug_tuple("CredentialSource")
            .field(&self.kind())
            .finish()
    }
}

impl CredentialSource {
    /// A short word for the source's kind, safe to print: never a value.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Env(_) => "env",
            Self::Store { .. } => "store",
        }
    }
}

/// The field that carries the output limit in an OpenAI-compatible request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum MaxTokensField {
    /// `max_tokens`, which most compatible servers understand.
    #[default]
    MaxTokens,
    /// `max_completion_tokens`, which newer OpenAI models require.
    MaxCompletionTokens,
}

/// Settings only one protocol understands.
///
/// Typed per protocol, so an option for one backend cannot be given to a
/// profile that uses the other.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolOptions {
    /// For [`Protocol::OpenAiChat`].
    OpenAiChat {
        /// Which field carries the output limit.
        max_tokens_field: MaxTokensField,
        /// Native OpenAI caching controls; compatibility is checked on the selected route.
        cache: OpenAiCache,
    },
    /// For [`Protocol::OpenAiResponses`].
    OpenAiResponses {
        /// Native OpenAI caching controls.
        cache: OpenAiCache,
    },
    /// For [`Protocol::AnthropicMessages`].
    AnthropicMessages {
        /// The `anthropic-version` header, if not the backend's own.
        version: Option<String>,
        /// The output limit, if not the backend's own.
        max_tokens: Option<u32>,
        /// Explicit opt-in to Anthropic cache writes.
        cache: AnthropicCache,
    },
}

impl fmt::Debug for ProtocolOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenAiChat {
                max_tokens_field,
                cache,
            } => f
                .debug_struct("OpenAiChat")
                .field("max_tokens_field", max_tokens_field)
                .field("cache", cache)
                .finish(),
            Self::OpenAiResponses { cache } => f
                .debug_struct("OpenAiResponses")
                .field("cache", cache)
                .finish(),
            Self::AnthropicMessages {
                version,
                max_tokens,
                cache,
            } => f
                .debug_struct("AnthropicMessages")
                // A version can contain any accepted text, including an accidentally pasted secret.
                .field("has_version", &version.is_some())
                .field("has_max_tokens", &max_tokens.is_some())
                .field("cache", cache)
                .finish(),
        }
    }
}

impl ProtocolOptions {
    fn defaults(protocol: Protocol) -> Self {
        match protocol {
            Protocol::OpenAiChat => Self::OpenAiChat {
                max_tokens_field: MaxTokensField::default(),
                cache: OpenAiCache::default(),
            },
            Protocol::OpenAiResponses => Self::OpenAiResponses {
                cache: OpenAiCache::default(),
            },
            Protocol::AnthropicMessages => Self::AnthropicMessages {
                version: None,
                max_tokens: None,
                cache: AnthropicCache::default(),
            },
        }
    }

    fn allowed_keys(protocol: Protocol) -> &'static [&'static str] {
        match protocol {
            Protocol::OpenAiChat => &[
                "max_tokens_field",
                "prompt_cache_key",
                "prompt_cache_retention",
                "prompt_cache_options",
            ],
            Protocol::OpenAiResponses => &[
                "prompt_cache_key",
                "prompt_cache_retention",
                "prompt_cache_options",
            ],
            Protocol::AnthropicMessages => &["version", "max_tokens", "cache_control"],
        }
    }
}

/// One protocol a profile's provider speaks, and where.
///
/// The unit a backend is handed: the URL and the options belong to the
/// protocol, since a provider that serves two usually serves them from two
/// places.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    protocol: Protocol,
    base_url: String,
    options: ProtocolOptions,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Gateway paths may carry credentials even when URL userinfo is forbidden.
        f.debug_struct("Endpoint")
            .field("protocol", &self.protocol)
            .field("base_url", &"[redacted]")
            .field("options", &self.options)
            .finish()
    }
}

impl Endpoint {
    /// How to talk to it.
    #[must_use]
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// Its base URL: the profile's own for this protocol, else the protocol's
    /// default. Never ends with a slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The settings only this protocol understands.
    #[must_use]
    pub fn options(&self) -> &ProtocolOptions {
        &self.options
    }
}

/// One named connection profile.
#[derive(Clone, PartialEq, Eq)]
pub struct Profile {
    copilot: bool,
    name: String,
    // In preference order, never empty. The order is data, not an accident of
    // how the file was written: see `prefer`.
    endpoints: Vec<Endpoint>,
    model: Option<String>,
    models: Vec<String>,
    model_protocols: BTreeMap<String, Protocol>,
    credential: Option<CredentialSource>,
    // The sign-in `credential.login` names, resolved when the profile was read
    // so a dangling or malformed flow stops Bee at launch.
    login: Option<Arc<LoginSpec>>,
    headers: Vec<(String, String)>,
    timeout_ms: u32,
    max_output_tokens: u32,
    context_window: Option<u32>,
    model_context_windows: BTreeMap<String, u32>,
    catalog_provider: Option<String>,
    catalog_protocol_hints: bool,
    catalog_context_window: bool,
    discover_models: bool,
    discovery: Option<crate::models::Integration>,
    metadata: crate::models::Metadata,
    model_metadata: BTreeMap<String, crate::models::Metadata>,
}

// Configuration strings can contain secrets regardless of the field's intended purpose.
impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("endpoints", &self.endpoints)
            .field("has_model", &self.model.is_some())
            .field("model_count", &self.models.len())
            .field(
                "credential",
                &self.credential.as_ref().map(CredentialSource::kind),
            )
            .field("header_count", &self.headers.len())
            .finish()
    }
}

impl Profile {
    /// Copilot authentication is independent of optional catalogue discovery.
    #[must_use]
    pub fn is_copilot(&self) -> bool {
        self.copilot
    }
    pub(crate) fn with_model_preferences(
        mut self,
        model: Option<String>,
        models: Option<Vec<String>>,
    ) -> Self {
        // Only the scoped clone changes: shared connection credentials and other
        // profiles' preferences must retain their original identity and defaults.
        if let Some(model) = model {
            self.model = Some(model);
        }
        if let Some(models) = models {
            self.models = models;
        }
        self
    }
    /// Exact Models.dev association; never a required registry entry.
    pub fn catalog_provider(&self) -> Option<&str> {
        self.catalog_provider.as_deref()
    }
    /// Whether published SDK signals may inform protocol selection.
    pub fn catalog_protocol_hints(&self) -> bool {
        self.catalog_protocol_hints
    }
    /// Whether catalog capacities may enforce context eviction.
    pub fn catalog_context_window(&self) -> bool {
        self.catalog_context_window
    }
    /// Whether to enumerate this endpoint automatically with existing credentials.
    pub fn discover_models(&self) -> bool {
        self.discover_models
    }
    /// Native catalogue identity is independent of inference protocol and public metadata.
    pub fn discovery(&self) -> Option<crate::models::Integration> {
        self.discovery
    }
    /// Explicit profile-wide descriptive defaults.
    pub fn metadata(&self) -> &crate::models::Metadata {
        &self.metadata
    }
    /// Exact model overrides; IDs remain opaque and case-sensitive.
    pub fn model_metadata(&self, model: &str) -> Option<&crate::models::Metadata> {
        self.model_metadata.get(model)
    }
    /// Whether routing was explicitly configured for this ID.
    pub fn has_model_protocol(&self, model: &str) -> bool {
        self.model_protocols.contains_key(model)
    }

    /// A hint can only select an already-configured protocol, below explicit mappings.
    pub fn endpoint_for_model_with_hint(&self, model: &str, hint: Option<Protocol>) -> &Endpoint {
        self.model_protocols
            .get(model)
            .and_then(|protocol| self.endpoint(*protocol))
            .or_else(|| hint.and_then(|protocol| self.endpoint(protocol)))
            .unwrap_or_else(|| self.preferred())
    }
    /// Explicit capacity for an opaque model; unknown capacity stays unknown.
    #[must_use]
    pub fn context_window(&self, model: &str) -> Option<u32> {
        self.model_context_windows
            .get(model)
            .copied()
            .or(self.context_window)
    }

    /// Whole-request deadline, including streamed body reads. Defaults to 120s.
    #[must_use]
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(u64::from(self.timeout_ms))
    }

    /// Common output limit, defaulting to 4096 tokens.
    #[must_use]
    pub fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }
    /// The profile's name, as written in configuration.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Every protocol the provider speaks, most preferred first.
    #[must_use]
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }

    /// The endpoint for `protocol`, if the profile lists it.
    #[must_use]
    pub fn endpoint(&self, protocol: Protocol) -> Option<&Endpoint> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.protocol == protocol)
    }

    /// The fallback endpoint: the first of [`Profile::endpoints`].
    #[must_use]
    pub fn preferred(&self) -> &Endpoint {
        // Validation guarantees at least one endpoint.
        &self.endpoints[0]
    }

    /// Resolve an opaque model ID without a catalog or network access.
    /// Exact configured mappings win; every other ID uses the preference order.
    #[must_use]
    pub fn endpoint_for_model(&self, model: &str) -> &Endpoint {
        match self.model_protocols.get(model) {
            // Validation guarantees mapped protocols belong to this profile.
            Some(protocol) => self.endpoint(*protocol).expect("validated model protocol"),
            None => self.preferred(),
        }
    }

    /// The preferred endpoint's protocol.
    #[must_use]
    pub fn protocol(&self) -> Protocol {
        self.preferred().protocol
    }

    /// The preferred endpoint's base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.preferred().base_url
    }

    /// The configured default model id, if any.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The model ids the profile suggests to a client. A hint for selectors:
    /// any other id is still accepted.
    #[must_use]
    pub fn models(&self) -> &[String] {
        &self.models
    }

    /// The model used when the client picked none: `model`, else the first of
    /// `models`.
    #[must_use]
    pub fn default_model(&self) -> Option<&str> {
        self.model()
            .or_else(|| self.models.first().map(String::as_str))
    }

    /// The suggested model ids without repeats, the default first.
    #[must_use]
    pub fn suggested_models(&self) -> Vec<&str> {
        let mut suggested: Vec<&str> = Vec::new();
        for model in self.model.iter().chain(&self.models) {
            if !suggested.contains(&model.as_str()) {
                suggested.push(model);
            }
        }
        suggested
    }

    /// Where the credential comes from, if the profile names one.
    #[must_use]
    pub fn credential(&self) -> Option<&CredentialSource> {
        self.credential.as_ref()
    }

    /// The browser sign-in this profile can run, if its `credential` names one.
    #[must_use]
    pub fn login_spec(&self) -> Option<&LoginSpec> {
        self.login.as_deref()
    }

    /// The custom headers, sorted by name. Only this profile's requests carry
    /// them.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// The preferred endpoint's protocol-specific settings.
    #[must_use]
    pub fn options(&self) -> &ProtocolOptions {
        &self.preferred().options
    }
}

/// The active grouping profile's connections and default connection.
#[derive(Clone, PartialEq, Eq)]
pub struct Providers {
    pub(crate) profiles: BTreeMap<String, Profile>,
    // Always names a profile, and is `Some` whenever there is one.
    pub(crate) default: Option<String>,
    pub(crate) profile_name: String,
}

impl fmt::Debug for Providers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Map keys and selected names are configuration values too; format only their structure.
        f.debug_struct("Providers")
            .field("profiles", &self.profiles.values().collect::<Vec<_>>())
            .field("has_default", &self.default.is_some())
            .finish()
    }
}

impl Default for Providers {
    fn default() -> Self {
        Self {
            profiles: BTreeMap::new(),
            default: None,
            profile_name: "default".into(),
        }
    }
}

impl Providers {
    /// The grouping profile that owns these connections and durable sessions.
    #[must_use]
    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }
    /// Read connections and resolve the active named grouping profile from `config`.
    ///
    /// No `providers` table is not an error: Bee then has no provider, which
    /// is the state before one is configured.
    ///
    /// # Errors
    ///
    /// A `bee::provider::*` error naming the key and file at fault: a wrong
    /// type, an unknown key, an unknown protocol, a missing `protocol`, an
    /// unusable value, or a default `provider` that is unknown or missing.
    pub fn from_config(config: &Config) -> Result<Self> {
        crate::profile::resolve(config, Self::connections(config)?)
    }

    pub(crate) fn connections(config: &Config) -> Result<BTreeMap<String, Profile>> {
        // Built before any profile is read, so a profile can name a preset or
        // a login flow the user defined (ADR-009).
        let catalog = Catalog::from_config(config)?;
        let reader = Reader {
            config,
            catalog: &catalog,
            fallback: "<unknown>".to_owned(),
        };

        let mut profiles = BTreeMap::new();
        match config.get("providers") {
            None => {}
            Some(Value::Table(table)) => {
                for (name, value) in table {
                    profiles.insert(name.clone(), reader.profile(name, value)?);
                }
            }
            Some(other) => return Err(reader.wrong_type("providers", "a table", other)),
        }

        Ok(profiles)
    }

    pub(crate) fn legacy(config: &Config, profiles: BTreeMap<String, Profile>) -> Result<Self> {
        let default = match config.get("provider") {
            None => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| Error::ProviderType {
                        key: "provider".into(),
                        path: config
                            .origin("provider")
                            .map_or_else(|| "<unknown>".into(), |p| p.display().to_string()),
                        expected: "a string",
                        found: value.type_str(),
                    })?
                    .to_owned(),
            ),
        };
        let available = || profiles.keys().cloned().collect::<Vec<_>>().join(", ");
        let default = match default {
            // Named by the user, so it must exist: falling back to another
            // profile would send their prompts somewhere they did not choose.
            Some(name) if profiles.contains_key(&name) => Some(name),
            Some(_) => {
                return Err(Error::ProviderUnknownDefault {
                    available: if profiles.is_empty() {
                        "none".to_owned()
                    } else {
                        available()
                    },
                });
            }
            None => match profiles.len() {
                0 => None,
                // A single profile is the obvious choice; demanding a
                // redundant `provider` would be ceremony.
                1 => profiles.keys().next().cloned(),
                // Guessing among several (say, the first alphabetically)
                // would make the default depend on a profile's name.
                _ => {
                    return Err(Error::ProviderNoDefault {
                        available: available(),
                    });
                }
            },
        };

        Ok(Self {
            profiles,
            default,
            profile_name: "default".into(),
        })
    }

    /// The profile called `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// The profile names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.profiles.keys().map(String::as_str)
    }

    /// The default profile's name: `None` only when there are no profiles.
    #[must_use]
    pub fn default_name(&self) -> Option<&str> {
        self.default.as_deref()
    }

    /// Whether no profile is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }
}

/// Whether `model` can be used as a model id.
///
/// Deliberately thin: ids are opaque, because private, fine-tuned and local
/// models follow no catalog. Only what could never be a name is refused.
///
/// # Errors
///
/// Fixed text saying what is wrong, never the id.
pub fn check_model_id(model: &str) -> std::result::Result<(), &'static str> {
    if model.trim().is_empty() {
        Err("a model id must not be empty")
    } else if model.trim() != model {
        Err("a model id must not start or end with whitespace")
    } else if model.chars().any(char::is_control) {
        Err("a model id must not contain control characters")
    } else {
        Ok(())
    }
}

/// Reads one configuration, naming the file of whatever goes wrong.
struct Reader<'a> {
    config: &'a Config,
    catalog: &'a Catalog,
    // Where a value no file set came from: the preset that supplied it.
    fallback: String,
}

impl Reader<'_> {
    /// The file that set `key`, for a diagnostic.
    fn path(&self, key: &str) -> String {
        self.config
            .origin(key)
            .map_or_else(|| self.fallback.clone(), |path| path.display().to_string())
    }

    /// Where the preset `name` is defined.
    fn preset_origin(&self, name: &str) -> String {
        match self.config.origin(&format!("presets.{name}")) {
            Some(path) => path.display().to_string(),
            None => format!("<built-in preset {name}>"),
        }
    }

    fn wrong_type(&self, key: &str, expected: &'static str, found: &Value) -> Error {
        Error::ProviderType {
            key: key.to_owned(),
            path: self.path(key),
            expected,
            found: found.type_str(),
        }
    }

    fn invalid(&self, key: &str, reason: &'static str) -> Error {
        Error::ProviderInvalid {
            key: key.to_owned(),
            path: self.path(key),
            reason,
        }
    }

    fn string<'v>(&self, key: &str, value: &'v Value) -> Result<&'v str> {
        value
            .as_str()
            .ok_or_else(|| self.wrong_type(key, "a string", value))
    }

    fn table<'v>(&self, key: &str, value: &'v Value) -> Result<&'v Table> {
        value
            .as_table()
            .ok_or_else(|| self.wrong_type(key, "a table", value))
    }

    /// Refuse any key of `table` that is not in `allowed`.
    fn only_keys(&self, key: &str, table: &Table, allowed: &[&str]) -> Result<()> {
        match table.keys().find(|k| !allowed.contains(&k.as_str())) {
            None => Ok(()),
            Some(unknown) => {
                let key = format!("{key}.{unknown}");
                Err(Error::ProviderUnknownKey {
                    path: self.path(&key),
                    key,
                    // A table that accepts nothing says so, instead of ending
                    // the sentence "this table accepts: ".
                    allowed: if allowed.is_empty() {
                        "no keys".to_owned()
                    } else {
                        allowed.join(", ")
                    },
                })
            }
        }
    }

    fn model(&self, key: &str, value: &Value) -> Result<String> {
        let model = self.string(key, value)?;
        check_model_id(model).map_err(|reason| self.invalid(key, reason))?;
        Ok(model.to_owned())
    }

    fn profile(&self, name: &str, value: &Value) -> Result<Profile> {
        let key = format!("providers.{name}");
        if name.is_empty() || name.chars().any(char::is_control) {
            return Err(self.invalid(
                &key,
                "a profile name must be non-empty and free of control characters",
            ));
        }
        let own = self.table(&key, value)?;
        self.only_keys(&key, own, PROFILE_KEYS)?;

        let Some(preset) = own.get("preset") else {
            return self.profile_from(name, &key, own, None);
        };
        let preset_key = format!("{key}.preset");
        let preset_name = self.string(&preset_key, preset)?;
        // Not echoed when unknown: the likeliest mistake is a pasted key.
        let Some(preset_table) = self.catalog.preset(preset_name) else {
            return Err(Error::ProviderUnknownPreset {
                path: self.path(&preset_key),
                key: preset_key,
                available: self.catalog.preset_names().join(", "),
            });
        };

        // Diagnostics about what the preset supplies name the preset, so the
        // user is not told "<unknown>" about a file they never wrote.
        let inner = Reader {
            config: self.config,
            catalog: self.catalog,
            fallback: self.preset_origin(preset_name),
        };
        inner.only_keys(&format!("presets.{preset_name}"), preset_table, PRESET_KEYS)?;
        let combined = overlay(preset_table, own);
        inner.profile_from(name, &key, &combined, Some(preset_name))
    }

    /// Read a profile from its final table: the preset, if any, already laid
    /// under the profile's own keys.
    fn profile_from(
        &self,
        name: &str,
        key: &str,
        table: &Table,
        preset: Option<&str>,
    ) -> Result<Profile> {
        let key = key.to_owned();
        let endpoints = self.endpoints(name, &key, table)?;
        let model_protocols = self.model_protocols(&key, table, &endpoints, preset)?;

        let model = table
            .get("model")
            .map(|value| self.model(&format!("{key}.model"), value))
            .transpose()?;

        let models = match table.get("models") {
            None => Vec::new(),
            Some(Value::Array(items)) => {
                let key = format!("{key}.models");
                let mut models = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    let item_key = format!("{key}[{index}]");
                    let model = self.string(&item_key, item)?;
                    // The index is not a configuration key, so the file is
                    // looked up under the array itself.
                    check_model_id(model).map_err(|reason| Error::ProviderInvalid {
                        key: item_key.clone(),
                        path: self.path(&key),
                        reason,
                    })?;
                    models.push(model.to_owned());
                }
                models
            }
            Some(other) => {
                return Err(self.wrong_type(&format!("{key}.models"), "an array", other));
            }
        };

        let (credential, login) = match table.get("credential") {
            None => (None, None),
            Some(value) => {
                let (credential, login) = self.credential(&format!("{key}.credential"), value)?;
                (Some(credential), login)
            }
        };

        let headers = match table.get("headers") {
            None => Vec::new(),
            Some(value) => self.headers(&format!("{key}.headers"), value)?,
        };

        let copilot = match table.get("auth") {
            None => false,
            Some(value) if self.string(&format!("{key}.auth"), value)? == "github" => true,
            Some(_) => {
                return Err(self.invalid(
                    &format!("{key}.auth"),
                    "requires a supported authentication integration",
                ));
            }
        };
        if login.as_ref().is_some_and(|flow| flow.is_copilot()) && !copilot {
            return Err(self.invalid(&key, "GitHub device login requires auth = github"));
        }
        if copilot
            && (credential.is_none()
                || login.as_ref().is_some_and(|flow| !flow.is_copilot())
                || endpoints.iter().any(|endpoint| {
                    crate::auth::copilot::validate_target(endpoint.base_url()).is_err()
                })
                || headers
                    .iter()
                    .any(|(name, _)| crate::auth::copilot::reserved_header(name)))
        {
            return Err(self.invalid(&key, "Copilot requires a credential, trusted Copilot endpoints, matching device login and no reserved headers"));
        }

        if login.as_ref().is_some_and(|flow| flow.is_chatgpt())
            && (endpoints.len() != 1
                || endpoints[0].protocol() != Protocol::OpenAiResponses
                || endpoints[0].base_url() != "https://api.openai.com/v1"
                || !headers.is_empty())
        {
            return Err(self.invalid(&key, "ChatGPT OAuth requires the public OpenAI Responses endpoint without custom headers"));
        }

        Ok(Profile {
            copilot,
            name: name.to_owned(),
            endpoints,
            model,
            models,
            model_protocols,
            credential,
            login: login.clone(),
            headers,
            timeout_ms: self.positive(table, &key, "timeout_ms", 120_000)?,
            max_output_tokens: self.positive(table, &key, "max_output_tokens", 4096)?,
            context_window: table
                .get("context_window")
                .map(|_| self.positive(table, &key, "context_window", 1))
                .transpose()?,
            model_context_windows: self.model_context_windows(&key, table, preset)?,
            catalog_provider: table
                .get("catalog_provider")
                .map(|value| self.model(&format!("{key}.catalog_provider"), value))
                .transpose()?,
            catalog_protocol_hints: self.boolean(table, &key, "catalog_protocol_hints")?,
            catalog_context_window: self.boolean(table, &key, "catalog_context_window")?,
            discover_models: self.boolean(table, &key, "discover_models")?,
            discovery: table.get("discovery").map(|value| {
                match self.string(&format!("{key}.discovery"), value)? {
                    "openrouter" => Ok(crate::models::Integration::OpenRouter),
                    "github" if copilot => Ok(crate::models::Integration::Copilot),
                    "chatgpt" if login.as_ref().is_some_and(|flow| flow.is_chatgpt()) => Ok(crate::models::Integration::ChatGpt),
                    _ => Err(self.invalid(&key, "discovery requires a supported integration and matching authentication")),
                }
            }).transpose()?,
            metadata: table
                .get("metadata")
                .map(|value| self.metadata(&format!("{key}.metadata"), value))
                .transpose()?
                .unwrap_or_default(),
            model_metadata: self.model_metadata(&key, table, preset)?,
        })
    }

    fn boolean(&self, table: &Table, key: &str, field: &str) -> Result<bool> {
        table
            .get(field)
            .map(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| self.wrong_type(&format!("{key}.{field}"), "a boolean", value))
            })
            .transpose()
            .map(|value| value.unwrap_or(false))
    }

    fn metadata(&self, key: &str, value: &Value) -> Result<crate::models::Metadata> {
        // Discard serde's value-bearing errors, including unknown field names.
        let metadata: crate::models::Metadata = value.clone().try_into().map_err(|_| {
            self.invalid(
                key,
                "must contain supported metadata fields with the documented types",
            )
        })?;
        if !metadata.valid() {
            return Err(self.invalid(
                key,
                "metadata must have positive limits and nonempty control-free names/modalities",
            ));
        }
        Ok(metadata)
    }

    fn model_metadata(
        &self,
        key: &str,
        table: &Table,
        preset: Option<&str>,
    ) -> Result<BTreeMap<String, crate::models::Metadata>> {
        let key = format!("{key}.model_metadata");
        let Some(value) = table.get("model_metadata") else {
            return Ok(BTreeMap::new());
        };
        let mut metadata = BTreeMap::new();
        for (id, value) in self.table(&key, value)? {
            let origin = self.config.origin(&format!("{key}.{id}")).or_else(|| {
                preset.and_then(|preset| {
                    self.config
                        .origin(&format!("presets.{preset}.model_metadata.{id}"))
                })
            });
            let path = origin.map_or_else(|| self.path(&key), |path| path.display().to_string());
            // IDs used as keys must not escape in errors, even in nested metadata.
            check_model_id(id).map_err(|reason| Error::ProviderInvalid {
                key: key.clone(),
                path: path.clone(),
                reason,
            })?;
            let entry = self.metadata(&key, value).map_err(|error| match error {
                Error::ProviderInvalid { key, reason, .. } => {
                    Error::ProviderInvalid { key, path, reason }
                }
                error => error,
            })?;
            metadata.insert(id.clone(), entry);
        }
        Ok(metadata)
    }

    fn model_context_windows(
        &self,
        key: &str,
        table: &Table,
        preset: Option<&str>,
    ) -> Result<BTreeMap<String, u32>> {
        let key = format!("{key}.model_context_windows");
        let Some(value) = table.get("model_context_windows") else {
            return Ok(BTreeMap::new());
        };
        let mut result = BTreeMap::new();
        for (model, value) in self.table(&key, value)? {
            // Model IDs are values even when represented as keys: never echo them.
            let origin = self.config.origin(&format!("{key}.{model}")).or_else(|| {
                preset.and_then(|preset| {
                    self.config
                        .origin(&format!("presets.{preset}.model_context_windows.{model}"))
                })
            });
            let path = origin.map_or_else(|| self.path(&key), |path| path.display().to_string());
            let invalid = |reason| Error::ProviderInvalid {
                key: key.clone(),
                path: path.clone(),
                reason,
            };
            check_model_id(model).map_err(invalid)?;
            let count = value
                .as_integer()
                .and_then(|count| u32::try_from(count).ok())
                .filter(|count| *count > 0)
                .ok_or_else(|| invalid("must be a positive 32-bit integer"))?;
            result.insert(model.clone(), count);
        }
        Ok(result)
    }

    fn model_protocols(
        &self,
        key: &str,
        table: &Table,
        endpoints: &[Endpoint],
        preset: Option<&str>,
    ) -> Result<BTreeMap<String, Protocol>> {
        let key = format!("{key}.model_protocols");
        let Some(value) = table.get("model_protocols") else {
            return Ok(BTreeMap::new());
        };
        let mappings = self.table(&key, value)?;
        let mut result = BTreeMap::new();
        for (model, value) in mappings {
            // IDs are values represented as TOML keys. Use them only for internal
            // provenance, never in diagnostics: a pasted credential must stay private.
            let origin = self.config.origin(&format!("{key}.{model}")).or_else(|| {
                preset.and_then(|preset| {
                    self.config
                        .origin(&format!("presets.{preset}.model_protocols.{model}"))
                })
            });
            let path = origin.map_or_else(|| self.path(&key), |path| path.display().to_string());
            let invalid = |reason| Error::ProviderInvalid {
                key: key.clone(),
                path: path.clone(),
                reason,
            };
            check_model_id(model).map_err(invalid)?;
            let name = value.as_str().ok_or_else(|| Error::ProviderType {
                key: key.clone(),
                path: path.clone(),
                expected: "a protocol name string",
                found: value.type_str(),
            })?;
            let protocol = Protocol::from_name(name).ok_or_else(|| invalid(
                "must name a supported protocol: openai-chat, openai-responses or anthropic-messages",
            ))?;
            if !endpoints
                .iter()
                .any(|endpoint| endpoint.protocol == protocol)
            {
                return Err(invalid(
                    "mapped protocol is not configured; add it under protocols or change the mapping",
                ));
            }
            result.insert(model.clone(), protocol);
        }
        Ok(result)
    }

    fn positive(&self, table: &Table, key: &str, field: &str, default: u32) -> Result<u32> {
        let Some(value) = table.get(field) else {
            return Ok(default);
        };
        let key = format!("{key}.{field}");
        let value = value
            .as_integer()
            .ok_or_else(|| self.wrong_type(&key, "a positive integer", value))?;
        u32::try_from(value)
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| self.invalid(&key, "must be a positive 32-bit integer"))
    }

    /// Read `protocols` and `prefer` into endpoints, most preferred first.
    fn endpoints(&self, name: &str, key: &str, table: &Table) -> Result<Vec<Endpoint>> {
        let protocols_key = format!("{key}.protocols");
        let protocols = match table.get("protocols") {
            None => {
                return Err(Error::ProviderMissingKey {
                    profile: name.to_owned(),
                    key: "protocols",
                    path: self.path(key),
                });
            }
            Some(value) => self.table(&protocols_key, value)?,
        };
        if protocols.is_empty() {
            return Err(self.invalid(
                &protocols_key,
                "name at least one protocol under `protocols`",
            ));
        }

        // Keyed by protocol so layers and presets merge per protocol. A name
        // that is not one is refused *without* being echoed, as a value would
        // be: the table is named instead.
        let mut endpoints: Vec<Endpoint> = Vec::with_capacity(protocols.len());
        for (protocol_name, value) in protocols {
            let protocol = Protocol::from_name(protocol_name).ok_or_else(|| {
                Error::ProviderUnknownProtocol {
                    path: self.path(&protocols_key),
                    key: protocols_key.clone(),
                    supported: Protocol::supported(),
                }
            })?;
            let endpoint_key = format!("{protocols_key}.{protocol_name}");
            let endpoint = self.table(&endpoint_key, value)?;
            self.only_keys(&endpoint_key, endpoint, ENDPOINT_KEYS)?;

            let base_url = match endpoint.get("base_url") {
                None => protocol.default_base_url().to_owned(),
                Some(value) => {
                    let url_key = format!("{endpoint_key}.base_url");
                    normalize_base_url(self.string(&url_key, value)?)
                        .map_err(|reason| self.invalid(&url_key, reason))?
                }
            };
            let options = match endpoint.get("options") {
                None => ProtocolOptions::defaults(protocol),
                Some(value) => self.options(&format!("{endpoint_key}.options"), protocol, value)?,
            };
            endpoints.push(Endpoint {
                protocol,
                base_url,
                options,
            });
        }

        // The preference. The table has no order of its own (it is sorted, and
        // layers merge into it), so the order is stated: guessing among several
        // would make the choice depend on how the names happen to sort.
        let prefer = match table.get("prefer") {
            None => None,
            Some(Value::Array(items)) => {
                let prefer_key = format!("{key}.prefer");
                let mut order: Vec<Protocol> = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    let item_key = format!("{prefer_key}[{index}]");
                    // The index is not a configuration key, so the file is
                    // looked up under the array itself.
                    let at = |reason: &'static str| Error::ProviderInvalid {
                        key: item_key.clone(),
                        path: self.path(&prefer_key),
                        reason,
                    };
                    let text = self.string(&item_key, item)?;
                    let protocol = Protocol::from_name(text).ok_or_else(|| {
                        Error::ProviderUnknownProtocol {
                            path: self.path(&prefer_key),
                            key: item_key.clone(),
                            supported: Protocol::supported(),
                        }
                    })?;
                    if !endpoints.iter().any(|e| e.protocol == protocol) {
                        return Err(at(
                            "`prefer` names a protocol that is not under `protocols`",
                        ));
                    }
                    if order.contains(&protocol) {
                        return Err(at("`prefer` names this protocol twice"));
                    }
                    order.push(protocol);
                }
                Some(order)
            }
            Some(other) => {
                return Err(self.wrong_type(&format!("{key}.prefer"), "an array", other));
            }
        };
        match prefer {
            None if endpoints.len() > 1 => Err(Error::ProviderNoPreference {
                profile: name.to_owned(),
                available: endpoints
                    .iter()
                    .map(|e| e.protocol.name())
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
            None => Ok(endpoints),
            Some(order) => {
                // Named protocols first, in the order given; any the list left
                // out follow in the fixed order of `Protocol::ALL`, so the
                // result never depends on the file.
                let rank = |protocol: Protocol| {
                    order
                        .iter()
                        .position(|p| *p == protocol)
                        .unwrap_or_else(|| {
                            order.len()
                                + Protocol::ALL
                                    .iter()
                                    .position(|p| *p == protocol)
                                    .unwrap_or(0)
                        })
                };
                endpoints.sort_by_key(|endpoint| rank(endpoint.protocol));
                Ok(endpoints)
            }
        }
    }

    fn credential(
        &self,
        key: &str,
        value: &Value,
    ) -> Result<(CredentialSource, Option<Arc<LoginSpec>>)> {
        let table = self.table(key, value)?;
        self.only_keys(key, table, &["env", "store", "login"])?;
        let login_key = format!("{key}.login");
        let login = table
            .get("login")
            .map(|value| self.string(&login_key, value))
            .transpose()?;
        match (table.get("env"), table.get("store")) {
            (None, None) => {
                Err(self.invalid(key, "name exactly one credential source: `env` or `store`"))
            }
            (Some(_), Some(_)) => Err(self.invalid(
                key,
                "name exactly one credential source: `env` or `store`, not both",
            )),
            (Some(env), None) => {
                // A sign-in stores what it obtains, so it only makes sense
                // with the store.
                if login.is_some() {
                    return Err(self.invalid(
                        &login_key,
                        "`login` signs in to the credential store, so it needs `store = true`, not `env`",
                    ));
                }
                let env_key = format!("{key}.env");
                let name = self.string(&env_key, env)?;
                // A variable *name*, so anything else is a mistake, and the
                // likeliest one is a pasted key. Refusing it without echoing it
                // keeps the key out of the diagnostic and out of the file's next
                // commit.
                let is_name = !name.is_empty()
                    && !name.starts_with(|c: char| c.is_ascii_digit())
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if !is_name {
                    return Err(self.invalid(
                        &env_key,
                        "`env` must be the name of an environment variable (letters, digits and \
                         underscores), not the credential itself",
                    ));
                }
                Ok((CredentialSource::Env(name.to_owned()), None))
            }
            (None, Some(store)) => {
                let store_key = format!("{key}.store");
                let Value::Boolean(enabled) = store else {
                    return Err(self.wrong_type(&store_key, "a boolean", store));
                };
                // `store = false` would read as "no stored credential", which
                // is what leaving `credential` out already means; accepting it
                // would give one meaning two spellings.
                if !enabled {
                    return Err(self.invalid(
                        &store_key,
                        "`store` can only be `true`; leave `credential` out to go without one",
                    ));
                }
                let spec = login
                    .map(|name| self.login_spec(&login_key, name).map(Arc::new))
                    .transpose()?;
                Ok((
                    CredentialSource::Store {
                        login: login.map(str::to_owned),
                    },
                    spec,
                ))
            }
        }
    }

    /// The login flow `name`, which `key` names.
    fn login_spec(&self, key: &str, name: &str) -> Result<LoginSpec> {
        // Not echoed when unknown: the likeliest mistake is a pasted token.
        let Some(spec) = self.catalog.login_spec(name) else {
            return Err(Error::ProviderUnknownLogin {
                path: self.path(key),
                key: key.to_owned(),
                available: self.catalog.login_names().join(", "),
            });
        };
        spec.map_err(|error| match error {
            SpecError::Invalid { key: rel, reason } => {
                let key = format!("login_flows.{name}.{rel}");
                self.invalid(&key, reason)
            }
            SpecError::Unknown { key: rel, allowed } => {
                let key = format!("login_flows.{name}.{rel}");
                Error::ProviderUnknownKey {
                    path: self.path(&key),
                    key,
                    allowed,
                }
            }
        })
    }

    fn headers(&self, key: &str, value: &Value) -> Result<Vec<(String, String)>> {
        let table = self.table(key, value)?;
        let mut headers: Vec<(String, String)> = Vec::with_capacity(table.len());
        for (name, value) in table {
            let header_key = format!("{key}.{name}");
            if !is_header_name(name) {
                return Err(self.invalid(
                    &header_key,
                    "a header name must be a non-empty HTTP token (letters, digits and \
                     ``!#$%&'*+-.^_`|~``)",
                ));
            }
            // HTTP header names are case-insensitive, and two spellings of one
            // would both be sent: which wins would depend on the server.
            if headers
                .iter()
                .any(|(seen, _)| seen.eq_ignore_ascii_case(name))
            {
                return Err(self.invalid(
                    &header_key,
                    "this header is set twice, differing only in case",
                ));
            }
            let text = self.string(&header_key, value)?;
            // A line break in a value would let it inject headers of its own.
            if text.chars().any(|c| c.is_control() && c != '\t') {
                return Err(self.invalid(
                    &header_key,
                    "a header value must not contain control characters or line breaks",
                ));
            }
            headers.push((name.clone(), text.to_owned()));
        }
        Ok(headers)
    }

    fn options(&self, key: &str, protocol: Protocol, value: &Value) -> Result<ProtocolOptions> {
        let table = self.table(key, value)?;
        self.only_keys(key, table, ProtocolOptions::allowed_keys(protocol))?;
        let mut options = ProtocolOptions::defaults(protocol);
        match &mut options {
            ProtocolOptions::OpenAiChat {
                max_tokens_field,
                cache,
            } => {
                *cache = self.openai_cache(key, table)?;
                if let Some(value) = table.get("max_tokens_field") {
                    let key = format!("{key}.max_tokens_field");
                    *max_tokens_field = match self.string(&key, value)? {
                        "max_tokens" => MaxTokensField::MaxTokens,
                        "max_completion_tokens" => MaxTokensField::MaxCompletionTokens,
                        _ => {
                            return Err(self
                                .invalid(&key, "must be `max_tokens` or `max_completion_tokens`"));
                        }
                    };
                }
            }
            ProtocolOptions::OpenAiResponses { cache } => {
                *cache = self.openai_cache(key, table)?;
            }
            ProtocolOptions::AnthropicMessages {
                version,
                max_tokens,
                cache,
            } => {
                *cache = self.anthropic_cache(key, table)?;
                if let Some(value) = table.get("version") {
                    let key = format!("{key}.version");
                    let text = self.string(&key, value)?;
                    if text.trim().is_empty() || text.chars().any(char::is_control) {
                        return Err(
                            self.invalid(&key, "must be non-empty text without control characters")
                        );
                    }
                    *version = Some(text.to_owned());
                }
                if let Some(value) = table.get("max_tokens") {
                    let key = format!("{key}.max_tokens");
                    let Value::Integer(number) = value else {
                        return Err(self.wrong_type(&key, "an integer", value));
                    };
                    *max_tokens = Some(
                        u32::try_from(*number)
                            .ok()
                            .filter(|number| *number > 0)
                            .ok_or_else(|| {
                                self.invalid(
                                    &key,
                                    "must be a positive integer that fits in 32 bits",
                                )
                            })?,
                    );
                }
            }
        }
        Ok(options)
    }
}

/// Lay a profile's own keys over its preset's.
///
/// Scalars and arrays (`model`, `models`) replace; `headers` and `options` merge
/// key by key, so a profile adds one header without restating the preset's.
/// `credential` is replaced **whole**: merging would let `{ env = ".." }` end up
/// beside the preset's `{ store = true }`, which is two sources, and the
/// profile's choice of one must win outright.
fn overlay(preset: &Table, own: &Table) -> Table {
    let mut combined = preset.clone();
    for (key, value) in own {
        match (key.as_str(), combined.get_mut(key), value) {
            ("preset", _, _) => {}
            ("credential", _, _) => {
                combined.insert(key.clone(), value.clone());
            }
            (_, Some(Value::Table(base)), Value::Table(over)) => deep_merge(base, over),
            _ => {
                combined.insert(key.clone(), value.clone());
            }
        }
    }
    combined
}

/// Whether `name` is an HTTP token, the only thing a header name can be.
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c))
}

/// Check a base URL and strip its trailing slashes.
///
/// Checked by hand because the only job is to catch typos before they become
/// a confusing connection error: no URL crate is needed for that, and none is
/// in the tree. The reasons are fixed text, so a URL that holds a secret is
/// never repeated.
pub(crate) fn normalize_base_url(url: &str) -> std::result::Result<String, &'static str> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("a base URL must not contain whitespace or control characters");
    }
    let lower = url.to_ascii_lowercase();
    let rest = if lower.starts_with("https://") {
        &url["https://".len()..]
    } else if lower.starts_with("http://") {
        &url["http://".len()..]
    } else {
        return Err("a base URL must start with `http://` or `https://`");
    };
    // Before anything else, because a query or fragment would be glued to
    // whatever path the backend appends.
    if rest.contains(['?', '#']) {
        return Err("a base URL must not contain a query or a fragment");
    }
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.contains('@') {
        return Err(
            "a base URL must not embed credentials; name a `credential` or a header instead",
        );
    }
    check_authority(authority)?;
    Ok(url.trim_end_matches('/').to_owned())
}

/// Check the `host[:port]` part of a URL.
fn check_authority(authority: &str) -> std::result::Result<(), &'static str> {
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        // An IPv6 literal: its own colons are not a port separator.
        let (host, after) = bracketed
            .split_once(']')
            .ok_or("a base URL has an unclosed `[` in its host")?;
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        {
            return Err("a base URL has an invalid IPv6 host");
        }
        (host, after.strip_prefix(':'))
    } else {
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        if host.is_empty() {
            return Err("a base URL must name a host");
        }
        if !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
        {
            return Err("a base URL has an invalid host name");
        }
        (host, port)
    };
    let _ = host;
    if let Some(port) = port
        && !(!port.is_empty()
            && port.chars().all(|c| c.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|port| port > 0))
    {
        return Err("a base URL has an invalid port");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_slash_is_trimmed_from_a_base_url() {
        assert_eq!(
            normalize_base_url("https://gw.example.com/v1/").as_deref(),
            Ok("https://gw.example.com/v1")
        );
    }

    #[test]
    fn local_hosts_with_ports_and_ipv6_literals_are_base_urls() {
        for url in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080",
            "http://[::1]:8080/v1",
            "HTTPS://Example.com",
        ] {
            assert!(normalize_base_url(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn malformed_base_urls_are_refused() {
        for url in [
            "",
            "example.com",
            "ftp://example.com",
            "https://",
            "https://:8080",
            "https://user:pw@example.com",
            "https://example.com?x=1",
            "https://example.com/#top",
            "https://exa mple.com",
            "https://example.com:99999",
            "https://example.com:",
            "https://[::1",
            "https://exa$mple.com",
        ] {
            assert!(normalize_base_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn a_model_id_is_opaque_apart_from_what_could_never_be_one() {
        for id in [
            "gpt-4o",
            "acme/ft-llama3:2026-01",
            "model with spaces inside",
            "ñ",
        ] {
            assert!(check_model_id(id).is_ok(), "{id}");
        }
        for id in ["", "  ", " padded", "line\nbreak"] {
            assert!(check_model_id(id).is_err(), "{id:?}");
        }
    }
}
