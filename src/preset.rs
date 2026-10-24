//! Provider presets and login flows, as data and free of the ACPv2 SDK.
//!
//! The only things Bee's Rust must know about a provider are its *protocol*
//! (each is a backend) and the *kind* of its browser sign-in (each is a
//! mechanism). Everything else (URLs, parameter names, the field a credential
//! comes back in, a profile's defaults) is data, in `data/presets.toml` shipped
//! inside the binary and extended or overridden by the user's own `presets` and
//! `login_flows` tables. See ADR-009.
//!
//! Like `provider`, the validation is written by hand over the generic TOML
//! value, and every diagnostic names a key and a file, never a value.

use toml::{Table, Value};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::provider::normalize_base_url;

/// The catalogue Bee ships.
const BUILTIN: &str = include_str!("../data/presets.toml");

/// The one flow kind Bee implements: authorization code with PKCE, whose
/// exchange returns a credential directly. A flow with refresh tokens or a
/// device code is another kind, and another piece of Rust.
const KIND_PKCE_KEY: &str = "pkce-key";

const FLOW_KEYS: &[&str] = &[
    "kind",
    "authorize_url",
    "exchange_url",
    "headless",
    "help",
    "authorize",
    "exchange",
];
const AUTHORIZE_KEYS: &[&str] = &[
    "callback",
    "challenge",
    "challenge_method",
    "state",
    "extra",
];
const EXCHANGE_KEYS: &[&str] = &["code", "verifier", "challenge_method", "response_key"];

/// What is wrong with a login flow's definition.
///
/// Keys are relative to the flow (`exchange.response_key`) and reasons are fixed
/// text, so the error cannot repeat a value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpecError {
    /// A setting is missing or unusable.
    Invalid {
        /// The key, relative to the flow.
        key: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A key the flow does not accept.
    Unknown {
        /// The key, relative to the flow.
        key: String,
        /// The keys the table accepts, comma separated.
        allowed: String,
    },
}

impl SpecError {
    fn invalid(key: impl Into<String>, reason: &'static str) -> Self {
        Self::Invalid {
            key: key.into(),
            reason,
        }
    }
}

type SpecResult<T> = std::result::Result<T, SpecError>;

/// The query parameters of the authorization URL, by the provider's names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthorizeNames {
    pub callback: String,
    pub challenge: String,
    pub challenge_method: String,
    pub state: String,
    pub extra: Vec<(String, String)>,
}

/// The JSON fields of the exchange, by the provider's names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExchangeNames {
    pub code: String,
    pub verifier: String,
    pub challenge_method: String,
    pub response_key: String,
}

/// A browser sign-in, described: where to send the user, where to exchange the
/// code, and what the provider calls each field.
///
/// Built from a `login_flows.<name>` table. Running it is `login`'s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSpec {
    pub(crate) name: String,
    pub(crate) authorize_url: String,
    pub(crate) exchange_url: String,
    pub(crate) headless: bool,
    pub(crate) help: Option<String>,
    pub(crate) authorize: AuthorizeNames,
    pub(crate) exchange: ExchangeNames,
}

impl LoginSpec {
    /// The flow's name, as `login` refers to it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the provider can show the code instead of redirecting, so
    /// `--paste-code` works.
    #[must_use]
    pub fn headless(&self) -> bool {
        self.headless
    }

    /// The URL the user is sent to, before any parameter.
    #[must_use]
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// The URL the code is exchanged at.
    #[must_use]
    pub fn exchange_url(&self) -> &str {
        &self.exchange_url
    }

    /// Read the flow `name` from its table.
    ///
    /// # Errors
    ///
    /// A [`SpecError`] naming the key at fault.
    pub fn from_table(name: &str, table: &Table) -> SpecResult<Self> {
        only_keys(table, FLOW_KEYS, "")?;
        match string(table, "kind", "kind")? {
            KIND_PKCE_KEY => {}
            _ => {
                return Err(SpecError::invalid(
                    "kind",
                    "`kind` must be `pkce-key`, the only login flow Bee implements",
                ));
            }
        }
        let authorize_url = endpoint(
            "authorize_url",
            string(table, "authorize_url", "authorize_url")?,
        )?;
        let exchange_url = endpoint(
            "exchange_url",
            string(table, "exchange_url", "exchange_url")?,
        )?;
        let headless = match table.get("headless") {
            None => false,
            Some(Value::Boolean(headless)) => *headless,
            Some(_) => return Err(SpecError::invalid("headless", "must be a boolean")),
        };
        let help = match table.get("help") {
            None => None,
            Some(Value::String(help)) if !help.chars().any(char::is_control) => Some(help.clone()),
            Some(_) => {
                return Err(SpecError::invalid(
                    "help",
                    "must be text without control characters",
                ));
            }
        };

        let authorize = subtable(table, "authorize")?;
        only_keys(authorize, AUTHORIZE_KEYS, "authorize")?;
        let exchange = subtable(table, "exchange")?;
        only_keys(exchange, EXCHANGE_KEYS, "exchange")?;

        let name_of = |table: &Table, section: &str, key: &str| -> SpecResult<String> {
            let path = format!("{section}.{key}");
            let text = string(table, key, &path)?;
            if is_parameter_name(text) {
                Ok(text.to_owned())
            } else {
                Err(SpecError::invalid(
                    path,
                    "must be a parameter name: letters, digits, `_`, `-` and `.`",
                ))
            }
        };

        let extra = match authorize.get("extra") {
            None => Vec::new(),
            Some(Value::Table(extra)) => {
                let mut pairs = Vec::with_capacity(extra.len());
                for (key, value) in extra {
                    let path = format!("authorize.extra.{key}");
                    if !is_parameter_name(key) {
                        return Err(SpecError::invalid(
                            path,
                            "an extra parameter name must be letters, digits, `_`, `-` and `.`",
                        ));
                    }
                    match value {
                        Value::String(text) if !text.chars().any(char::is_control) => {
                            pairs.push((key.clone(), text.clone()));
                        }
                        _ => {
                            return Err(SpecError::invalid(
                                path,
                                "an extra parameter must be text without control characters",
                            ));
                        }
                    }
                }
                pairs
            }
            Some(_) => return Err(SpecError::invalid("authorize.extra", "must be a table")),
        };

        Ok(Self {
            name: name.to_owned(),
            authorize_url,
            exchange_url,
            headless,
            help,
            authorize: AuthorizeNames {
                callback: name_of(authorize, "authorize", "callback")?,
                challenge: name_of(authorize, "authorize", "challenge")?,
                challenge_method: name_of(authorize, "authorize", "challenge_method")?,
                state: name_of(authorize, "authorize", "state")?,
                extra,
            },
            exchange: ExchangeNames {
                code: name_of(exchange, "exchange", "code")?,
                verifier: name_of(exchange, "exchange", "verifier")?,
                challenge_method: name_of(exchange, "exchange", "challenge_method")?,
                response_key: name_of(exchange, "exchange", "response_key")?,
            },
        })
    }
}

/// A required string setting.
fn string<'a>(table: &'a Table, key: &str, path: &str) -> SpecResult<&'a str> {
    match table.get(key) {
        Some(Value::String(text)) => Ok(text),
        Some(_) => Err(SpecError::invalid(path, "must be a string")),
        None => Err(SpecError::invalid(path, "is required")),
    }
}

/// A required nested table.
fn subtable<'a>(table: &'a Table, key: &str) -> SpecResult<&'a Table> {
    match table.get(key) {
        Some(Value::Table(nested)) => Ok(nested),
        Some(_) => Err(SpecError::invalid(key, "must be a table")),
        None => Err(SpecError::invalid(key, "is required")),
    }
}

/// Refuse a key `allowed` does not list.
fn only_keys(table: &Table, allowed: &[&str], prefix: &str) -> SpecResult<()> {
    match table.keys().find(|key| !allowed.contains(&key.as_str())) {
        None => Ok(()),
        Some(unknown) => Err(SpecError::Unknown {
            key: if prefix.is_empty() {
                unknown.clone()
            } else {
                format!("{prefix}.{unknown}")
            },
            allowed: allowed.join(", "),
        }),
    }
}

/// An endpoint that receives the authorization code, the verifier or the key.
///
/// HTTPS only, and the base-URL rules on top (no credentials in the URL, no
/// query, no fragment): a code sent in the clear, or to a URL carrying its own
/// parameters, is the attack this flow's PKCE exists to blunt.
fn endpoint(key: &str, url: &str) -> SpecResult<String> {
    if !url.to_ascii_lowercase().starts_with("https://") {
        return Err(SpecError::invalid(key, "must be an `https://` URL"));
    }
    normalize_base_url(url).map_err(|reason| SpecError::invalid(key, reason))
}

/// Whether `text` can be a query parameter or JSON field name.
fn is_parameter_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Merge `over` into `base`: tables merge key by key, anything else is replaced.
pub(crate) fn deep_merge(base: &mut Table, over: &Table) {
    for (key, value) in over {
        match (base.get_mut(key), value) {
            (Some(Value::Table(base)), Value::Table(over)) => deep_merge(base, over),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Every preset and login flow available: the shipped ones, then the user's.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    presets: Table,
    login_flows: Table,
}

impl Catalog {
    /// What ships in the binary, and nothing else.
    ///
    /// # Panics
    ///
    /// Never in a released build: a unit test parses the embedded file.
    #[must_use]
    pub fn builtin() -> Self {
        let table: Table = BUILTIN
            .parse()
            .expect("the embedded `data/presets.toml` is valid TOML");
        let section = |name: &str| match table.get(name) {
            Some(Value::Table(section)) => section.clone(),
            _ => Table::new(),
        };
        Self {
            presets: section("presets"),
            login_flows: section("login_flows"),
        }
    }

    /// The shipped catalogue with the `presets` and `login_flows` of `config`
    /// merged over it, by name.
    ///
    /// # Errors
    ///
    /// [`Error::ProviderType`] for a section or entry that is not a table, and
    /// [`Error::ProviderUntrustedLayer`] for a login flow set in the project
    /// file.
    pub fn from_config(config: &Config) -> Result<Self> {
        let mut catalog = Self::builtin();

        // A login flow names the URLs that receive the authorization code and
        // the PKCE verifier. The project file can come from a repository the
        // user merely cloned, so it may not choose them; the user's own files
        // can. A preset is only shorthand for a profile, and a project could
        // already write that profile out.
        if let Some(key) = config.set_by_project("login_flows") {
            return Err(Error::ProviderUntrustedLayer {
                path: config
                    .origin(&key)
                    .map_or_else(|| "<unknown>".to_owned(), |path| path.display().to_string()),
                key,
            });
        }

        for (section, target) in [
            ("presets", &mut catalog.presets),
            ("login_flows", &mut catalog.login_flows),
        ] {
            let path_of = |key: &str| {
                config
                    .origin(key)
                    .map_or_else(|| "<unknown>".to_owned(), |path| path.display().to_string())
            };
            let wrong_type = |key: String, expected, found: &Value| Error::ProviderType {
                path: path_of(&key),
                key,
                expected,
                found: found.type_str(),
            };
            match config.get(section) {
                None => {}
                Some(Value::Table(user)) => {
                    for (name, value) in user {
                        let Value::Table(entry) = value else {
                            return Err(wrong_type(format!("{section}.{name}"), "a table", value));
                        };
                        match target.get_mut(name) {
                            Some(Value::Table(existing)) => deep_merge(existing, entry),
                            _ => {
                                target.insert(name.clone(), value.clone());
                            }
                        }
                    }
                }
                Some(other) => return Err(wrong_type(section.to_owned(), "a table", other)),
            }
        }
        Ok(catalog)
    }

    /// The preset `name`, as the table a profile overlays.
    pub(crate) fn preset(&self, name: &str) -> Option<&Table> {
        self.presets.get(name)?.as_table()
    }

    /// The preset names, sorted.
    #[must_use]
    pub fn preset_names(&self) -> Vec<&str> {
        self.presets.keys().map(String::as_str).collect()
    }

    /// The login flow names, sorted.
    #[must_use]
    pub fn login_names(&self) -> Vec<&str> {
        self.login_flows.keys().map(String::as_str).collect()
    }

    /// The login flow `name`. `None` if there is no such flow; `Some(Err(..))`
    /// if there is and its definition is unusable.
    #[must_use]
    pub fn login_spec(&self, name: &str) -> Option<SpecResult<LoginSpec>> {
        let table = self.login_flows.get(name)?.as_table()?;
        Some(LoginSpec::from_table(name, table))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_login_flow_is_a_valid_definition() {
        let catalog = Catalog::builtin();

        assert!(!catalog.login_names().is_empty());
        for name in catalog.login_names() {
            let spec = catalog.login_spec(name).unwrap();
            assert!(spec.is_ok(), "{name}: {spec:?}");
        }
    }

    #[test]
    fn every_shipped_preset_that_names_a_login_flow_names_one_that_exists() {
        let catalog = Catalog::builtin();

        for name in catalog.preset_names() {
            let login = catalog
                .preset(name)
                .and_then(|preset| preset.get("credential")?.get("login")?.as_str());
            if let Some(login) = login {
                assert!(catalog.login_names().contains(&login), "{name} -> {login}");
            }
        }
    }

    #[test]
    fn the_shipped_openrouter_flow_matches_its_documentation() {
        let spec = Catalog::builtin()
            .login_spec("openrouter")
            .unwrap()
            .unwrap();

        assert_eq!(spec.authorize_url(), "https://openrouter.ai/auth");
        assert_eq!(
            spec.exchange_url(),
            "https://openrouter.ai/api/v1/auth/keys"
        );
        assert!(spec.headless());
        assert_eq!(spec.authorize.callback, "callback_url");
        assert_eq!(spec.exchange.response_key, "key");
    }

    fn flow(text: &str) -> Table {
        text.parse().unwrap()
    }

    const VALID: &str = r#"
kind = "pkce-key"
authorize_url = "https://id.example/authorize"
exchange_url = "https://id.example/token"
[authorize]
callback = "redirect"
challenge = "challenge"
challenge_method = "method"
state = "state"
[exchange]
code = "code"
verifier = "verifier"
challenge_method = "method"
response_key = "api_key"
"#;

    #[test]
    fn a_minimal_flow_is_valid_and_not_headless_by_default() {
        let spec = LoginSpec::from_table("x", &flow(VALID)).unwrap();

        assert!(!spec.headless());
        assert!(spec.authorize.extra.is_empty());
    }

    #[test]
    fn a_flow_is_refused_for_each_of_these_mistakes() {
        for (name, text) in [
            ("kind", VALID.replace("pkce-key", "device-code")),
            (
                "http",
                VALID.replace("https://id.example/token", "http://id.example/token"),
            ),
            ("query", VALID.replace("/authorize\"", "/authorize?x=1\"")),
            (
                "userinfo",
                VALID.replace("id.example/token", "u:p@id.example/token"),
            ),
            ("parameter", VALID.replace("\"redirect\"", "\"re direct\"")),
            ("missing", VALID.replace("response_key = \"api_key\"", "")),
            ("unknown", format!("{VALID}\nsurprise = 1")),
        ] {
            assert!(
                LoginSpec::from_table("x", &flow(&text)).is_err(),
                "{name} should be refused"
            );
        }
    }
}
