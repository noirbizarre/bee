//! Staged file and bounded recursive directory copying.

use super::{Context, Failure, Target, Tool, tree};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Copy files or explicitly requested directory trees to an absent exact path.
pub struct Copy;

/// Source and exact destination, without implicit overwrite.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Absolute or session-relative source.
    #[schemars(length(min = 1))]
    pub source: String,
    /// Absent exact destination with an existing parent directory.
    #[schemars(length(min = 1))]
    pub destination: String,
    /// Permit recursively copying a directory tree.
    #[serde(default)]
    pub recursive: bool,
}

/// Count published by one staged operation.
#[derive(Serialize)]
pub struct Output {
    /// Number of copied files and directories.
    pub entries_copied: usize,
}

impl Tool for Copy {
    type Input = Input;
    type Output = Output;
    fn kind(&self) -> &'static str {
        "edit"
    }
    fn name(&self) -> &str {
        "copy"
    }
    fn description(&self) -> &str {
        "Copy a regular file to an absent exact destination; directories require recursive:true. Parents must exist. Rejects symlinks, special files and nested mounts. At most 10000 entries, depth 64 and 64 MiB total file bytes. Stages completely before no-clobber publication; creates independent files, without ownership/ACL/xattr/timestamp preservation."
    }
    fn targets(&self, input: &Input, context: &Context) -> Vec<Target> {
        tree::scopes(
            context,
            &input.source,
            Some(&input.destination),
            input.recursive,
            "copy",
        )
    }
    async fn execute(&self, input: Input, context: &Context) -> Result<Output, Failure> {
        let manifest = tree::prepared(
            context,
            &input.source,
            Some(&input.destination),
            input.recursive,
            "copy",
        )?;
        Ok(Output {
            entries_copied: manifest.copy(context).await?,
        })
    }
}
