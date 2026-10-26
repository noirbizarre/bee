//! Typed native tools and bounded dispatch, independent of providers and ACP.

#![doc = include_str!("../docs/tools.md")]

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;

use schemars::{JsonSchema, generate::SchemaSettings};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub mod bash;
pub mod copy;
pub mod delete;
mod discovery;
pub mod edit;
mod fs;
pub mod glob;
pub mod grep;
pub mod list;
pub mod r#move;
mod mutation;
pub mod read;
mod tree;
pub mod write;

/// Maximum serialized bytes per tool result, including its JSON envelope.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Minimum budget, leaving room for a complete fixed failure envelope.
pub const MIN_OUTPUT_BYTES: usize = 256;

/// Compose shipped tools once, keeping registration outside the inference loop.
pub fn builtins() -> Result<Registry, Error> {
    builtins_with_bash(bash::Settings::default())
}

/// Compose shipped tools with an explicitly validated Bash execution policy.
pub fn builtins_with_bash(settings: bash::Settings) -> Result<Registry, Error> {
    let mut registry = Registry::default();
    registry.register(bash::Bash::new(settings))?;
    registry.register(read::Read)?;
    registry.register(glob::Glob)?;
    registry.register(grep::Grep)?;
    registry.register(list::List)?;
    registry.register(write::Write)?;
    registry.register(edit::Edit)?;
    registry.register(delete::Delete)?;
    registry.register(r#move::Move)?;
    registry.register(copy::Copy)?;
    Ok(registry)
}

/// Safe-to-display registration, context, or cancellation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    /// Names must be portable across Bee's supported function-call protocols.
    #[error("invalid native tool name; use 1–64 ASCII letters, digits, underscores or hyphens")]
    #[diagnostic(code(bee::tool::name))]
    Name,
    /// A description is needed for useful model selection.
    #[error("empty native tool description; describe when the model should use the tool")]
    #[diagnostic(code(bee::tool::description))]
    Description,
    /// Two implementations cannot own the same dispatch name.
    #[error("duplicate native tool name; register each tool name only once")]
    #[diagnostic(code(bee::tool::duplicate))]
    Duplicate,
    /// The generated schema cannot be safely advertised or compiled.
    #[error("invalid native tool schema; use a Draft 2020-12 object schema with local references")]
    #[diagnostic(code(bee::tool::schema))]
    Schema,
    /// The configured bound cannot contain a result or exceeds the loop ceiling.
    #[error("invalid native tool output limit; choose between 256 and 1048576 bytes")]
    #[diagnostic(code(bee::tool::limit))]
    Limit,
    /// Cancellation aborts dispatch rather than becoming model-visible error data.
    #[error("native tool execution was cancelled; start a new turn to continue")]
    #[diagnostic(code(bee::tool::cancelled))]
    Cancelled,
}

/// Per-turn execution inputs, supplied by the session owner.
type ArtifactSink = std::sync::Arc<
    dyn Fn(&str, crate::presentation::Artifact) -> Result<(), Failure> + Send + Sync,
>;
type Prepared = std::sync::Arc<
    std::sync::Mutex<BTreeMap<String, Result<std::sync::Arc<tree::Manifest>, Failure>>>,
>;

/// Per-turn execution inputs, supplied by the session owner.
#[derive(Clone)]
pub struct Context {
    read_snapshot: Option<std::sync::Arc<str>>,
    cwd: PathBuf,
    cancellation: CancellationToken,
    output_limit: usize,
    artifact: Option<ArtifactSink>,
    call_id: String,
    prepared: Prepared,
}

impl Context {
    /// Construct a context without inspecting or changing process cwd.
    ///
    /// The directory is an execution base, not a filesystem sandbox. Implementors
    /// must bound intermediate allocations, not just the final serialized result.
    pub fn new(
        cwd: impl Into<PathBuf>,
        cancellation: CancellationToken,
        output_limit: usize,
    ) -> Result<Self, Error> {
        if !(MIN_OUTPUT_BYTES..=MAX_OUTPUT_BYTES).contains(&output_limit) {
            return Err(Error::Limit);
        }
        Ok(Self {
            read_snapshot: None,
            cwd: cwd.into(),
            cancellation,
            output_limit,
            artifact: None,
            call_id: String::new(),
            prepared: Default::default(),
        })
    }

    /// Session/project directory; relative tool paths should be resolved here.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Signal for cooperative cancellation; tools must not detach pending work.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Maximum serialized result bytes, including the envelope.
    pub fn output_limit(&self) -> usize {
        self.output_limit
    }
    pub(crate) fn with_artifacts(
        mut self,
        sink: impl Fn(&str, crate::presentation::Artifact) -> Result<(), Failure>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.artifact = Some(std::sync::Arc::new(sink));
        self
    }
    pub(crate) fn for_call(&self, id: &str) -> Self {
        let mut context = self.clone();
        context.call_id = id.into();
        context
    }
    pub(crate) fn emit(&self, artifact: crate::presentation::Artifact) -> Result<(), Failure> {
        self.artifact
            .as_ref()
            .map_or(Ok(()), |sink| sink(&self.call_id, artifact))
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Context([redacted])")
    }
}

/// A validated native filesystem target, used for instruction scope discovery.
pub struct Target {
    /// Path relative to the execution context, or absolute.
    pub path: PathBuf,
    /// Whether the target itself is a directory rather than a file.
    pub directory: bool,
    /// Whether a successful operation can activate a cataloged skill.
    pub read: bool,
}

/// Stable model-facing failure classifications, not diagnostic codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// No registered implementation owns this name.
    UnknownTool,
    /// Arguments failed schema validation, decoding, or semantic checks.
    InvalidArguments,
    /// The implementation or output serialization failed.
    Execution,
    /// The serialized result exceeded the context's budget.
    OutputLimit,
}

/// An ordinary tool failure deliberately returned to the model.
///
/// Messages may contain intentional model-facing details. They are never included
/// in diagnostic formatting, debug output, or an error source chain.
#[derive(Clone, Serialize, thiserror::Error)]
#[error("native tool failure ({code:?}); inspect the model-facing result")]
pub struct Failure {
    code: Code,
    message: String,
}

impl Failure {
    /// Construct a failure with an intentional, actionable model-facing message.
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Failure")
            .field("code", &self.code)
            .finish_non_exhaustive()
    }
}

/// Implement a native operation; registration erases only its concrete types.
pub trait Tool: Send + Sync + 'static {
    /// Concrete authorization checks; custom tools ask unless explicitly described otherwise.
    fn permissions(&self, _input: &Self::Input) -> Vec<crate::permissions::Check> {
        vec![crate::permissions::Check::value(
            self.name(),
            "*",
            crate::permissions::Effect::Ask,
        )]
    }
    /// Display kind is owned by the implementation, not guessed from its name.
    fn kind(&self) -> &'static str {
        "other"
    }
    /// Additional affected scopes, including nested mutation directories.
    fn targets(&self, input: &Self::Input, _context: &Context) -> Vec<Target> {
        self.target(input).into_iter().collect()
    }
    /// Input schema and decoder must describe the same object-shaped input.
    type Input: DeserializeOwned + JsonSchema + Send;
    /// Structured success data; serialization is bounded by the registry.
    type Output: Serialize + Send;

    /// Portable function name, unique within the registry.
    fn name(&self) -> &str;
    /// Model-facing explanation of when and how to use this operation.
    fn description(&self) -> &str;
    /// Describe a filesystem target without executing; custom tools opt in explicitly.
    fn target(&self, _input: &Self::Input) -> Option<Target> {
        None
    }
    /// Check semantic constraints before a caller asks authorization for an operation.
    fn preflight(&self, _input: &Self::Input) -> Result<(), Failure> {
        Ok(())
    }
    /// Execute validated input. Dropping this future must stop pending work.
    ///
    /// Await cancellation for cooperative cleanup, but do not depend on async
    /// cleanup running: an outer cancellation may drop this future immediately.
    fn execute(
        &self,
        input: Self::Input,
        context: &Context,
    ) -> impl Future<Output = Result<Self::Output, Failure>> + Send;
}

/// Immutable advertised metadata paired with a compiled validator on registration.
#[derive(Clone)]
pub struct Definition {
    /// Validated, portable dispatch name.
    pub name: String,
    /// Model-facing usage guidance.
    pub description: String,
    /// The exact Draft 2020-12 schema enforced before typed decoding.
    pub input_schema: Value,
}

impl fmt::Debug for Definition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Definition([redacted])")
    }
}

/// A complete bounded JSON envelope, ready for correlation by the caller.
pub struct Output {
    /// JSON containing either `data` or a structured `error` object.
    pub json: String,
    /// Whether the envelope contains an ordinary tool failure.
    pub is_error: bool,
}

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Output([redacted])")
    }
}

type Execution<'a> = Pin<Box<dyn Future<Output = Output> + Send + 'a>>;

trait Erased: Send + Sync {
    fn permissions(&self, arguments: Value) -> Vec<crate::permissions::Check>;
    fn kind(&self) -> &'static str;
    fn targets(&self, arguments: Value, context: &Context) -> Vec<Target>;
    fn preflight(&self, arguments: Value) -> Result<(), Failure>;
    fn target(&self, arguments: Value) -> Option<Target>;
    fn execute<'a>(&'a self, arguments: Value, context: &'a Context) -> Execution<'a>;
}

impl<T: Tool> Erased for T {
    fn permissions(&self, arguments: Value) -> Vec<crate::permissions::Check> {
        serde_json::from_value(arguments)
            .ok()
            .map_or_else(Vec::new, |input| Tool::permissions(self, &input))
    }
    fn kind(&self) -> &'static str {
        Tool::kind(self)
    }
    fn targets(&self, arguments: Value, context: &Context) -> Vec<Target> {
        serde_json::from_value(arguments)
            .ok()
            .map_or_else(Vec::new, |input| Tool::targets(self, &input, context))
    }
    fn preflight(&self, arguments: Value) -> Result<(), Failure> {
        let input = serde_json::from_value(arguments).map_err(|_| {
            Failure::new(
                Code::InvalidArguments,
                "Arguments could not be decoded; follow the input schema.",
            )
        })?;
        Tool::preflight(self, &input)
    }
    fn target(&self, arguments: Value) -> Option<Target> {
        let input = serde_json::from_value(arguments).ok()?;
        Tool::target(self, &input)
    }
    fn execute<'a>(&'a self, arguments: Value, context: &'a Context) -> Execution<'a> {
        Box::pin(async move {
            // Never retain Serde's error: it can quote arguments containing keys.
            let Ok(input) = serde_json::from_value(arguments) else {
                return failure(
                    Code::InvalidArguments,
                    "Arguments could not be decoded; follow the input schema.",
                    context.output_limit,
                );
            };
            match Tool::execute(self, input, context).await {
                Ok(data) => encode(&Success { data }, false, context.output_limit),
                Err(error) => encode(&ErrorEnvelope { error }, true, context.output_limit),
            }
        })
    }
}

struct Entry {
    definition: Definition,
    validator: jsonschema::Validator,
    implementation: Box<dyn Erased>,
}

/// Explicit native-tool registration and dispatch; no global discovery or state.
#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<String, Entry>,
}

impl Registry {
    pub(crate) fn permissions(
        &self,
        name: &str,
        arguments: &Value,
    ) -> Vec<crate::permissions::Check> {
        self.entries.get(name).map_or_else(Vec::new, |entry| {
            entry.implementation.permissions(arguments.clone())
        })
    }
    pub(crate) fn targets(&self, name: &str, arguments: &Value, context: &Context) -> Vec<Target> {
        self.entries
            .get(name)
            .filter(|e| e.validator.is_valid(arguments))
            .map_or_else(Vec::new, |e| {
                e.implementation.targets(arguments.clone(), context)
            })
    }
    pub(crate) fn description(
        &self,
        name: &str,
        arguments: &Value,
        context: &Context,
    ) -> crate::presentation::Description {
        crate::presentation::Description {
            name: name.into(),
            kind: self
                .entries
                .get(name)
                .map_or("other", |e| e.implementation.kind()),
            locations: self
                .targets(name, arguments, context)
                .into_iter()
                .filter_map(|t| fs::path(context, &t.path.to_string_lossy()).ok())
                .take(64)
                .collect(),
        }
    }
    /// Validate without executing so authorization never precedes input checks.
    pub(crate) fn preflight(&self, name: &str, arguments: &Value) -> Result<(), Failure> {
        let entry = self.entries.get(name).ok_or_else(|| {
            Failure::new(Code::UnknownTool, "Unknown tool; choose a registered tool.")
        })?;
        if !arguments.is_object() || !entry.validator.is_valid(arguments) {
            return Err(Failure::new(
                Code::InvalidArguments,
                "Arguments do not match the input schema; correct the call.",
            ));
        }
        entry.implementation.preflight(arguments.clone())
    }
    /// Inspect schema-valid arguments using the registered implementation's decoder.
    pub fn target(&self, name: &str, arguments: &Value) -> Option<Target> {
        let entry = self.entries.get(name)?;
        if !entry.validator.is_valid(arguments) {
            return None;
        }
        entry.implementation.target(arguments.clone())
    }

    pub(crate) async fn invoke_snapshot(
        &self,
        name: &str,
        arguments: &Value,
        context: &Context,
        snapshot: std::sync::Arc<str>,
    ) -> Result<Output, Error> {
        let mut context = context.clone();
        context.read_snapshot = Some(snapshot);
        self.invoke(name, arguments, &context).await
    }
    /// Validate and register a typed implementation without replacing collisions.
    pub fn register<T: Tool>(&mut self, tool: T) -> Result<(), Error> {
        let name = tool.name();
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(Error::Name);
        }
        if self.entries.contains_key(name) {
            return Err(Error::Duplicate);
        }
        if tool.description().trim().is_empty() {
            return Err(Error::Description);
        }
        let schema = SchemaSettings::draft2020_12()
            .for_deserialize()
            .into_generator()
            .into_root_schema_for::<T::Input>();
        let input_schema = serde_json::to_value(schema).map_err(|_| Error::Schema)?;
        if input_schema.get("type").and_then(Value::as_str) != Some("object")
            || input_schema.get("$schema").and_then(Value::as_str)
                != Some("https://json-schema.org/draft/2020-12/schema")
        {
            return Err(Error::Schema);
        }
        // Offline compilation supports local definitions without URL/file I/O.
        // Discard compiler diagnostics because schema defaults may be secrets.
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .offline()
            .build(&input_schema)
            .map_err(|_| Error::Schema)?;
        let definition = Definition {
            name: name.to_owned(),
            description: tool.description().to_owned(),
            input_schema,
        };
        self.entries.insert(
            definition.name.clone(),
            Entry {
                definition,
                validator,
                implementation: Box::new(tool),
            },
        );
        Ok(())
    }

    /// Advertise definitions in name order, independent of registration order.
    pub fn definitions(&self) -> impl Iterator<Item = &Definition> {
        self.entries.values().map(|entry| &entry.definition)
    }

    /// Validate and invoke a tool; ordinary failures are JSON results.
    ///
    /// Cancellation returns `Error::Cancelled`. It wins over a simultaneously
    /// ready result, and dropping this future cancels the child execution token.
    pub async fn invoke(
        &self,
        name: &str,
        arguments: &Value,
        context: &Context,
    ) -> Result<Output, Error> {
        if context.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let Some(entry) = self.entries.get(name) else {
            return Ok(failure(
                Code::UnknownTool,
                "Unknown tool; choose a registered tool.",
                context.output_limit,
            ));
        };
        if !arguments.is_object() || !entry.validator.is_valid(arguments) {
            return Ok(failure(
                Code::InvalidArguments,
                "Arguments do not match the input schema; correct the call.",
                context.output_limit,
            ));
        }
        let execution = Context {
            read_snapshot: context.read_snapshot.clone(),
            cwd: context.cwd.clone(),
            cancellation: context.cancellation.child_token(),
            output_limit: context.output_limit,
            artifact: context.artifact.clone(),
            call_id: context.call_id.clone(),
            prepared: context.prepared.clone(),
        };
        // Also signal observers when an outer Abortable drops this dispatch.
        let _guard = execution.cancellation.clone().drop_guard();
        tokio::select! {
            biased;
            () = execution.cancellation.cancelled() => Err(Error::Cancelled),
            result = entry.implementation.execute(arguments.clone(), &execution) => {
                if execution.cancellation.is_cancelled() {
                    Err(Error::Cancelled)
                } else {
                    Ok(result)
                }
            }
        }
    }
}

#[derive(Serialize)]
struct Success<T> {
    data: T,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: Failure,
}

fn failure(code: Code, message: &str, limit: usize) -> Output {
    encode(
        &ErrorEnvelope {
            error: Failure::new(code, message),
        },
        true,
        limit,
    )
}

struct Bounded {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            self.exceeded = true;
            return Err(io::Error::other("tool output limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode(value: &impl Serialize, is_error: bool, limit: usize) -> Output {
    let mut writer = Bounded {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    if serde_json::to_writer(&mut writer, value).is_err() {
        // Replace the whole envelope: truncating JSON would hide its error code.
        // Both fixed envelopes fit the minimum context budget, without recursion.
        let (code, message) = if writer.exceeded {
            (
                Code::OutputLimit,
                "Result exceeded the output limit; request less data.",
            )
        } else {
            (
                Code::Execution,
                "Tool output could not be serialized; correct the implementation.",
            )
        };
        return Output {
            json: serde_json::to_string(&ErrorEnvelope {
                error: Failure::new(code, message),
            })
            .expect("fixed failure envelope is serializable"),
            is_error: true,
        };
    }
    Output {
        // serde_json emits UTF-8 even when a custom Serialize impl is used.
        json: String::from_utf8(writer.bytes).expect("JSON serialization is UTF-8"),
        is_error,
    }
}
