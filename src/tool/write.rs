//! Complete UTF-8 file creation and explicit replacement.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Context, Failure, Tool, mutation};

/// Create files, replacing an existing regular file only with explicit opt-in.
pub struct Write;

/// Complete contents and an explicit overwrite policy.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Absolute path or path relative to the execution cwd; parents must exist.
    #[schemars(length(min = 1))]
    pub path: String,
    /// Complete UTF-8 contents, bounded by the execution byte budget; no NUL.
    pub content: String,
    /// Permit replacing an existing writable regular file (default false).
    #[serde(default)]
    pub overwrite: bool,
}

/// A bounded acknowledgement without file contents or paths.
#[derive(Serialize)]
pub struct Output {
    /// Total UTF-8 bytes published to the file.
    pub bytes_written: usize,
}

impl Tool for Write {
    fn permissions(&self, input: &Input) -> Vec<crate::permissions::Check> {
        vec![crate::permissions::Check::path(
            "edit",
            &input.path,
            crate::permissions::Effect::Ask,
        )]
    }
    fn kind(&self) -> &'static str {
        "edit"
    }
    type Input = Input;
    type Output = Output;
    fn target(&self, input: &Input) -> Option<super::Target> {
        Some(super::Target {
            path: input.path.clone().into(),
            directory: false,
            read: false,
        })
    }

    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write complete UTF-8 content to a file. Existing files require overwrite:true (default false). Parents must exist; final symlinks and nonregular/readonly files are rejected. Text must fit the execution byte budget and contain no NUL. Relative paths use cwd; not a sandbox. Staged publication avoids partial files."
    }

    async fn execute(&self, input: Input, context: &Context) -> Result<Output, Failure> {
        mutation::text(&input.content, context)?;
        let target = mutation::Target::new(&input.path, context)?;
        if target.exists() && !input.overwrite {
            return Err(mutation::failure(
                "Destination already exists; inspect it and set overwrite:true only if replacement is intended.",
            ));
        }
        // Patch capture is best-effort: overwriting binary/large files remains supported.
        let before = target
            .exists()
            .then(|| target.snapshot(context).ok())
            .flatten();
        context.emit(crate::presentation::Artifact::Reserve(
            crate::presentation::PATCH_BYTES + target.path.as_os_str().len() * 6 + 1024,
        ))?;
        let staged = mutation::Staged::new(&target, &input.content, context).await?;
        staged.publish(&target, None, context)?;
        mutation::receipt(
            context,
            if target.exists() { "modify" } else { "add" },
            &target.path,
            None,
            false,
            before.as_ref().map(|s| s.bytes.as_str()),
            Some(&input.content),
        )?;
        Ok(Output {
            bytes_written: input.content.len(),
        })
    }
}
