# Tools

Bee's SDK-free `tool` module defines typed native operations, their advertised
schemas, and their execution context. `agent::NativeExecutor` connects a registry
to the bounded inference loop. Normal ACP sessions advertise and execute the
shipped `bash`, `read`, `glob`, `grep`, `list`, `write`, and `edit` tools. Their execution base is the session cwd,
not the launch directory used to load configuration.

## Execution plans

Every ACP runtime also offers the turn-owned `update_plan` operation. Models use
it for substantive multi-step work and send complete ordered snapshots:

```json
{"entries":[{"content":"Inspect the implementation","priority":"medium","status":"in_progress"},
            {"content":"Implement and verify","priority":"high","status":"pending"}]}
```

Each entry requires `content`, `priority` (`high`, `medium`, `low`) and `status`
(`pending`, `in_progress`, `completed`, `cancelled`). Descriptions must be nonblank
and at most 4096 UTF-8 bytes. At most 128 entries and 64 KiB of serialized arguments
are accepted; extra fields are rejected. Invalid calls return `invalid_arguments`
tool errors without changing the plan. Accepted calls return `{"updated":true}`.
`update_plan` is reserved and cannot be replaced by an injected native tool.

The operation creates one plan per turn, with a runtime-owned stable `planId`.
Calls replace the whole list, including descriptions, priorities, statuses and
ordering; identical snapshots produce no additional notification. `{"entries":[]}`
explicitly creates an empty plan or clears an existing list. Simple answers need
no plan, and Bee never synthesizes one from assistant text or tool activity.

Clients receive separate standard ACP v2 `plan_update` notifications, rather than
assistant-message chunks or tool-lifecycle events. Plan entries have no standard
item IDs: their order is the array order, and clients replace by `planId`.
At every terminal stop, including normal completion, Bee preserves completed and
cancelled entries and changes unfinished entries to `cancelled` before idle.
Only explicit model updates mark work completed. Recoverable tool errors do not
end the plan. See [plan retention and replay](sessions.md#execution-plan-retention).

## ACP user interactions

Before each validated native `bash` call, Bee sends `session/request_permission`
with the command, absolute session cwd and originating tool-call ID. Options are
`allow_once` and `reject_once`. Only the exact offered allow ID permits execution;
rejection, unknown IDs/outcomes, cancellation, malformed replies, request errors and
timeouts execute nothing. The client decides policy and can answer automatically.
Bee does not classify commands or cache approval. Command timeout starts after approval.
Other native tools keep their existing contracts; client-hosted MCP has its own policy.

A client advertising `capabilities.elicitation.form` receives an additional ACP tool,
`ask_user`. Input is `{"question":"Which environment should I use?"}`; the question
must be nonempty and at most 4096 UTF-8 bytes. Bee requests a form with one required
string, `answer`, correlated with the session and model tool call. Valid accepted
answers become `{"data":{"answer":"staging"}}`. Answers must be nonempty, at most
4096 UTF-8 bytes, and fit the complete tool-output budget; extra fields are rejected.
Decline/cancel/unknown actions, errors and invalid answers become bounded `interaction`
tool errors. This tool is for task information, not credentials. `ask_user` is reserved
in ACP runtimes and cannot be replaced by an injected native tool.

Form and URL capabilities are negotiated independently. Missing, null or empty
elicitation capabilities enable neither. URL support enables prompt-time
[provider browser login](index.md#acp-browser-sign-in), not a model-facing URL tool.
Permission requests do not require an elicitation capability.

Permission/form waits have five-minute deadlines; browser login has a five-minute
overall deadline. Bee reports `requires_action` while blocked on user input and
`running` when progress resumes. An interaction-level denial/cancel is a recoverable
tool error; session cancellation or close aborts the whole turn. Pending requests and
listeners are dropped before the single terminal idle; direct close responds afterward.
Late replies cannot authorize work or revive an ended turn. Active resume snapshots
`requires_action` when appropriate. ACP interactions do not implement MCP
`input_required` or task continuation.

## Client-hosted MCP tools

Bee advertises `session.mcp.acp`. The client hosts or proxies MCP servers and declares their opaque server IDs in
`session/new` or `session/resume`. Bee rejects HTTP, stdio and unknown server transports; it does not launch MCP
processes or maintain MCP configuration. A successful resume replaces the session's attachments, including clearing
them when the list is empty. Closing removes attachments; retained conversation context does not grant access to old
servers. Failed resume requests leave attachments unchanged.

At the start of each prompt, Bee snapshots its session attachments and discovers the current catalog with `tools/list`,
including pagination. Discovery runs outside ACP dispatch so cancellation remains responsive. Discovery failure fails
that turn before contacting the model, rather than silently offering a partial catalog. A later prompt can retry.
Only the session's declared servers are considered, even if another session attached other servers on the connection.

MCP tools appear alongside native tools under portable names: `mcp_` followed by 56 hexadecimal characters from a
SHA-256 of the JSON-encoded server-ID/tool-name pair. Names are stable under discovery reordering; changed server IDs
produce changed aliases. Descriptions include the original server and tool names. An explicit routing table maps each
alias to its original server ID and tool name, so identical tool names on different servers do not shadow each other.
Any alias collision, including with a native tool, fails discovery.

Object input schemas are compiled offline using JSON Schema Draft 2020-12. Local references are supported; unresolved
external references and malformed schemas fail discovery. Arguments are validated before invocation and forwarded as
JSON without stringifying their fields. Calls use `mcp/message` with `tools/call`, fresh logical request IDs, and the
required MCP `2026-07-28` version and empty client capabilities in inner `params._meta`. There is no MCP `initialize`.

Completed results retain the entire JSON envelope: `{"result": ...}` or `{"error": ...}`, plus carrier metadata if
present. Content blocks, structured content (including null or scalar values), result metadata and inner error
code/message/data/extensions are preserved. This JSON becomes the provider's native correlated tool-result string;
Anthropic additionally receives `is_error`. Non-text MCP content is preserved as JSON, not rendered as model-native
images or resources.

**Permissions belong to the client/server.** Bee does not request approval before each call or interpret annotations
as authorization. Inner MCP errors, permission denials, tools that disappear after discovery and `isError: true`
results become ordinary tool errors for model recovery. Outer ACP failures, malformed results and timeouts become
sanitized local error envelopes. Interactive `input_required`, tasks and unknown result types are explicit unsupported
outcomes; Bee does not replay their state or fulfill input requests.

The initial bounds are 32 servers per session (IDs and names up to 1024 bytes), 64 discovery pages per server,
256 MCP tools, 1 MiB of aggregate serialized discovery results, depth 32 for discovery JSON, tool names up to 1024
bytes, and cursors up to 4096 bytes. Discovery has a total 30-second deadline; each call has a 120-second deadline.
These checks apply after SDK decoding; they are not transport allocation limits or a schema-compilation sandbox.
Results exceeding the 1 MiB tool-output budget are replaced by a complete error envelope, never truncated JSON.
Calls also share the normal turn step/call budgets and context accounting with native tools.

Cancellation or close drops pending discovery/call futures and triggers SDK request cancellation. Remote cancellation
is cooperative and cannot undo side effects. Bee never automatically retries an MCP call. Cancelled and failed turns
are excluded from future model context under the normal turn contract.

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

### Built-in content search

Register `bee::tool::grep::Grep` to embed repository-aware content search. Built-in
ACP sessions already include it. Search is entirely in Rust: no `rg` executable,
shell permission request, or subprocess is involved.

For example, invoke `grep` with:

```json
{"pattern":"fn\\s+main","path":"src","include":["**/*.rs"],"exclude":["**/generated/**"],"limit":100}
```

`pattern` is a nonempty Unicode Rust regex, at most 4096 UTF-8 bytes. Matching is
line-oriented: lookaround, backreferences, and explicit newline/NUL matches are
unsupported. Regex compilation and DFA caches each have an approximate 1 MiB
limit. Invalid or excessive patterns return sanitized `invalid_arguments` errors.
`case_insensitive` defaults to false; enabling it applies Unicode case folding to
content, independently of path filters. Inline regex flags are supported.

`path` defaults to `.` and must identify a directory. Use glob filters to narrow
search to an individual file. `include` and `exclude` default to empty arrays;
each accepts at most 100 nonempty patterns of at most 4096 UTF-8 bytes. They use
the same root-relative, case-sensitive syntax as `glob`. A file must match any
include (or none are required when the array is empty), and no exclude. Excludes
win. Filters select files without pruning ancestors or overriding ignore rules.
`include_hidden` and `include_ignored` default to false, independently, with the
same repository-local rules and disabled global Git ignores as `glob`.

Success data is structured rather than formatted as terminal output:

```json
{"data":{"matches":[{"path":"main.rs","line_number":12,"line":"fn main() {"}],"truncated":false,"skipped_binary_files":0}}
```

Each matching logical line appears once, regardless of occurrence count. Paths
are UTF-8, root-relative and use `/`; line numbers are 1-based. LF/CRLF endings
and an initial UTF-8 BOM are removed before matching/display. Empty files have
no lines; a final newline creates no extra line. No multiline search is offered.

`limit` defaults to 1000 and accepts 1–10000 matching lines. Results form a prefix
sorted by path and numeric line number, independent of filesystem enumeration.
The complete serialized byte budget, including JSON escaping, may reduce that
prefix further. Omitted matches or reaching traversal depth 64 set `truncated`.
A first match too large to represent can produce empty, truncated results. Narrow
the root/filters or increase the budget/count to inspect more matches.

Only regular UTF-8 text files are searched. NUL or invalid UTF-8 marks a file as
binary: its matches are discarded and `skipped_binary_files` increases. Files
are scanned through EOF even after the selection fills; bounded per-file staging
prevents late binary markers from leaking earlier matches or evicting valid text
results. A logical line exceeding `context.output_limit()` instead fails the
search with `output_limit`, even when it would not match. A line-size failure can
precede detection of later binary content. Lines are never silently split.

Explicit directory symlink roots are followed; nested symlinks (including file
links and dangling links) and special files are skipped. Missing/non-directory
roots, inaccessible files/directories, malformed ignore rules, I/O errors and
non-UTF-8 names produce actionable errors. Path resolution has the same
non-sandbox policy as other native tools, including absolute paths and `..`.

Traversal and bounded chunk scanning yield cooperatively for cancellation,
including inside a large nonmatching file. Cancellation aborts dispatch instead
of returning partial success; dropping dispatch stops its scanner without a
detached task. Filesystem syscalls can block and concurrent replacements can
race with metadata checks. Output and intermediate buffers are bounded, but
total scan time is not; deterministic selection and binary checks scan every
eligible file. Scoped guidance checks the explicit root, and snippets from a
`SKILL.md` file do not activate that skill.

#### Embedded crates and licenses

The lockfile resolves these ripgrep ecosystem crates, all under
`Unlicense OR MIT`, compatible with Bee's MIT license:

| Crate | Version | Purpose |
| --- | --- | --- |
| `grep-regex` | 0.1.14 | Unicode Rust regex compilation and matching |
| `grep-matcher` | 0.1.9 | Supported matcher interface |
| `ignore` | 0.4.33 | Repository-local traversal and ignore rules |
| `globset` | 0.4.20 | Portable include/exclude filters |

Their package metadata and MIT license texts were checked; the MIT option allows
embedding with preservation of the dependency copyright and permission notices
when distributing copies or substantial portions. Bee uses its own bounded,
cooperative reader around the supported matcher API rather than the synchronous
`grep-searcher` full-reader loop, so nonmatching reads also provide cancellation
checkpoints. No missing regex API requires another engine or executable fallback.

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
format assertions. Implement reusable authorization-time semantic checks in `Tool::preflight`
and enforce them in `execute`, returning
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
