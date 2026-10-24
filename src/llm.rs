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
    /// The credential was rejected.
    #[error(
        "the model endpoint rejected authentication; check the configured credential or run `bee auth login`"
    )]
    #[diagnostic(code(bee::llm::authentication))]
    Authentication,
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
#[derive(Clone, PartialEq)]
pub struct Call {
    /// Correlates the model call with its result.
    pub id: String,
    /// Name of a declared tool.
    pub name: String,
    /// Parsed object arguments.
    pub arguments: Value,
}

/// Native continuation is kept separate from portable text.
#[derive(Clone, PartialEq)]
pub struct Native {
    /// The only protocol permitted to replay this value.
    pub protocol: Protocol,
    /// Ordered native output items or content blocks.
    pub items: Vec<Value>,
}

/// A conversation entry. Contents deliberately have no derived Debug.
#[derive(Clone, PartialEq)]
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
    profile: Profile,
    model: String,
    credential: Option<Secret>,
    http: crate::http::InferenceHttp,
}

impl Model {
    /// Construct a model without a vendor registry or catalog lookup.
    pub fn new(profile: Profile, model: String, credential: Option<Secret>) -> Result<Self, Error> {
        Ok(Self {
            http: crate::http::InferenceHttp::new()?,
            profile,
            model,
            credential,
        })
    }

    /// Produce one model step, streaming text to `text` as it arrives.
    pub async fn step(
        &self,
        messages: &[Message],
        tools: &[Tool],
        text: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Step, Error> {
        match self.endpoint().protocol() {
            Protocol::OpenAiChat => chat::run(self, messages, tools, text).await,
            Protocol::OpenAiResponses => responses::run(self, messages, tools, text).await,
            Protocol::AnthropicMessages => anthropic::run(self, messages, tools, text).await,
        }
    }
}

impl Model {
    fn endpoint(&self) -> &crate::provider::Endpoint {
        self.profile.endpoint_for_model(&self.model)
    }

    pub(crate) fn headers(
        &self,
        version: Option<&str>,
    ) -> Result<reqwest::header::HeaderMap, Error> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert(
            "accept",
            HeaderValue::from_static("text/event-stream, application/json"),
        );
        if let Some(version) = version {
            headers.insert(
                "anthropic-version",
                HeaderValue::from_str(version).map_err(|_| Error::Headers)?,
            );
        }
        if let Some(secret) = &self.credential {
            let (name, value) = crate::auth::header_for(self.endpoint().protocol(), secret);
            let mut value = HeaderValue::from_str(value.expose()).map_err(|_| Error::Headers)?;
            value.set_sensitive(true);
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Headers)?,
                value,
            );
        }
        for (name, value) in self.profile.headers() {
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
