# Project instructions and Agent Skills

Bee supports [AGENTS.md](https://agents.md/) project instructions and the vendor-neutral
[Agent Skills format](https://agentskills.io/specification). These supplement Bee's built-in system guidance across
Chat Completions, Responses and Anthropic Messages. No Bee-specific skill file or activation tool is required.

## Instruction discovery and scope

For each session, Bee finds the nearest ancestor containing a `.git` directory or worktree marker file. That directory
is the project root; without a marker, the session's working directory is the root. Bee loads `AGENTS.md` from the root
through the session directory. It does not search above the project root. This traversal is independent of configuration
loading of `.config/bee.toml`, which still uses the launch directory only.
`.agents/` is Bee's canonical project agent-resource directory.
[Project MCP configuration](mcp.md) uses the same root boundary and a separate
`.agents/mcp.toml` source; `.agents/skills/` is reserved for standard Agent Skills.

`~/.agents/AGENTS.md` supplies additional user instructions alongside the built-in system prompt. Without a usable home
directory, user-level discovery is disabled; Windows falls back from absent or empty `HOME` to `USERPROFILE`.
Files are ordinary UTF-8 Markdown. Missing optional files are normal; unreadable,
oversized or invalid applicable instruction files fail the turn with a diagnostic explaining how to repair them.

Parent rules remain applicable. A closer `AGENTS.md` wins conflicts for its subtree; explicit user requests override
file-based guidance. Each nested source is labeled with its scope, so a package's rules are not presented as global
rules for its siblings. Instruction files at different scopes remain distinct even when they share content.

Before native `read`, `write`, `edit`, `delete`, `move`, `copy`, `list`, `glob` or `grep` operations, Bee checks the target's
applicable ancestor chain.
If new or changed guidance was absent when the model generated the calls, the entire batch returns correlated
`guidance_required` results without executing. The next request contains the guidance, and the model must reconsider
and issue fresh calls. If an earlier operation changes instructions, remaining calls are deferred before execution.
Deferred calls and extra model requests count against the normal execution limits.

Discovery operations check their explicit target directory, not every returned descendant. Directory mutations instead
prepare a complete bounded manifest and inspect all affected source directories and projected destination scopes before
side effects. Copied/moved instruction files are refreshed under their destination scopes after publication.
Arbitrary Bash commands and custom tools require the model to read applicable instructions before
working on new paths; shell commands cannot be reliably analyzed for all affected files. Embedders can opt custom tools
into structured-target handling explicitly. Content search checks its explicit directory root; matching snippets from a
`SKILL.md` file do not activate that skill.

Targets resolve through existing ancestors and symlinks. External files remain accessible through native tools, but Bee
does not import external project instruction trees automatically. An `AGENTS.md` symlink must resolve inside the project;
its containing directory still defines its scope. Checks are optimistic, like native file mutations, and do not prevent
concurrent filesystem replacement. Scope labels guide the model; they do not isolate its memory between subtrees.

## Skill discovery and precedence

Bee looks for immediate `<skill-name>/SKILL.md` children in these roots, highest precedence first:

1. `<project-root>/.agents/skills/`
2. Additional configured roots, in array order
3. `~/.agents/skills/`

```toml
[skills]
extra_roots = ["/opt/team/skills", "../shared-skills"]
```

Configure this table in the normal global, `BEE_CONFIG`, or launch-project configuration. Relative roots resolve against
the directory containing the file that supplied the winning array. For example, `../shared-skills` in
`<project>/.config/bee.toml` means `<project>/shared-skills`. There is no tilde or environment-variable expansion.
Higher-layer arrays replace lower-layer arrays; `extra_roots = []` clears inherited additional roots, retaining defaults.
Configured roots apply to the connection; project defaults follow each session's project root.

Entries are sorted, canonical source aliases are deduplicated, and the first valid skill with a given normalized name
wins. The final catalog is sorted by name. Skill directory symlinks are supported, including shared external sources;
the logical candidate directory name must match its metadata. Discovery is shallow and does not follow references to
find more skills. Nested `AGENTS.md` files do not implicitly introduce additional skill roots.

Malformed skills are skipped with actionable ACP warning notices, including safe source locations and YAML positions
when available. They do not reserve names: a valid lower-precedence skill can supply the same name. Missing defaults are
normal; inaccessible configured roots produce warnings. Invalid discovery configuration fails at launch. Parser values,
source excerpts, and instruction contents never appear in diagnostics or Debug output.
Diagnostics for configured roots identify the configuration file and `skills.extra_roots` entry index rather than
repeating the configured path value, including during activation.

## Format and progressive loading

```markdown
---
name: code-review
description: Review code for correctness and regressions when a review is requested.
metadata:
  version: "1.0"
---

Read the changes and supporting context. See references/checklist.md.
```

Frontmatter is real YAML, including multiline descriptions. Bee validates required `name` and `description`, directory
matching, Unicode-normalized lowercase names, hyphen rules and standard length limits. It also validates optional
`license`, `compatibility`, `allowed-tools` and string-to-string `metadata`, rejecting duplicate keys, unknown fields and
incorrect types. `allowed-tools` is accepted experimental metadata but does not grant permissions or register tools.
Compatibility metadata does not install dependencies or run setup commands.

Initially, the model sees names, descriptions and absolute `SKILL.md` locations, without skill bodies. A successful
native `read` of a cataloged file activates its complete validated snapshot, even when the displayed line window is
partial. Displayed content and activation use the same source revision. Failed reads and unrelated files named
`SKILL.md` do not activate skills. A new activation defers remaining batch calls so the model sees the skill before
acting on arguments generated without it.

Supporting files can have any layout. The existing `read` tool loads references and assets as needed, and `bash` can run
scripts. Relative resource paths in a skill refer to the skill directory, not the session cwd; the prompt explicitly
supplies that directory. Bee does not automatically execute bundled scripts.

Activated snapshots are committed with successful conversation history and survive whole-turn eviction, close/resume
and provider/model changes. Failed, refused, cancelled or stale turns do not commit new activations. Subsequent reads
can refresh a skill snapshot. Project instructions and the catalog are rediscovered each turn. Persistent guidance is
included in context estimates and is not silently evicted; an unfit baseline fails with a context-limit error.

## Bounds and embedding

Sources must be regular UTF-8 files without NUL bytes, at most 256 KiB each. YAML frontmatter is limited to 32 KiB.
Configuration accepts up to 64 additional roots, and discovery inspects at most 4096 immediate entries in total. Exceeding
that discovery ceiling omits the catalog with a warning rather than selecting a filesystem-order-dependent subset.
The composed persistent prompt is limited to 1 MiB. At most 16 detailed discovery warnings plus an omission summary are
sent per turn. Filesystem loops yield for cancellation between entries and sources; individual filesystem calls remain
synchronous and may block on slow filesystems.

The binary enables discovery by default. Low-level library runtimes preserve explicit caller ownership: use
`Runtime::with_instructions(Sources::new(home))` or `Sources::from_config` to enable discovery with injected environment
inputs. Low-level `Model` callers can supply additional instructions with `Model::with_instructions`.

See [ADR-016](adr/016-project-instructions-and-agent-skills.md) for the integration decisions.
