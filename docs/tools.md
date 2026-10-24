# Native tools

Bee's SDK-free `tool` module defines typed native operations, their advertised
schemas, and their execution context. `agent::NativeExecutor` connects a registry
to the existing bounded inference loop. Production ACP sessions do not register
or advertise native tools yet.

## Implement and register a tool

### Built-in file reading

Register `bee::tool::read::Read` with `registry.register(bee::tool::read::Read)?`.
Its `read` input requires `path`; optional `offset` is 1-based (default 1), and
`limit` is a positive line count (default 2000). Unknown fields are rejected.
An offset beyond EOF returns empty content. Zero or overflowing ranges fail.

Success data contains `content` formatted as `1: text\n`, `truncated`, and
`next_line` (null at EOF). LF and CRLF endings are normalized; an initial UTF-8
BOM is removed. Empty files have no lines; a final newline does not create a
phantom line. Truncation reports omitted content at either the requested line
limit or the serialized byte budget. Repeat with `offset: next_line` to continue.
If one line cannot fit, increase the execution budget; the tool never silently
splits a logical line. Even skipped lines have bounded buffers, so an oversized
line before the requested offset can also produce an `output_limit` failure.

Only regular UTF-8 text files are supported. Invalid UTF-8 and NUL bytes in
scanned content fail; unread portions are not prevalidated. Missing, inaccessible,
directory and special-file paths produce actionable tool errors. Reads use bounded
buffers and cooperative cancellation between chunks; filesystem syscalls can block.

Paths resolve relative to the execution cwd; absolute paths, parent traversal and
explicit symlinks are allowed, including targets outside cwd. This is **not a
sandbox**: metadata checks do not prevent concurrent filesystem changes. No file
contents or arbitrary OS error messages enter diagnostic or debug output.

### Custom tools

```rust
use bee::tool::{Context, Failure, Registry, Tool, MAX_OUTPUT_BYTES};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EchoInput {
    #[schemars(length(min = 1))]
    text: String,
}

struct Echo;

impl Tool for Echo {
    type Input = EchoInput;
    type Output = String;

    fn name(&self) -> &str { "echo" }
    fn description(&self) -> &str { "Echo nonempty text back to the model" }

    async fn execute(&self, input: EchoInput, _: &Context) -> Result<String, Failure> {
        Ok(input.text)
    }
}

# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), bee::tool::Error> {
let mut registry = Registry::default();
registry.register(Echo)?;
let cancellation = CancellationToken::new();
let context = Context::new("/session/project", cancellation, MAX_OUTPUT_BYTES)?;
let output = registry.invoke("echo", &serde_json::json!({"text":"hello"}), &context).await?;
assert_eq!(output.json, r#"{"data":"hello"}"#);

// Use the same registry and context to advertise and execute calls in agent::run.
let executor = bee::agent::NativeExecutor::new(&registry, context);
let definitions = executor.definitions();
// agent::run(&model, messages, &definitions, Some(&executor), &mut on_text).await?;
# Ok(())
# }
```

Registration accepts names matching `[A-Za-z0-9_-]{1,64}` and nonempty descriptions.
Duplicate names are rejected without replacing the original implementation.
Definitions are returned in name order. Metadata is captured on registration;
changing a returned definition clone does not change validation or dispatch.

## Validation and errors

Schemars generates a Draft 2020-12 object schema from `Tool::Input`. The registry
compiles it once and enforces it before deserializing each call. Local `$defs`
references work; unresolved external references fail registration without network
or file retrieval. Native tools are trusted code, including custom `JsonSchema`
implementations. There is no provider-specific schema rewriting.

Arguments must be JSON objects. Validation performs no coercion or insertion of
defaults; Serde applies declared defaults during decoding. Unknown fields follow
the advertised schema. Prefer `#[serde(deny_unknown_fields)]` for closed inputs.
JSON Schema `format` follows Draft 2020-12 annotation semantics, not additional
format assertions. Implement tool-specific semantic checks in `execute`, returning
`Failure::new(Code::InvalidArguments, "actionable guidance")` when appropriate.

Successful results are JSON envelopes containing `data`. Ordinary errors contain
an `error` object with stable `code` and `message` fields:

```json
{"error":{"code":"invalid_arguments","message":"Arguments do not match the input schema; correct the call."}}
```

Codes are `unknown_tool`, `invalid_arguments`, `execution`, and `output_limit`.
Tools can return a `Failure` with deliberate model-facing details. Argument and
schema validation errors never repeat input values. Diagnostic `Display`, `Debug`,
and source chains do not reveal tool result or failure contents. Tool authors must
avoid logging arguments, schema defaults, configuration, or arbitrary source errors.

The executor preserves call IDs and error flags through the loop. JSON envelopes
also preserve recognizable errors in Chat Completions and Responses, whose tool
result formats do not carry a separate error flag. Invalid arguments and execution
failures allow the model to correct its next call. Cancellation aborts the turn.
Whole-batch checks for unknown advertised names, duplicate call IDs, and budgets
still precede any execution; per-tool argument validation occurs during dispatch.

## Execution context and bounds

The caller supplies the session/project working directory, a cancellation token,
and a per-result byte budget. Use `context.cwd().join(relative_path)` for relative
paths. Bee neither changes process cwd nor canonicalizes the supplied path. This
base directory is not a sandbox, and this contract defines no permission framework.

`MAX_OUTPUT_BYTES` is the default ceiling of 1 MiB; pass it to `Context::new` for
the standard budget. Custom budgets range from `MIN_OUTPUT_BYTES` (256) through
that ceiling. Limits count UTF-8 bytes of the entire serialized envelope, including
JSON escaping and metadata, not characters or just the success data. The registry
bounds serialization and replaces oversized successes or errors with a complete
`output_limit` envelope. A serializer failure produces an `execution` envelope.
Implementations must also bound intermediate allocations and reads using
`context.output_limit()`; a final serialization bound cannot prevent those costs.

Pre-cancelled calls never execute. Active dispatch receives a child token, and
cancellation wins over a simultaneously ready result. Cancelling the caller's token
stops dispatch and returns `tool::Error::Cancelled` (`llm::Error::Cancelled` through
the adapter). Dropping dispatch signals its child token and drops the implementation
future without cancelling unrelated calls. Implementations must cooperate at await
points and must not detach ongoing work. Async cleanup is not guaranteed, and
external side effects cannot be rolled back. The ACP boundary retains ownership of
its existing cancellation lifecycle and terminal events.
