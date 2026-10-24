# Native tools

Bee's SDK-free `tool` module defines typed native operations, their advertised
schemas, and their execution context. `agent::NativeExecutor` connects a registry
to the bounded inference loop. Normal ACP sessions advertise and execute the
shipped `bash`, `read`, `glob`, `list`, `write`, and `edit` tools. Their execution base is the session cwd,
not the launch directory used to load configuration.

## Implement and register a tool

### Built-in Bash execution

`bash` runs project commands such as tests, builds, and formatters. Input requires
nonempty `command`; optional positive `timeout_ms` and `output_limit` select limits
within the host's ceilings. Unknown fields and NUL in commands are rejected.
For example, `{"command":"cargo test","timeout_ms":60000}` runs tests in the
session directory without changing Bee's own working directory.

Bee starts `bash --noprofile --norc -c <command>` with no interactive input or PTY.
Stdin is EOF, stdout and stderr are separately captured, and `BASH_ENV` is removed
to prevent noninteractive startup-script injection. The remaining environment is
inherited. Bash must be installed, including on Windows; Bee does not substitute
another shell or translate command syntax.

Configure policy in the normal layered launch-time configuration:

```toml
[tools.bash]
shell = "bash"                  # executable on PATH, or an absolute executable path
timeout_ms = 120000              # default deadline, milliseconds
max_timeout_ms = 600000          # per-call ceiling
output_limit = 65536             # default complete serialized result budget
max_output_limit = 1048576       # per-call ceiling, at most 1 MiB
```

These are the defaults. Defaults must not exceed their ceilings; output budgets
must be between 256 and 1048576 bytes. Call overrides can increase defaults up to
the ceilings or request smaller limits. The execution context's output budget is
an additional ceiling. Invalid policy fails startup with `bee::bash::setting`,
identifying the setting and source file without exposing its value. Policy is
loaded once at launch, while commands use the session cwd.

Completed commands return structured data, for example:

```json
{"data":{"stdout":"ok\n","stderr":"","exit_code":0,"signal":null,"timed_out":false,"stdout_truncated":false,"stderr_truncated":false}}
```

Nonzero exits retain streams and actual exit codes as normal structured results;
a missing command normally returns Bash's exit 127 and explanatory stderr. Failure
to launch Bash or access the working directory is instead an actionable tool
error. Unix signal termination has a null `exit_code` and a numeric `signal`;
other platforms return a null signal.

Both pipes drain concurrently with bounded storage. Capture retains prefixes,
decodes bytes as lossy UTF-8, and marks each truncated stream. Metadata and JSON
escaping count against the complete result budget. Remaining capacity is divided
equally between streams with a conservative escaping allowance, so retained text
can be substantially smaller than the byte budget. Excess output is discarded
while execution continues under its timeout. Binary output can contain replacement
characters; this tool does not provide byte-exact binary capture.

A deadline terminates the command and returns `timed_out: true`, partial captured
output, and the available termination status. Both truncation flags are set when
timeout prevents complete capture. Cancellation aborts the turn using Bee's normal
cancelled-turn contract, rather than returning a model-facing command result.

On Unix, commands run in a new process group. Timeout, cancellation, future drop,
and foreground-shell completion kill remaining processes in that group. Descendants
that deliberately move to another group/session can escape this cleanup; the
deadline still bounds waiting on inherited pipes. On other platforms, Bee terminates
the direct shell process, without a descendant-tree guarantee. Normal and timeout
paths await the shell for reaping; immediate future drop initiates termination
synchronously and uses Tokio's best-effort background reaping.

Commands execute with Bee's **host user privileges**. A working directory is a
starting point, **not a sandbox**: commands can access files outside cwd, network
resources, and inherited environment credentials. Captured output is sent to the
model. Cancellation and timeout cannot undo filesystem or network side effects.

Embedders can register `bee::tool::bash::Bash::default()` or construct `Bash::new`
with `Settings::from_config`. `tool::builtins_with_bash(settings)` composes the
complete shipped catalog with that policy for `acp::Runtime::new`.

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

### Built-in file writing and editing

Register `bee::tool::write::Write` and `bee::tool::edit::Edit` alongside `Read`:

```rust
# fn main() -> Result<(), bee::tool::Error> {
let mut registry = bee::tool::Registry::default();
registry.register(bee::tool::write::Write)?;
registry.register(bee::tool::edit::Edit)?;
# Ok(())
# }
```

`write` requires `path` and complete UTF-8 `content`. Optional `overwrite` defaults
to false: existing files fail unless `overwrite: true` explicitly authorizes
replacement. For example, `{"path":"notes.txt","content":"hello\n"}` creates a
file; adding `"overwrite":true` permits replacing its complete contents. An
initially absent target is always published without overwriting a competing
creator, even when overwrite was requested. Empty files are supported.

`edit` requires `path`, nonempty `old_text`, and `new_text`. For example,
`{"path":"notes.txt","old_text":"hello","new_text":"goodbye"}` replaces one
literal fragment. Exactly one matching start position is required: missing,
multiple, and overlapping matches fail without guessing. Thus `old_text: "aa"`
is ambiguous in `"aaa"`. Include surrounding text to disambiguate. Empty
`new_text` deletes the match. Identical old/new text is a validated no-op.

Both reject unknown fields. Success data is `{"bytes_written":123}`, counting
the entire published file's UTF-8 bytes, or zero for an edit no-op. No contents or
paths are echoed. Each text argument, edit source, and resulting file must fit
`context.output_limit()` in UTF-8 bytes, independently of JSON escaping. Source
reads and intermediate buffers are bounded; several bounded buffers may coexist.
Oversized/NUL arguments and oversized edit results return `invalid_arguments`;
unsupported/oversized source files and filesystem/conflict errors return `execution`.
Neither tool supports NUL content; edit also rejects invalid UTF-8 source files.

Edits preserve untouched bytes, including BOM, mixed LF/CRLF endings, and missing
final newlines. Matching does not normalize line endings: include `\r\n` when
matching a CRLF boundary. The numbered, normalized text returned by `read` is a
presentation, not a byte-for-byte source fragment.

Paths follow the execution-cwd policy described above, including absolute paths
and parent traversal. Parents must already exist; neither tool creates directories.
Final-component symlinks (including dangling links), readonly files, directories,
and special files are rejected. Parent-directory symlinks are allowed. Replacements
preserve Unix permission bits; new files use tempfile defaults (0600 on Unix,
platform defaults on Windows). Ownership, ACLs, extended attributes, Windows
attributes, and hardlink relationships are not preserved. Updating one hardlink
path replaces that entry; other links retain the original file.

Content is staged in the same directory, synchronized, and then published as a
complete file. Errors before publication leave the original intact, with staging
cleaned up on ordinary failures or future drop. No-clobber publication never
overwrites an existing entry, but on some platforms an extra staging hardlink can
remain if unlinking it fails or publication is interrupted. Atomic visibility is
not a power-loss durability guarantee: the parent directory is not synchronized.

Edit takes a bounded source snapshot, then compares bytes and available metadata
again immediately before publication. Changes detected **during the invocation**,
including removal and detectable replacement, abort the edit. Earlier reads are
not tracked. This is optimistic detection, **not atomic compare-and-swap**: changes
can race with the final check; same-content replacements may evade portable
metadata checks. Parent redirection and hostile filesystem races are not prevented.

Reads/staging cooperate with cancellation at checkpoints; synchronous filesystem
calls, including final validation, can block. Cancellation before publication leaves
the target intact. Cancellation after publication can report a cancelled invocation
even though a complete file change committed; it cannot roll back that change.

### Built-in discovery

Register `bee::tool::glob::Glob` and `bee::tool::list::List` alongside `Read`.
For example, invoke `glob` with `{"pattern":"**/*.rs"}`, then `list` with
`{"path":"src"}`, and `read` with a discovered file path. All three use the
same context and can be advertised through `NativeExecutor`.

`glob` requires a root-relative, case-sensitive `pattern` (up to 4096 bytes).
`*` matches within a component, `?` one character, `[ab]` a character class,
`{rs,toml}` alternatives, and `**` recursive components. Use `/` as a separator
on every platform; backslash escapes metacharacters. For example, `*.rs` matches
root files while `**/*.rs` also matches nested files. Malformed patterns fail.
Only regular files and symlink entries are returned, not directories or devices.

Optional `path` defaults to cwd; `limit` defaults to 1000 and accepts 1–10000.
`include_hidden` and `include_ignored` both default to false and are independent:
including dotfiles does not disable ignore rules. Rules from `.ignore`, nested
and parent `.gitignore` files, and repository-local `.git/info/exclude` apply,
including negation and rules outside a Git checkout. Global user ignore files
are disabled for reproducibility. `include_ignored` disables all these local
ignore sources. Hidden directories are not traversed unless requested.

`list` accepts optional `path` and `limit` with the same defaults and bounds.
It returns immediate entries of every kind, including hidden and ignored names,
without recursion or ignore filtering. Entry kinds are `file`, `directory`,
`symlink`, and `other`.

Both return `entries` containing root-relative UTF-8 `path` and `kind`, sorted
lexically using `/` separators, plus `truncated`. Count and complete serialized
byte budgets select a lexical prefix independently of enumeration order. A
single oversized entry can yield an empty truncated result. Discover a smaller
subdirectory or increase the budget/count to inspect more entries. The tools
scan all eligible entries to select this deterministic prefix, retaining only
bounded result storage. Glob stops descending at depth 64 and reports truncation
when a directory reaches that boundary, even if it might be empty.

Explicit symlink roots are followed; nested symlink entries (including dangling
links) are reported without traversal, preventing recursive symlink cycles.
Missing/non-directory roots, inaccessible descendants, malformed ignore rules,
I/O failures and non-UTF-8 names produce actionable errors rather than silently
incomplete success. Filesystem changes can still race with enumeration. Path
resolution and cancellation have the same non-sandbox limitations as `read`.

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

### ACP runtime composition

`tool::builtins()` compiles a registry containing all shipped tools; `Registry::default()`
remains empty. Embedders can extend either registry and supply the complete catalog:

```rust
# fn main() -> bee::Result<()> {
let tools = bee::tool::builtins()?;
let limits = bee::agent::Limits::new(8, 32)?;
let runtime = bee::acp::Runtime::new(limits, tools);
let agent = bee::acp::agent_with_runtime(bee::provider::Providers::default(), runtime);
// Connect the agent to an ACP client transport.
# let _ = agent;
# Ok(())
# }
```

`Runtime::builtins(limits)` is the shorthand for a shipped catalog with explicit
budgets. `serve_with_runtime` and `serve_stdio_with_runtime` accept the same runtime.
Supplied catalogs replace the default catalog rather than implicitly merging with it.
The runtime is shared by sessions; each turn receives its own directory and cancellation context.

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
The final allowed model step cannot execute a tool batch, since its results would
have no subsequent model request. See [turn execution limits](inference.md#turn-execution-limits).

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
