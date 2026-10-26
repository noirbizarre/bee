//! Explicit file and bounded recursive directory deletion.

use super::{Context, Failure, Target, Tool, tree};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Delete files or explicitly requested directory trees.
pub struct Delete;

/// Exact target; recursive deletion requires explicit opt-in.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Absolute or session-relative target.
    #[schemars(length(min = 1))]
    pub path: String,
    /// Permit deleting a nonempty directory and all planned descendants.
    #[serde(default)]
    pub recursive: bool,
}

/// Committed entry count, including directories.
#[derive(Serialize)]
pub struct Output {
    /// Number of successfully deleted entries.
    pub entries_deleted: usize,
}

impl Tool for Delete {
    type Input = Input;
    type Output = Output;
    fn kind(&self) -> &'static str {
        "delete"
    }
    fn name(&self) -> &str {
        "delete"
    }
    fn description(&self) -> &str {
        "Delete an exact file or empty directory. Nonempty directories require recursive:true. Plans the complete tree before deletion; rejects symlinks, special files, nested mounts, cwd ancestors and oversized trees. Cancellation/failure can leave partial deletions; committed removals are not rolled back."
    }
    fn targets(&self, input: &Input, context: &Context) -> Vec<Target> {
        tree::scopes(context, &input.path, None, input.recursive, "delete")
    }
    async fn execute(&self, input: Input, context: &Context) -> Result<Output, Failure> {
        let manifest = tree::prepared(context, &input.path, None, input.recursive, "delete")?;
        Ok(Output {
            entries_deleted: manifest.delete(context).await?,
        })
    }
}
