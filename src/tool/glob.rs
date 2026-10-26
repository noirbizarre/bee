//! Ignore-aware recursive discovery without following directory symlinks.

use globset::GlobBuilder;
use ignore::WalkBuilder;
use schemars::JsonSchema;
use serde::Deserialize;

pub use super::discovery::{Entry, Kind, Output};
use super::{Code, Context, Failure, Tool, discovery};

/// Find files and symlink entries by a root-relative glob pattern.
pub struct Glob;

/// Explicit root, glob syntax and optional filtering overrides.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Case-sensitive root-relative glob; use ** for recursive matching.
    #[schemars(length(min = 1, max = 4096))]
    pub pattern: String,
    /// Directory relative to cwd, or an absolute directory.
    #[serde(default = "path")]
    #[schemars(length(min = 1))]
    pub path: String,
    /// Maximum entries; the serialized byte limit also applies.
    #[serde(default = "limit")]
    #[schemars(range(min = 1, max = 10000))]
    pub limit: usize,
    /// Traverse hidden names (ignore rules still apply).
    #[serde(default)]
    pub include_hidden: bool,
    /// Disable repository-local ignore rules (hidden filtering still applies).
    #[serde(default)]
    pub include_ignored: bool,
}

fn path() -> String {
    ".".into()
}
fn limit() -> usize {
    1000
}

impl Tool for Glob {
    fn kind(&self) -> &'static str {
        "search"
    }
    type Input = Input;
    type Output = Output;
    fn target(&self, input: &Input) -> Option<super::Target> {
        Some(super::Target {
            path: input.path.clone().into(),
            directory: true,
            read: false,
        })
    }
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find files and symlinks by a case-sensitive root-relative glob (*, ?, classes, braces, **; / separates components). Optional path defaults to cwd, limit to 1000 (max 10000). Hidden and locally ignored paths are excluded unless include_hidden/include_ignored is true. Results are lexical and byte-bounded with explicit truncation; maximum depth 64. Never follows nested directory symlinks. Absolute roots, .. and explicit symlink roots are allowed; not a sandbox."
    }
    async fn execute(&self, input: Input, context: &Context) -> Result<Output, Failure> {
        if input.pattern.is_empty() || input.pattern.len() > 4096 {
            return Err(Failure::new(
                Code::InvalidArguments,
                "Use a glob pattern between 1 and 4096 bytes.",
            ));
        }
        let matcher = GlobBuilder::new(&input.pattern)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .map_err(|_| {
                Failure::new(
                    Code::InvalidArguments,
                    "Invalid glob pattern; check brackets, braces and ** placement.",
                )
            })?
            .compile_matcher();
        let root = discovery::root(context, &input.path)?;
        let mut selection = discovery::Selection::new(context, input.limit)?;
        let walk = |directory: &std::path::Path| {
            let mut builder = WalkBuilder::new(directory);
            // Each iterator opens only one level: a deep walk must not make
            // walkdir buffer entire wide directories when its FD limit is hit.
            builder
                .follow_links(false)
                .hidden(!input.include_hidden)
                .git_global(false)
                .parents(!input.include_ignored)
                .ignore(!input.include_ignored)
                .git_ignore(!input.include_ignored)
                .git_exclude(!input.include_ignored)
                .require_git(false)
                .max_depth(Some(1));
            builder.build()
        };
        let mut stack = vec![walk(&root)];
        while let Some(current) = stack.last_mut() {
            tokio::task::yield_now().await;
            if context.cancellation().is_cancelled() {
                break;
            }
            let Some(entry) = current.next() else {
                stack.pop();
                continue;
            };
            let entry = entry.map_err(|_| {
                Failure::new(
                    Code::Execution,
                    "Directory traversal failed; check access permissions and local ignore files.",
                )
            })?;
            if entry.error().is_some() {
                return Err(Failure::new(
                    Code::Execution,
                    "Invalid or unreadable ignore file; correct repository-local ignore rules.",
                ));
            }
            if entry.depth() == 0 {
                continue;
            }
            let Some(kind) = entry.file_type() else {
                continue;
            };
            let path = discovery::relative(entry.path(), &root)?;
            if kind.is_dir() {
                if stack.len() >= 64 {
                    selection.truncated = true;
                } else {
                    stack.push(walk(entry.path()));
                }
                continue;
            }
            if matcher.is_match(&path) && (kind.is_file() || kind.is_symlink()) {
                selection.insert(path, kind.into());
            }
        }
        Ok(selection.finish())
    }
}
