//! Internal authorization mechanisms with compiled-in trusted contracts.

use toml::{Table, Value};

use crate::provider::normalize_base_url;

/// The catalogue Bee ships.
const OPENROUTER_LOGIN: &str = r#"
kind = "pkce-key"
authorize_url = "https://openrouter.ai/auth"
exchange_url = "https://openrouter.ai/api/v1/auth/keys"
headless = true
help = "OpenRouter's authorization codes expire after 10 minutes and are single use."
[authorize]
callback = "callback_url"
challenge = "code_challenge"
challenge_method = "code_challenge_method"
state = "state"
extra = { key_label = "Bee" }
[exchange]
code = "code"
verifier = "code_verifier"
challenge_method = "code_challenge_method"
response_key = "key"
"#;

/// Configurable authorization-code/PKCE exchange returning an API key. ChatGPT
/// has a dedicated mechanism because registration, identity and renewal differ.
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
#[derive(Clone, PartialEq, Eq)]
pub struct LoginSpec {
    pub(crate) kind: LoginKind,
    pub(crate) name: String,
    pub(crate) authorize_url: String,
    pub(crate) exchange_url: String,
    pub(crate) headless: bool,
    pub(crate) help: Option<String>,
    pub(crate) authorize: AuthorizeNames,
    pub(crate) exchange: ExchangeNames,
}

impl std::fmt::Debug for LoginSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Test/injected flow specifications may contain sensitive URLs or parameters too.
        f.debug_struct("LoginSpec")
            .field("kind", &self.kind)
            .field("headless", &self.headless)
            .finish_non_exhaustive()
    }
}

/// Implemented authorization mechanisms, independent of credential sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginKind {
    PkceKey,
    ChatGpt,
    Copilot,
}

impl LoginSpec {
    /// Whether this is OpenAI's dedicated public-client OAuth mechanism.
    #[must_use]
    pub fn is_chatgpt(&self) -> bool {
        self.kind == LoginKind::ChatGpt
    }
    /// Whether this is GitHub's device authorization mechanism.
    #[must_use]
    pub fn is_copilot(&self) -> bool {
        self.kind == LoginKind::Copilot
    }
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
        if matches!(
            table.get("kind").and_then(Value::as_str),
            Some("chatgpt" | "github")
        ) {
            only_keys(table, &["kind"], "")?;
            let copilot = table["kind"].as_str() == Some("github");
            // This mechanism's trust boundary is fixed, not configurable URLs.
            return Ok(Self {
                kind: if copilot {
                    LoginKind::Copilot
                } else {
                    LoginKind::ChatGpt
                },
                name: name.into(),
                authorize_url: if copilot {
                    "https://github.com/login/device"
                } else {
                    "https://auth.openai.com/api/accounts/authorize"
                }
                .into(),
                exchange_url: if copilot {
                    "https://github.com/login/oauth/access_token"
                } else {
                    "https://auth.openai.com/api/accounts/oauth/token"
                }
                .into(),
                headless: false,
                help: None,
                authorize: AuthorizeNames {
                    callback: String::new(),
                    challenge: String::new(),
                    challenge_method: String::new(),
                    state: String::new(),
                    extra: Vec::new(),
                },
                exchange: ExchangeNames {
                    code: String::new(),
                    verifier: String::new(),
                    challenge_method: String::new(),
                    response_key: String::new(),
                },
            });
        }
        only_keys(table, FLOW_KEYS, "")?;
        match string(table, "kind", "kind")? {
            KIND_PKCE_KEY => {}
            _ => {
                return Err(SpecError::invalid(
                    "kind",
                    "`kind` must be `pkce-key`, `chatgpt` or `github`",
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
            kind: LoginKind::PkceKey,
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

/// Resolve a compiled-in acquisition contract. User configuration cannot choose auth URLs.
pub fn builtin(name: &str) -> Option<SpecResult<LoginSpec>> {
    let table = match name {
        "openrouter" => OPENROUTER_LOGIN.parse().expect("compiled-in PKCE contract"),
        "chatgpt" | "github" => Table::from_iter([("kind".into(), Value::String(name.into()))]),
        _ => return None,
    };
    Some(LoginSpec::from_table(name, &table))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_login_flow_is_a_valid_definition() {
        for name in ["openrouter", "chatgpt", "github"] {
            let spec = builtin(name).unwrap();
            assert!(spec.is_ok(), "{name}: {spec:?}");
        }
    }

    #[test]
    fn every_registered_acquisition_flow_exists() {
        for definition in crate::provider::registry::definitions() {
            let defaults = definition.defaults();
            let login = defaults
                .get("credential")
                .and_then(|credential| credential.get("login")?.as_str());
            if let Some(login) = login {
                assert!(builtin(login).is_some(), "{} -> {login}", definition.id);
            }
        }
    }

    #[test]
    fn the_shipped_openrouter_flow_matches_its_documentation() {
        let spec = builtin("openrouter").unwrap().unwrap();

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
    fn malformed_flow_fields_report_their_path_without_echoing_values() {
        for (path, value) in [
            ("kind", Value::Integer(7)),
            ("headless", Value::String("private-setting".into())),
            ("help", Value::String("private-setting\n".into())),
            ("help", Value::Integer(7)),
            ("authorize", Value::String("private-setting".into())),
            ("exchange", Value::Integer(7)),
            ("authorize.extra", Value::String("private-setting".into())),
            ("authorize.extra.label", Value::Integer(7)),
            (
                "authorize.extra.label",
                Value::String("private-setting\n".into()),
            ),
            (
                "exchange.verifier",
                Value::String("private-setting!".into()),
            ),
        ] {
            let mut table = flow(VALID);
            let mut target = &mut table;
            let parts: Vec<_> = path.split('.').collect();
            for part in &parts[..parts.len() - 1] {
                target = target
                    .entry((*part).to_owned())
                    .or_insert_with(|| Value::Table(Table::new()))
                    .as_table_mut()
                    .unwrap();
            }
            target.insert(parts.last().unwrap().to_string(), value);
            let error = LoginSpec::from_table("private-flow-name", &table).unwrap_err();
            assert!(matches!(&error, SpecError::Invalid { key, .. } if key == path));
            assert!(!format!("{error:?}").contains("private-setting"));
        }
        for section in ["authorize", "exchange"] {
            let mut table = flow(VALID);
            table.remove(section);
            assert!(
                matches!(LoginSpec::from_table("x", &table), Err(SpecError::Invalid { key, reason: "is required" }) if key == section)
            );
        }
    }

    #[test]
    fn trusted_native_flows_refuse_endpoint_overrides() {
        for kind in ["chatgpt", "github"] {
            let mut table = flow(&format!("kind='{kind}'"));
            table.insert(
                "authorize_url".into(),
                Value::String("https://private-setting.example".into()),
            );
            let error = LoginSpec::from_table("x", &table).unwrap_err();
            assert!(
                matches!(&error, SpecError::Unknown { key, allowed } if key == "authorize_url" && allowed == "kind")
            );
            assert!(!format!("{error:?}").contains("private-setting"));
        }
    }

    #[test]
    fn flow_debug_redacts_urls_help_names_and_extra_parameters() {
        let mut table = flow(VALID);
        table.insert("help".into(), Value::String("private-help".into()));
        table["authorize_url"] = Value::String("https://private-host.example/auth".into());
        table["authorize"].as_table_mut().unwrap().insert(
            "extra".into(),
            Value::Table(Table::from_iter([(
                "label".into(),
                Value::String("private-extra".into()),
            )])),
        );
        let spec = LoginSpec::from_table("private-name", &table).unwrap();
        assert_eq!(
            spec.authorize.extra,
            [("label".into(), "private-extra".into())]
        );
        let debug = format!("{spec:?}");
        assert!(debug.contains("PkceKey"));
        assert!(!debug.contains("private-"));
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
