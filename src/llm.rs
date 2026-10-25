//! SDK-free inference types and protocol dispatch.

pub mod anthropic;
pub mod chat;
pub mod responses;

use std::fmt;

use serde_json::Value;

use crate::auth::Secret;
use crate::provider::{Profile, Protocol};

/// A normalized, safe-to-display inference failure. Never stores wire values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    /// Applicable guidance could not be loaded or retained.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Instructions(#[from] crate::instructions::Diagnostic),
    /// The credential was rejected.
    #[error(
        "the model endpoint rejected authentication; check the configured credential or run `bee auth login`"
    )]
    #[diagnostic(code(bee::llm::authentication))]
    Authentication,
    /// Safe, allowlisted ChatGPT-plan recovery guidance.
    #[error("ChatGPT plan request failed: {0}")]
    #[diagnostic(code(bee::llm::chatgpt))]
    ChatGpt(&'static str),
    /// The endpoint is throttling requests.
    #[error("the model endpoint rate limited the request; wait before trying again")]
    #[diagnostic(code(bee::llm::rate_limit))]
    RateLimit,
    /// The whole request deadline expired.
    #[error("the model request timed out; check connectivity or increase `timeout_ms`")]
    #[diagnostic(code(bee::llm::timeout))]
    Timeout,
    /// No usable HTTP exchange completed.
    #[error("the model endpoint could not be reached; check its base URL and network connection")]
    #[diagnostic(code(bee::llm::transport))]
    Transport,
    /// A non-success HTTP status, without the possibly sensitive response body.
    #[error("the model endpoint returned HTTP {0}; check the endpoint, model and request settings")]
    #[diagnostic(code(bee::llm::http))]
    Http(u16),
    /// Required wire structure or lifecycle was invalid.
    #[error(
        "the model endpoint returned an invalid or incomplete response; check protocol compatibility"
    )]
    #[diagnostic(code(bee::llm::malformed))]
    Malformed,
    /// The server reported failure inside a successful HTTP response.
    #[error("the model endpoint reported a failed response; check the model and provider settings")]
    #[diagnostic(code(bee::llm::provider))]
    Provider,
    /// A deliberately unsupported capability.
    #[error(
        "unsupported model capability: {0}; use a supported text/function-calling configuration"
    )]
    #[diagnostic(code(bee::llm::unsupported))]
    Unsupported(&'static str),
    /// Invalid request headers, without their names or contents.
    #[error(
        "model request headers are invalid or conflict with protocol headers; fix the profile's headers"
    )]
    #[diagnostic(code(bee::llm::headers))]
    Headers,
    /// A finite execution or buffer budget was exceeded.
    #[error("the model turn exceeded its execution or response-size limit; shorten the request")]
    #[diagnostic(code(bee::llm::limit))]
    Limit,
    /// Input and reserved output cannot fit the model's context window.
    #[error(
        "the model context window was exceeded; shorten the prompt, start a new session, or correct context_window and max_output_tokens"
    )]
    #[diagnostic(code(bee::llm::context_limit))]
    ContextLimit,
    /// The executor has no matching tool.
    #[error("the model requested an unavailable tool; configure a matching tool executor")]
    #[diagnostic(code(bee::llm::tool))]
    Tool,
    /// An injected executor's cancellation signal stopped the turn.
    #[error("tool execution was cancelled; start a new turn to continue")]
    #[diagnostic(code(bee::llm::cancelled))]
    Cancelled,
}

/// The result of a single model step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// A final answer.
    End,
    /// Complete calls are ready to execute.
    Tools,
    /// The output limit was reached.
    MaxTokens,
    /// The model declined the request.
    Refusal,
}

/// Optional counters: absent is different from zero.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Usage {
    /// Input tokens, when supplied.
    pub input: Option<u64>,
    /// Output tokens, when supplied.
    pub output: Option<u64>,
}

/// A function the caller is prepared to execute.
#[derive(Clone)]
pub struct Tool {
    /// Model-visible function name.
    pub name: String,
    /// Description for the model.
    pub description: String,
    /// JSON Schema for the function input.
    pub parameters: Value,
}

/// A fully assembled function call, never an argument delta.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Call {
    /// Correlates the model call with its result.
    pub id: String,
    /// Name of a declared tool.
    pub name: String,
    /// Parsed object arguments.
    pub arguments: Value,
}

/// Native continuation is kept separate from portable text.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Native {
    /// The only protocol permitted to replay this value.
    pub protocol: Protocol,
    /// Ordered native output items or content blocks.
    pub items: Vec<Value>,
}

/// A conversation entry. Contents deliberately have no derived Debug.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Message {
    /// User text, including explicitly described resource links.
    User(String),
    /// A complete model step.
    Assistant {
        /// Text also used for portable history.
        text: String,
        /// Complete calls, in model order.
        calls: Vec<Call>,
        /// Protocol-owned continuation items.
        native: Option<Native>,
    },
    /// One result per call.
    Tool {
        /// Original call ID.
        id: String,
        /// Textual or JSON-encoded result.
        output: String,
        /// Whether the tool reported failure.
        is_error: bool,
    },
}

// Prompts, schemas and tool results may contain keys too: never print them in
// an assertion, transport trace or error chain.
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::User(_) => "User([redacted])",
            Self::Assistant { .. } => "Assistant([redacted])",
            Self::Tool { .. } => "Tool([redacted])",
        })
    }
}

/// Completed output returned after terminal validation.
#[derive(Debug, Clone)]
pub struct Step {
    /// Assistant text, calls and continuation state.
    pub message: Message,
    /// Why generation stopped.
    pub stop: Stop,
    /// Counters for this step.
    pub usage: Usage,
}

/// A selected endpoint and its reusable async transport.
#[derive(Clone)]
pub struct Model {
    chatgpt_session: Option<crate::auth::chatgpt::Session>,
    instructions: String,
    profile: Profile,
    model: String,
    credential: Option<Secret>,
    http: crate::http::InferenceHttp,
    selection: crate::models::Selection,
}

impl Model {
    #[cfg(test)]
    pub(crate) fn with_test_http(mut self, http: crate::http::InferenceHttp) -> Self {
        self.http = http;
        self
    }
    /// Construct a model without a vendor registry or catalog lookup.
    pub fn new(profile: Profile, model: String, credential: Option<Secret>) -> Result<Self, Error> {
        let selection = crate::models::Snapshot::default().resolve(&profile, &model);
        Ok(Self {
            chatgpt_session: None,
            instructions: String::new(),
            http: crate::http::InferenceHttp::new()?,
            profile,
            model,
            credential,
            selection,
        })
    }

    /// Produce one model step, streaming text to `text` as it arrives.
    pub async fn step(
        &self,
        messages: &[Message],
        tools: &[Tool],
        text: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Step, Error> {
        self.context(messages, tools)?.check()?;
        if self.chatgpt_session.is_some() && !self.is_chatgpt() {
            return Err(Error::ChatGpt(
                "renewable sessions require a validated ChatGPT profile",
            ));
        }
        if self.is_chatgpt() && self.chatgpt_session.is_none() {
            return Err(Error::ChatGpt(
                "a renewable ChatGPT session is required; run `bee auth login <profile>`",
            ));
        }
        let mut refreshed;
        let model = if let Some(session) = &self.chatgpt_session {
            refreshed = self.clone();
            refreshed.credential = Some(session.credential().await.map_err(|_| {
                Error::ChatGpt(
                    "cannot renew authentication; retry later or run `bee auth login <profile>`",
                )
            })?);
            &refreshed
        } else {
            self
        };
        match model.endpoint().protocol() {
            Protocol::OpenAiChat => chat::run(model, messages, tools, text).await,
            Protocol::OpenAiResponses => responses::run(model, messages, tools, text).await,
            Protocol::AnthropicMessages => anthropic::run(model, messages, tools, text).await,
        }
    }
}

impl Model {
    /// Use renewable ChatGPT credentials for each step of a long-running turn.
    pub fn with_chatgpt_session(mut self, session: crate::auth::chatgpt::Session) -> Self {
        self.chatgpt_session = Some(session);
        self
    }

    pub(crate) fn is_chatgpt(&self) -> bool {
        self.profile
            .login_spec()
            .is_some_and(crate::preset::LoginSpec::is_chatgpt)
    }
    /// Use an already-resolved immutable route and budget without any catalog I/O.
    pub(crate) fn with_selection(mut self, selection: crate::models::Selection) -> Self {
        self.selection = selection;
        self
    }
    /// Additional persistent instructions, kept outside evictable user-led turns.
    pub fn with_instructions(mut self, instructions: String) -> Self {
        self.instructions = instructions;
        self
    }

    /// Estimate the exact selected protocol representation without exposing content.
    pub fn context(
        &self,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<crate::context::Report, Error> {
        let body = match self.endpoint().protocol() {
            Protocol::OpenAiChat => chat::request(self, messages, tools)?,
            Protocol::OpenAiResponses => responses::request(self, messages, tools)?,
            Protocol::AnthropicMessages => anthropic::request(self, messages, tools)?,
        };
        let mut report = crate::context::estimate(&body, self.selection.window);
        // ChatGPT forbids the wire generation cap, but still needs local headroom.
        if self.is_chatgpt() {
            report.reserved_output = u64::from(self.profile.max_output_tokens());
        }
        report.window_fact = self.selection.window_fact.clone();
        Ok(report)
    }

    fn endpoint(&self) -> &crate::provider::Endpoint {
        self.profile
            .endpoint(self.selection.protocol)
            .expect("resolved configured protocol")
    }

    pub(crate) fn headers(
        &self,
        version: Option<&str>,
    ) -> Result<reqwest::header::HeaderMap, Error> {
        request_headers(
            &self.profile,
            self.endpoint().protocol(),
            self.credential.as_ref(),
            version,
            true,
        )
    }
}

/// Shared auth rules prevent endpoint discovery from silently presenting a key differently.
pub(crate) fn request_headers(
    profile: &Profile,
    protocol: Protocol,
    credential: Option<&Secret>,
    version: Option<&str>,
    streaming: bool,
) -> Result<reqwest::header::HeaderMap, Error> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "accept",
        HeaderValue::from_static(if streaming {
            "text/event-stream, application/json"
        } else {
            "application/json"
        }),
    );
    if let Some(version) = version {
        headers.insert(
            "anthropic-version",
            HeaderValue::from_str(version).map_err(|_| Error::Headers)?,
        );
    }
    if let Some(secret) = credential {
        let (name, value) = crate::auth::header_for(protocol, secret);
        let mut value = HeaderValue::from_str(value.expose()).map_err(|_| Error::Headers)?;
        value.set_sensitive(true);
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Headers)?,
            value,
        );
    }
    for (name, value) in profile.headers() {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Headers)?;
        if headers.contains_key(&name)
            || matches!(
                name.as_str(),
                "host" | "content-length" | "transfer-encoding"
            )
        {
            return Err(Error::Headers);
        }
        let mut value = HeaderValue::from_str(value).map_err(|_| Error::Headers)?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    Ok(headers)
}

/// Inspect only recognized classifications; upstream text never escapes this seam.
pub(crate) fn provider_error(protocol: Protocol, error: &Value) -> Error {
    match error["code"].as_str() {
        Some("subscription_sharing_user_not_eligible") => {
            return Error::ChatGpt(
                "account/workspace is ineligible; check ChatGPT plan and workspace policy",
            );
        }
        Some("subscription_sharing_usage_limit_exceeded") => {
            return Error::ChatGpt(
                "usage limit reached; manage usage at https://chatgpt.com/settings/usage",
            );
        }
        Some(
            "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable",
        ) => return Error::ChatGpt("temporarily unavailable; retry later"),
        Some("subscription_sharing_invalid_user") => {
            return Error::ChatGpt(
                "account authorization was rejected; run `bee auth login <profile>`",
            );
        }
        Some("chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context") => {
            return Error::ChatGpt(
                "permission is insufficient; check the selected registration and granted scopes",
            );
        }
        Some(
            "subscription_sharing_route_not_supported"
            | "subscription_sharing_unsupported_capability",
        ) => {
            return Error::ChatGpt(
                "unsupported route or capability; check the ChatGPT profile and model",
            );
        }
        _ => {}
    }
    if error["code"] == "context_length_exceeded" || error["type"] == "context_length_exceeded" {
        return Error::ContextLimit;
    }
    if protocol == Protocol::AnthropicMessages {
        match error["type"].as_str() {
            Some("authentication_error" | "permission_error") => return Error::Authentication,
            Some("rate_limit_error") => return Error::RateLimit,
            Some("invalid_request_error")
                if error["message"]
                    .as_str()
                    .is_some_and(|message| message.starts_with("prompt is too long")) =>
            {
                return Error::ContextLimit;
            }
            _ => {}
        }
    }
    Error::Provider
}

pub(crate) fn string(value: &Value) -> Result<&str, Error> {
    value.as_str().ok_or(Error::Malformed)
}

pub(crate) fn call(id: &Value, name: &Value, arguments: &str) -> Result<Call, Error> {
    let id = string(id)?;
    let name = string(name)?;
    if id.is_empty() || name.is_empty() || arguments.len() > crate::http::MAX_EVENT {
        return Err(Error::Malformed);
    }
    let arguments: Value = serde_json::from_str(arguments).map_err(|_| Error::Malformed)?;
    if !arguments.is_object() {
        return Err(Error::Malformed);
    }
    Ok(Call {
        id: id.into(),
        name: name.into(),
        arguments,
    })
}
