//! Provider profiles, free of the ACPv2 SDK.
//!
//! A *profile* is a named connection: which protocol to speak, where, with
//! which headers, and which model to ask for by default. The protocol is a
//! property of the profile, never inferred from its name, and nothing here
//! knows a commercial provider: any name and any model id is accepted. See
//! ADR-007.
//!
//! Profiles are read from the merged [`Config`] (ADR-006) under `provider`
//! (the default profile) and `providers.<name>`. Nothing is sent anywhere yet:
//! this module only describes endpoints. Credentials are named, not resolved
//! (#12).
//!
//! The validation is written by hand over the generic TOML value instead of
//! derived with serde: serde's error text quotes the offending value, and
//! configuration files are where users keep API keys. Every diagnostic here
//! names a key and a file, never a value.

use std::collections::BTreeMap;
use std::fmt;

use toml::{Table, Value};

use crate::config::Config;
use crate::error::{Error, Result};

/// The keys a profile table accepts.
const PROFILE_KEYS: &[&str] = &[
    "protocol",
    "base_url",
    "model",
    "models",
    "credential",
    "headers",
    "options",
];

/// How Bee talks to an endpoint.
///
/// A closed set on purpose: each variant is code that exists. New endpoints
/// need no variant, only a profile that picks one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Protocol {
    /// OpenAI Chat Completions, and the many servers that copy it.
    OpenAiChat,
    /// Anthropic Messages.
    AnthropicMessages,
}

impl Protocol {
    /// Every protocol, in the order diagnostics list them.
    pub const ALL: [Self; 2] = [Self::OpenAiChat, Self::AnthropicMessages];

    /// The name used in configuration.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
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
            Self::OpenAiChat => "https://api.openai.com/v1",
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
/// Only a reference: resolving it, and any secret handling, is #12's. Adding
/// a source kind later is a new variant and a new accepted key.
// `Debug` is safe to derive: the payload is a variable *name*, which validation
// guarantees is not a pasted key.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialSource {
    /// The environment variable with this name.
    Env(String),
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolOptions {
    /// For [`Protocol::OpenAiChat`].
    OpenAiChat {
        /// Which field carries the output limit.
        max_tokens_field: MaxTokensField,
    },
    /// For [`Protocol::AnthropicMessages`].
    AnthropicMessages {
        /// The `anthropic-version` header, if not the backend's own.
        version: Option<String>,
        /// The output limit, if not the backend's own.
        max_tokens: Option<u32>,
    },
}

impl ProtocolOptions {
    fn defaults(protocol: Protocol) -> Self {
        match protocol {
            Protocol::OpenAiChat => Self::OpenAiChat {
                max_tokens_field: MaxTokensField::default(),
            },
            Protocol::AnthropicMessages => Self::AnthropicMessages {
                version: None,
                max_tokens: None,
            },
        }
    }

    fn allowed_keys(protocol: Protocol) -> &'static [&'static str] {
        match protocol {
            Protocol::OpenAiChat => &["max_tokens_field"],
            Protocol::AnthropicMessages => &["version", "max_tokens"],
        }
    }
}

/// One named connection profile.
#[derive(Clone, PartialEq, Eq)]
pub struct Profile {
    name: String,
    protocol: Protocol,
    base_url: String,
    model: Option<String>,
    models: Vec<String>,
    credential: Option<CredentialSource>,
    headers: Vec<(String, String)>,
    options: ProtocolOptions,
}

// Written by hand: header values are routinely secrets (`Authorization`), and
// a derived `Debug` would print them into assertion failures and logs. Names
// and the credential's *kind* are enough to debug a configuration.
impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("name", &self.name)
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("models", &self.models)
            .field(
                "credential",
                &self
                    .credential
                    .as_ref()
                    .map(|CredentialSource::Env(_)| "env"),
            )
            .field(
                "header_names",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("options", &self.options)
            .finish()
    }
}

impl Profile {
    /// The profile's name, as written in configuration.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How to talk to the endpoint.
    #[must_use]
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// The endpoint's base URL: the profile's own, else the protocol's default.
    /// Never ends with a slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
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

    /// The custom headers, sorted by name. Only this profile's requests carry
    /// them.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// The protocol-specific settings.
    #[must_use]
    pub fn options(&self) -> &ProtocolOptions {
        &self.options
    }
}

/// Every configured profile, and which one is the default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Providers {
    profiles: BTreeMap<String, Profile>,
    // Always names a profile, and is `Some` whenever there is one.
    default: Option<String>,
}

impl Providers {
    /// Read and validate the profiles from `config`.
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
        let reader = Reader { config };

        let default = match config.get("provider") {
            None => None,
            Some(value) => Some(reader.string("provider", value)?.to_owned()),
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

        Ok(Self { profiles, default })
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
}

impl Reader<'_> {
    /// The file that set `key`, for a diagnostic.
    fn path(&self, key: &str) -> String {
        self.config
            .origin(key)
            .map_or_else(|| "<unknown>".to_owned(), |path| path.display().to_string())
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
                    allowed: allowed.join(", "),
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
        let table = self.table(&key, value)?;
        self.only_keys(&key, table, PROFILE_KEYS)?;

        let protocol_key = format!("{key}.protocol");
        let protocol = match table.get("protocol") {
            None => {
                return Err(Error::ProviderMissingKey {
                    profile: name.to_owned(),
                    key: "protocol",
                    path: self.path(&key),
                });
            }
            Some(value) => {
                let text = self.string(&protocol_key, value)?;
                Protocol::from_name(text).ok_or_else(|| Error::ProviderUnknownProtocol {
                    path: self.path(&protocol_key),
                    key: protocol_key.clone(),
                    supported: Protocol::supported(),
                })?
            }
        };

        let base_url = match table.get("base_url") {
            None => protocol.default_base_url().to_owned(),
            Some(value) => {
                let key = format!("{key}.base_url");
                normalize_base_url(self.string(&key, value)?)
                    .map_err(|reason| self.invalid(&key, reason))?
            }
        };

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

        let credential = table
            .get("credential")
            .map(|value| self.credential(&format!("{key}.credential"), value))
            .transpose()?;

        let headers = match table.get("headers") {
            None => Vec::new(),
            Some(value) => self.headers(&format!("{key}.headers"), value)?,
        };

        let options = match table.get("options") {
            None => ProtocolOptions::defaults(protocol),
            Some(value) => self.options(&format!("{key}.options"), protocol, value)?,
        };

        Ok(Profile {
            name: name.to_owned(),
            protocol,
            base_url,
            model,
            models,
            credential,
            headers,
            options,
        })
    }

    fn credential(&self, key: &str, value: &Value) -> Result<CredentialSource> {
        let table = self.table(key, value)?;
        self.only_keys(key, table, &["env"])?;
        let Some(env) = table.get("env") else {
            return Err(self.invalid(key, "name exactly one credential source: `env`"));
        };
        let env_key = format!("{key}.env");
        let name = self.string(&env_key, env)?;
        // A variable *name*, so anything else is a mistake, and the likeliest
        // one is a pasted key. Refusing it without echoing it keeps the key out
        // of the diagnostic and out of the file's next commit.
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
        Ok(CredentialSource::Env(name.to_owned()))
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
            ProtocolOptions::OpenAiChat { max_tokens_field } => {
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
            ProtocolOptions::AnthropicMessages {
                version,
                max_tokens,
            } => {
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
fn normalize_base_url(url: &str) -> std::result::Result<String, &'static str> {
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
