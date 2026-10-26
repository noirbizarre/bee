//! Same-filesystem, no-clobber file and directory moves.

use super::{Context, Failure, Target, Tool, tree};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Move a fully inspected source without replacing a destination.
pub struct Move;

/// Source and exact absent destination.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Absolute or session-relative source file or directory.
    #[schemars(length(min = 1))]
    pub source: String,
    /// Absent exact destination with an existing parent.
    #[schemars(length(min = 1))]
    pub destination: String,
}

/// Count affected by the atomic namespace change.
#[derive(Serialize)]
pub struct Output {
    /// Number of moved files and directories.
    pub entries_moved: usize,
}

impl Tool for Move {
    fn permissions(&self, input: &Input) -> Vec<crate::permissions::Check> {
        vec![
            crate::permissions::Check::path("edit", &input.source, crate::permissions::Effect::Ask),
            crate::permissions::Check::path(
                "edit",
                &input.destination,
                crate::permissions::Effect::Ask,
            ),
        ]
    }
    type Input = Input;
    type Output = Output;
    fn kind(&self) -> &'static str {
        "move"
    }
    fn name(&self) -> &str {
        "move"
    }
    fn description(&self) -> &str {
        "Move a file or directory to an absent exact destination on the same filesystem using no-clobber rename. Inspects at most 10000 entries/depth 64 first. Rejects symlinks, special files, nested mounts, cwd ancestors and self/descendant destinations. Cross-filesystem or unsupported exclusive rename fails; never falls back to copy/delete."
    }
    fn targets(&self, input: &Input, context: &Context) -> Vec<Target> {
        tree::scopes(
            context,
            &input.source,
            Some(&input.destination),
            true,
            "move",
        )
    }
    async fn execute(&self, input: Input, context: &Context) -> Result<Output, Failure> {
        let manifest = tree::prepared(
            context,
            &input.source,
            Some(&input.destination),
            true,
            "move",
        )?;
        Ok(Output {
            entries_moved: manifest.move_to(context).await?,
        })
    }
}
