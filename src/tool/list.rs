//! Immediate, unfiltered directory inspection.

use schemars::JsonSchema;
use serde::Deserialize;

pub use super::discovery::{Entry, Kind, Output};
use super::{Context, Failure, Tool, discovery, fs};

/// List all immediate entry types, including hidden and ignored names.
pub struct List;

/// Directory root and maximum result count.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Directory relative to cwd, or an absolute directory.
    #[serde(default = "path")]
    #[schemars(length(min = 1))]
    pub path: String,
    /// Maximum entries; the serialized byte limit also applies.
    #[serde(default = "limit")]
    #[schemars(range(min = 1, max = 10000))]
    pub limit: usize,
}

fn path() -> String {
    ".".into()
}
fn limit() -> usize {
    1000
}

impl Tool for List {
    type Input = Input;
    type Output = Output;
    fn name(&self) -> &str {
        "list"
    }
    fn description(&self) -> &str {
        "List immediate directory entries, including hidden and ignored names, in lexical order with file/directory/symlink kinds. Optional path defaults to cwd, limit to 1000 (max 10000). Results obey the byte budget and report truncation. Explicit directory symlinks, absolute paths and .. are allowed; not a sandbox. Never recurses."
    }
    async fn execute(&self, input: Input, context: &Context) -> Result<Output, Failure> {
        let root = discovery::root(context, &input.path)?;
        let mut selection = discovery::Selection::new(context, input.limit)?;
        for entry in std::fs::read_dir(&root).map_err(fs::io)? {
            tokio::task::yield_now().await;
            if context.cancellation().is_cancelled() {
                break;
            }
            let entry = entry.map_err(fs::io)?;
            let path = discovery::relative(&entry.path(), &root)?;
            selection.insert(path, entry.file_type().map_err(fs::io)?.into());
        }
        Ok(selection.finish())
    }
}
