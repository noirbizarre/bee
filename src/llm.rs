//! SDK-free inference types and protocol dispatch.

pub mod anthropic;
mod cache;
pub mod chat;
pub mod responses;
mod usage;
pub use usage::{CacheCounter, CacheUsage};

use base64::Engine;
use std::fmt;

use serde_json::Value;

use crate::auth::Secret;
use crate::provider::{Profile, Protocol};

/// A normalized, safe-to-display inference failure. Never stores wire values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    /// Conversation identity must come from operating-system entropy.
    #[error("cannot generate a conversation identity; check operating-system randomness and retry")]
    #[diagnostic(code(bee::llm::randomness))]
    Randomness,
    /// Fixed Cloud recovery advice, never upstream error text.
    #[error("Google Cloud inference: {0}")]
    #[diagnostic(code(bee::llm::google_cloud))]
    GoogleCloud(&'static str),
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
    /// Fixed Copilot compatibility/authentication guidance.
    #[error("GitHub Copilot request failed: {0}")]
    #[diagnostic(code(bee::llm::copilot))]
    Copilot(&'static str),
    /// Fixed recovery advice for Console API keys and offering limits.
    #[error("OpenCode request failed: {0}")]
    #[diagnostic(code(bee::llm::opencode))]
    OpenCode(&'static str),
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
    /// A requested cache policy is not documented for the selected route.
    #[error(
        "unsupported prompt-cache setting: {0}; reset or disable that option, or select a documented endpoint/model combination (see inference documentation)"
    )]
    #[diagnostic(code(bee::llm::cache))]
    Cache(&'static str),
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
    /// Reported native cache counters, retaining their provider-specific meanings.
    pub cache: Vec<CacheUsage>,
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
    /// Ordered output items, content blocks, or a signed Chat assistant snapshot.
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
    conversation_id: String,
    google_cloud_session: Option<crate::auth::google_cloud::Session>,
    copilot_session: Option<crate::auth::copilot::Session>,
    copilot_agent: bool,
    chatgpt_session: Option<crate::auth::chatgpt::Session>,
    instructions: String,
    cache_scope: String,
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
        let mut identity = [0_u8; 16];
        getrandom::fill(&mut identity).map_err(|_| Error::Randomness)?;
        Ok(Self {
            conversation_id: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(identity),
            google_cloud_session: None,
            copilot_session: None,
            copilot_agent: false,
            chatgpt_session: None,
            instructions: String::new(),
            cache_scope: "default".into(),
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
        if self.profile.google_cloud().is_some() != self.google_cloud_session.is_some() {
            return Err(Error::GoogleCloud(
                "prepare a renewable Google Cloud session before inference",
            ));
        }
        if self.profile.is_copilot() != self.copilot_session.is_some() {
            return Err(Error::Copilot(
                "prepare a validated Copilot session before inference",
            ));
        }
        if let Some(session) = &self.copilot_session {
            session.check(&self.profile).await.map_err(|_| {
                Error::Copilot(
                    "credentials changed or became unavailable; start a new turn or repeat login",
                )
            })?;
            if self.credential.as_ref() != Some(session.credential.secret()) {
                return Err(Error::Copilot(
                    "request credential does not match the prepared session",
                ));
            }
        }
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
        let mut cloud;
        let model = if let Some(session) = &model.google_cloud_session {
            session.check(&model.profile).map_err(|_| {
                Error::GoogleCloud("prepared session does not match the Cloud target")
            })?;
            cloud = model.clone();
            cloud.credential = Some(session.credential().await.map_err(|_| Error::GoogleCloud("ADC changed or cannot be renewed; check credentials and start a new turn"))?.secret().clone());
            &cloud
        } else {
            model
        };
        let mut copilot;
        let model = if model.copilot_session.is_some() {
            copilot = model.clone();
            // Classify semantic tool continuations before Messages wraps them in a user role.
            copilot.copilot_agent = !matches!(messages.last(), Some(Message::User(_)));
            &copilot
        } else {
            model
        };
        let result = match model.endpoint().protocol() {
            Protocol::OpenAiChat => chat::run(model, messages, tools, text).await,
            Protocol::OpenAiResponses => responses::run(model, messages, tools, text).await,
            Protocol::AnthropicMessages => anthropic::run(model, messages, tools, text).await,
        };
        if model
            .profile
            .discovery()
            .and_then(crate::models::Integration::opencode_root)
            .is_some()
        {
            // Access denial also covers subscription and model policy, not just invalid keys.
            return result.map_err(|error| match error {
                Error::Authentication => Error::OpenCode("access denied; check this connection's API key, workspace billing/subscription and model access in https://opencode.ai/console"),
                Error::RateLimit => Error::OpenCode("usage or rate limit reached; check this offering's allowance in https://opencode.ai/console and retry later"),
                error => error,
            });
        }
        result
    }
}

impl Model {
    /// Use turn-pinned renewable ADC for official Google Cloud inference.
    pub fn with_google_cloud_session(
        mut self,
        session: crate::auth::google_cloud::Session,
    ) -> Self {
        self.google_cloud_session = Some(session);
        self
    }
    /// Attach a trusted target and identity pinned for this turn.
    pub fn with_copilot_session(mut self, session: crate::auth::copilot::Session) -> Self {
        self.copilot_session = Some(session);
        self
    }

    fn base_url(&self) -> &str {
        self.copilot_session.as_ref().map_or_else(
            || self.endpoint().base_url(),
            |session| session.target.as_str(),
        )
    }
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

    /// Group automatic cache accounting without including session IDs or prompt content.
    /// Embedded callers may retain the default scope or supply their stable grouping profile.
    pub fn with_cache_scope(mut self, scope: String) -> Self {
        self.cache_scope = scope;
        self
    }

    /// Preserve conversation routing when a caller constructs a model for each turn.
    pub fn with_conversation_id(mut self, id: String) -> Self {
        self.conversation_id = id;
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
        let mut headers = request_headers(
            &self.profile,
            self.endpoint().protocol(),
            self.credential.as_ref(),
            version,
            true,
        )?;
        if self
            .profile
            .discovery()
            .and_then(crate::models::Integration::opencode_root)
            .is_some()
        {
            // A conversation is stable across steps, unlike an individual HTTP request.
            if self.conversation_id.is_empty() || self.conversation_id.len() > 256 {
                return Err(Error::Headers);
            }
            let mut value = reqwest::header::HeaderValue::from_str(&self.conversation_id)
                .map_err(|_| Error::Headers)?;
            value.set_sensitive(true);
            headers.insert("x-opencode-session", value);
        }
        if let Some(project) = self
            .google_cloud_session
            .as_ref()
            .and_then(|s| s.quota_project())
        {
            let mut value =
                reqwest::header::HeaderValue::from_str(project).map_err(|_| Error::Headers)?;
            value.set_sensitive(true);
            headers.insert("x-goog-user-project", value);
        }
        if let Some(session) = &self.copilot_session {
            use reqwest::header::HeaderValue;
            headers.insert(
                "openai-intent",
                HeaderValue::from_static("conversation-edits"),
            );
            headers.insert(
                "x-interaction-type",
                HeaderValue::from_static("conversation-agent"),
            );
            headers.insert(
                "x-interaction-id",
                HeaderValue::from_str(&session.interaction).map_err(|_| Error::Headers)?,
            );
            headers.insert(
                "x-initiator",
                HeaderValue::from_static(if self.copilot_agent { "agent" } else { "user" }),
            );
        }
        Ok(headers)
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
        let (name, value) = if profile.is_copilot() {
            (
                "authorization",
                Secret::new(format!("Bearer {}", secret.expose())),
            )
        } else {
            crate::auth::header_for(protocol, secret)
        };
        let mut value = HeaderValue::from_str(value.expose()).map_err(|_| Error::Headers)?;
        value.set_sensitive(true);
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Headers)?,
            value,
        );
    }
    let opencode = profile
        .discovery()
        .and_then(crate::models::Integration::opencode_root)
        .is_some();
    if profile.is_copilot() || opencode {
        headers.insert(
            "user-agent",
            HeaderValue::from_static(concat!("bee/", env!("CARGO_PKG_VERSION"))),
        );
    }
    if profile.is_copilot() {
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static("2026-08-01"),
        );
    }
    for (name, value) in profile.headers() {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Headers)?;
        if headers.contains_key(&name)
            || (opencode && name.as_str() == "x-opencode-session")
            || (profile.is_copilot() && crate::auth::copilot::reserved_header(name.as_str()))
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
    // Google ErrorInfo reasons are machine identifiers; free-text messages and metadata stay private.
    if let Some(details) = error["details"].as_array() {
        for detail in details {
            if detail["@type"] != "type.googleapis.com/google.rpc.ErrorInfo" {
                continue;
            }
            match detail["reason"].as_str() {
                Some("SERVICE_DISABLED") => {
                    return Error::GoogleCloud(
                        "Vertex AI API is disabled; enable aiplatform.googleapis.com in the configured project",
                    );
                }
                Some("BILLING_DISABLED" | "BILLING_NOT_ACTIVE") => {
                    return Error::GoogleCloud(
                        "billing is unavailable; enable billing for the resource and quota projects",
                    );
                }
                Some("IAM_PERMISSION_DENIED" | "ACCESS_DENIED") => {
                    return Error::GoogleCloud(
                        "permission denied; grant prediction access and serviceusage.services.use on the quota project where required",
                    );
                }
                _ => {}
            }
        }
    }
    match error["status"].as_str() {
        Some("UNAUTHENTICATED") => return Error::Authentication,
        Some("PERMISSION_DENIED") => {
            return Error::GoogleCloud(
                "permission denied; check Vertex AI IAM, API enablement, billing and quota-project access",
            );
        }
        Some("NOT_FOUND") => {
            return Error::GoogleCloud(
                "model or resource unavailable; check the configured project, location and google/ model ID",
            );
        }
        Some("RESOURCE_EXHAUSTED") => return Error::RateLimit,
        Some("UNAVAILABLE" | "INTERNAL") => {
            return Error::GoogleCloud(
                "service temporarily unavailable; retry later without changing the billing project",
            );
        }
        _ => {}
    }
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
