//! Native contracts without providers, plus the executor's cancellation boundary.

use std::borrow::Cow;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use bee::agent::{Executor, NativeExecutor};
use bee::tool::{
    Code, Context, Error, Failure, MAX_OUTPUT_BYTES, MIN_OUTPUT_BYTES, Registry, Tool,
};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Input {
    #[schemars(length(min = 1))]
    text: String,
    #[serde(default)]
    fail: bool,
}

struct Echo {
    name: String,
    description: String,
    calls: Arc<AtomicUsize>,
}

impl Echo {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            description: "Echo the text".into(),
            calls: Arc::default(),
        }
    }
}

impl Tool for Echo {
    type Input = Input;
    type Output = Value;

    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }

    async fn execute(&self, input: Input, _: &Context) -> Result<Value, Failure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if input.fail {
            Err(Failure::new(Code::Execution, input.text))
        } else {
            Ok(json!({"text": input.text}))
        }
    }
}

fn context(limit: usize) -> Context {
    Context::new("/session", CancellationToken::new(), limit).unwrap()
}

fn registry() -> Registry {
    let mut registry = Registry::default();
    registry.register(Echo::new("echo")).unwrap();
    registry
}

fn envelope(output: &bee::tool::Output) -> Value {
    serde_json::from_str(&output.json).unwrap()
}

#[test]
fn registration_validates_metadata_and_keeps_the_original_on_collision() {
    let mut registry = registry();
    let mut replacement = Echo::new("echo");
    replacement.description = "Replace the original".into();
    assert_eq!(registry.register(replacement), Err(Error::Duplicate));
    registry.register(Echo::new("aaa")).unwrap();
    assert_eq!(
        registry
            .definitions()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["aaa", "echo"]
    );
    assert_eq!(
        registry.definitions().last().unwrap().description,
        "Echo the text"
    );
    for name in ["", "with space", "a.b", "héllo", &"a".repeat(65)] {
        assert_eq!(registry.register(Echo::new(name)), Err(Error::Name));
    }
    registry.register(Echo::new(&"a".repeat(64))).unwrap();
    registry.register(Echo::new("A-z_09")).unwrap();
    let mut empty = Echo::new("empty");
    empty.description = " \n ".into();
    assert_eq!(registry.register(empty), Err(Error::Description));
}

// Deliberately custom schemas exercise rejection at the public registration seam.
#[derive(Deserialize)]
struct SchemaInput<const KIND: u8> {
    text: String,
}

impl<const KIND: u8> JsonSchema for SchemaInput<KIND> {
    fn schema_name() -> Cow<'static, str> {
        format!("SchemaInput{KIND}").into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        let value = match KIND {
            0 => json!({"type":"object","required":"sentinel"}),
            1 => {
                json!({"type":"object","properties":{"text":{"$ref":"https://invalid.example/sentinel"}}})
            }
            2 => {
                json!({"type":"object","properties":{"text":{"$ref":"#/$defs/Text"}},"$defs":{"Text":{"type":"string"}},"required":["text"]})
            }
            4 => json!({"type":"string"}),
            5 => json!({"type":"object","properties":{"text":{}},"required":["text"]}),
            _ => json!({"type":"object","properties":{"text":{"$ref":"file:///sentinel"}}}),
        };
        value.try_into().unwrap()
    }
}

struct SchemaTool<const KIND: u8>;
impl<const KIND: u8> Tool for SchemaTool<KIND> {
    type Input = SchemaInput<KIND>;
    type Output = String;
    fn name(&self) -> &str {
        "schema"
    }
    fn description(&self) -> &str {
        "Schema validation fixture"
    }
    async fn execute(&self, input: Self::Input, _: &Context) -> Result<String, Failure> {
        Ok(input.text)
    }
}

#[test]
fn invalid_schemas_and_external_references_are_rejected_without_diagnostics_containing_values() {
    let mut registry = Registry::default();
    for result in [
        registry.register(SchemaTool::<0>),
        registry.register(SchemaTool::<1>),
        registry.register(SchemaTool::<4>),
        registry.register(SchemaTool::<6>),
    ] {
        let error = result.unwrap_err();
        assert_eq!(error, Error::Schema);
        assert!(!format!("{error} {error:?}").contains("sentinel"));
        assert!(std::error::Error::source(&error).is_none());
        assert!(!format!("{:?}", miette::Report::new(error)).contains("sentinel"));
    }
    assert_eq!(registry.definitions().count(), 0);
}

#[tokio::test]
async fn local_references_are_validated_and_decoding_is_an_additional_check() {
    let mut registry = Registry::default();
    registry.register(SchemaTool::<2>).unwrap();
    let ctx = context(MAX_OUTPUT_BYTES);
    let good = registry
        .invoke("schema", &json!({"text":"yes", "extra":"allowed"}), &ctx)
        .await
        .unwrap();
    assert_eq!(envelope(&good), json!({"data":"yes"}));
    let bad = registry
        .invoke("schema", &json!({"text":123}), &ctx)
        .await
        .unwrap();
    assert_eq!(envelope(&bad)["error"]["code"], "invalid_arguments");

    let mut registry = Registry::default();
    registry.register(SchemaTool::<5>).unwrap();
    let bad = registry
        .invoke("schema", &json!({"text":123}), &ctx)
        .await
        .unwrap();
    assert_eq!(envelope(&bad)["error"]["code"], "invalid_arguments");
}

#[tokio::test]
async fn schema_invalid_arguments_never_reach_the_implementation() {
    let tool = Echo::new("echo");
    let calls = tool.calls.clone();
    let mut registry = Registry::default();
    registry.register(tool).unwrap();
    let ctx = context(MAX_OUTPUT_BYTES);
    for arguments in [
        json!(null),
        json!([]),
        json!({}),
        json!({"text":123}),
        json!({"text":""}),
        json!({"text":"yes","sentinel":"secret"}),
    ] {
        let result = registry.invoke("echo", &arguments, &ctx).await.unwrap();
        assert!(result.is_error);
        assert_eq!(envelope(&result)["error"]["code"], "invalid_arguments");
        assert!(!result.json.contains("sentinel"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let result = registry
        .invoke("echo", &json!({"text":"héllo"}), &ctx)
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(envelope(&result), json!({"data":{"text":"héllo"}}));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let missing = registry.invoke("sentinel", &json!({}), &ctx).await.unwrap();
    assert_eq!(envelope(&missing)["error"]["code"], "unknown_tool");
    assert!(!missing.json.contains("sentinel"));
}

#[tokio::test]
async fn failures_are_model_data_but_debug_and_diagnostics_are_redacted() {
    let failure = Failure::new(Code::Execution, "sentinel");
    assert!(!format!("{failure:?} {failure}").contains("sentinel"));
    assert!(std::error::Error::source(&failure).is_none());
    let registry = registry();
    let result = registry
        .invoke(
            "echo",
            &json!({"text":"sentinel","fail":true}),
            &context(MAX_OUTPUT_BYTES),
        )
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(
        envelope(&result)["error"],
        json!({"code":"execution","message":"sentinel"})
    );
    assert!(!format!("{result:?}").contains("sentinel"));
    let ctx = Context::new("/sentinel", CancellationToken::new(), MAX_OUTPUT_BYTES).unwrap();
    assert!(!format!("{ctx:?}").contains("sentinel"));
    let mut definition = registry.definitions().next().unwrap().clone();
    definition.input_schema["default"] = "sentinel".into();
    assert!(!format!("{definition:?}").contains("sentinel"));
}

#[tokio::test]
async fn output_limits_count_utf8_bytes_and_complete_envelope_overhead() {
    let registry = registry();
    let ctx = context(MIN_OUTPUT_BYTES);
    let overhead = json!({"data":{"text":""}}).to_string().len();
    let text = "x".repeat(MIN_OUTPUT_BYTES - overhead);
    let exact = registry
        .invoke("echo", &json!({"text":text}), &ctx)
        .await
        .unwrap();
    assert!(!exact.is_error);
    assert_eq!(exact.json.len(), MIN_OUTPUT_BYTES);
    for text in [
        format!("{text}x"),
        "é".repeat((MIN_OUTPUT_BYTES - overhead) / 2 + 1),
        "\n".repeat(MIN_OUTPUT_BYTES),
    ] {
        let result = registry
            .invoke("echo", &json!({"text":text}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.json.len() <= MIN_OUTPUT_BYTES);
        assert_eq!(envelope(&result)["error"]["code"], "output_limit");
    }
    let result = registry
        .invoke(
            "echo",
            &json!({"text":"x".repeat(MIN_OUTPUT_BYTES),"fail":true}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.json.len() <= MIN_OUTPUT_BYTES);
    assert_eq!(envelope(&result)["error"]["code"], "output_limit");
    for limit in [0, MIN_OUTPUT_BYTES - 1, MAX_OUTPUT_BYTES + 1, usize::MAX] {
        assert_eq!(
            Context::new("/", CancellationToken::new(), limit).unwrap_err(),
            Error::Limit
        );
    }
}

struct BrokenOutput;

impl serde::Serialize for BrokenOutput {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("sentinel"))
    }
}

struct Broken;

impl Tool for Broken {
    type Input = Empty;
    type Output = BrokenOutput;
    fn name(&self) -> &str {
        "broken"
    }
    fn description(&self) -> &str {
        "Return a failing serializer"
    }
    async fn execute(&self, _: Empty, _: &Context) -> Result<BrokenOutput, Failure> {
        Ok(BrokenOutput)
    }
}

#[tokio::test]
async fn serializer_failures_produce_bounded_errors_without_source_text() {
    let mut registry = Registry::default();
    registry.register(Broken).unwrap();
    let result = registry
        .invoke("broken", &json!({}), &context(MIN_OUTPUT_BYTES))
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(envelope(&result)["error"]["code"], "execution");
    assert!(!result.json.contains("sentinel"));
    assert!(result.json.len() <= MIN_OUTPUT_BYTES);
}

struct CancelReady;

impl Tool for CancelReady {
    type Input = Empty;
    type Output = Value;
    fn name(&self) -> &str {
        "cancel"
    }
    fn description(&self) -> &str {
        "Cancel while producing a result"
    }
    async fn execute(&self, _: Empty, ctx: &Context) -> Result<Value, Failure> {
        ctx.cancellation().cancel();
        Ok(json!("completed"))
    }
}

#[tokio::test]
async fn cancellation_wins_over_a_result_and_does_not_cancel_sibling_work() {
    let mut registry = Registry::default();
    registry.register(CancelReady).unwrap();
    let ctx = context(MAX_OUTPUT_BYTES);
    assert_eq!(
        registry
            .invoke("cancel", &json!({}), &ctx)
            .await
            .unwrap_err(),
        Error::Cancelled
    );
    assert!(!ctx.cancellation().is_cancelled());
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

struct Inspect;
impl Tool for Inspect {
    type Input = Empty;
    type Output = Value;
    fn name(&self) -> &str {
        "inspect"
    }
    fn description(&self) -> &str {
        "Return supplied context"
    }
    async fn execute(&self, _: Empty, ctx: &Context) -> Result<Value, Failure> {
        Ok(json!({"cwd":ctx.cwd(),"limit":ctx.output_limit()}))
    }
}

#[tokio::test]
async fn session_directories_and_output_budgets_are_explicit_and_independent() {
    let mut registry = Registry::default();
    registry.register(Inspect).unwrap();
    for cwd in ["/project/one", "/project/two"] {
        let ctx = Context::new(cwd, CancellationToken::new(), MIN_OUTPUT_BYTES).unwrap();
        let result = registry.invoke("inspect", &json!({}), &ctx).await.unwrap();
        assert_eq!(
            envelope(&result)["data"],
            json!({"cwd":cwd,"limit":MIN_OUTPUT_BYTES})
        );
    }
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct Pending {
    started: tokio::sync::mpsc::UnboundedSender<CancellationToken>,
    dropped: Arc<AtomicBool>,
}

impl Tool for Pending {
    type Input = Empty;
    type Output = Value;
    fn name(&self) -> &str {
        "pending"
    }
    fn description(&self) -> &str {
        "Wait until cancelled"
    }
    async fn execute(&self, _: Empty, ctx: &Context) -> Result<Value, Failure> {
        let _guard = Dropped(self.dropped.clone());
        self.started.send(ctx.cancellation().clone()).unwrap();
        std::future::pending().await
    }
}

#[tokio::test]
async fn pre_cancelled_execution_never_starts_and_aborts_the_adapter() {
    let tool = Echo::new("echo");
    let calls = tool.calls.clone();
    let mut registry = Registry::default();
    registry.register(tool).unwrap();
    let ctx = context(MAX_OUTPUT_BYTES);
    ctx.cancellation().cancel();
    assert_eq!(
        registry
            .invoke("echo", &json!({"text":"yes"}), &ctx)
            .await
            .unwrap_err(),
        Error::Cancelled
    );
    let executor = NativeExecutor::new(&registry, ctx);
    let call = bee::llm::Call {
        id: "one".into(),
        name: "echo".into(),
        arguments: json!({"text":"yes"}),
    };
    assert!(matches!(
        executor.execute(&call).await,
        Err(bee::llm::Error::Cancelled)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_signals_active_work_and_drops_its_future() {
    for outer_drop in [false, true] {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let mut registry = Registry::default();
        registry
            .register(Pending {
                started: tx,
                dropped: dropped.clone(),
            })
            .unwrap();
        let ctx = context(MAX_OUTPUT_BYTES);
        let sibling = ctx.cancellation().child_token();
        let args = json!({});
        let mut execution = Box::pin(registry.invoke("pending", &args, &ctx));
        let signal = tokio::select! {
            result = &mut execution => panic!("execution finished early: {result:?}"),
            signal = rx.recv() => signal.unwrap(),
        };
        assert!(!signal.is_cancelled());
        if outer_drop {
            drop(execution);
            assert!(!ctx.cancellation().is_cancelled());
            assert!(!sibling.is_cancelled());
        } else {
            ctx.cancellation().cancel();
            assert_eq!(execution.await.unwrap_err(), Error::Cancelled);
        }
        assert!(signal.is_cancelled());
        assert!(dropped.load(Ordering::SeqCst));
    }
}
