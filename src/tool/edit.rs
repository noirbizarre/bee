//! Literal, unambiguous UTF-8 replacement with optimistic conflict detection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Code, Context, Failure, Tool, mutation};

/// Replace exactly one literal text fragment in an existing file.
pub struct Edit;

/// Exact text replacement; line endings are significant.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Absolute path or path relative to the execution cwd.
    #[schemars(length(min = 1))]
    pub path: String,
    /// Nonempty exact fragment that must occur at exactly one start position.
    #[schemars(length(min = 1))]
    pub old_text: String,
    /// Replacement text; empty text deletes the matched fragment.
    pub new_text: String,
}

/// A bounded acknowledgement of the resulting file size.
#[derive(Serialize)]
pub struct Output {
    /// Entire resulting file's UTF-8 byte length, or zero for an unchanged no-op.
    pub bytes_written: usize,
}

impl Tool for Edit {
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
        "edit"
    }

    fn description(&self) -> &str {
        "Replace exactly one literal old_text with new_text in an existing UTF-8 file. Empty or missing/ambiguous old_text fails; overlaps count as ambiguous. Preserve untouched bytes; CRLF must match exactly. Source, text and result must fit the execution byte budget; no NUL. Reject final symlinks/readonly/nonregular files. Check for changes during this call, not since earlier reads; optimistic, not atomic compare-and-swap. Relative paths use cwd; not a sandbox."
    }

    async fn execute(&self, input: Input, context: &Context) -> Result<Output, Failure> {
        mutation::text(&input.old_text, context)?;
        mutation::text(&input.new_text, context)?;
        if input.old_text.is_empty() {
            return Err(Failure::new(
                Code::InvalidArguments,
                "Use nonempty old_text to identify a unique fragment.",
            ));
        }
        let target = mutation::Target::new(&input.path, context)?;
        if !target.exists() {
            return Err(mutation::failure(
                "File does not exist; check the path or use write to create it.",
            ));
        }
        tokio::task::yield_now().await;
        let source = target.snapshot(context)?;
        let start = source.bytes.find(&input.old_text).ok_or_else(|| mutation::failure(
            "old_text was not found; read the file again and match exact bytes, including CRLF line endings.",
        ))?;
        // Advance one character, not the match length, so overlapping candidates remain ambiguous.
        let next = start
            + source.bytes[start..]
                .chars()
                .next()
                .expect("nonempty match")
                .len_utf8();
        if source.bytes[next..].contains(&input.old_text) {
            return Err(mutation::failure(
                "old_text matches multiple locations; include more surrounding text to identify one unique fragment.",
            ));
        }
        if input.old_text == input.new_text {
            target.validate(Some(&source), context)?;
            return Ok(Output { bytes_written: 0 });
        }
        let size = source.bytes.len().checked_sub(input.old_text.len())
            .and_then(|size| size.checked_add(input.new_text.len()))
            .filter(|&size| size <= context.output_limit())
            .ok_or_else(|| Failure::new(Code::InvalidArguments, "Result exceeds the execution byte budget; use a smaller replacement or increase the budget."))?;
        let mut result = String::with_capacity(size);
        result.push_str(&source.bytes[..start]);
        result.push_str(&input.new_text);
        result.push_str(&source.bytes[start + input.old_text.len()..]);
        let staged = mutation::Staged::new(&target, &result, context).await?;
        context.emit(crate::presentation::Artifact::Reserve(
            crate::presentation::PATCH_BYTES + target.path.as_os_str().len() * 6 + 1024,
        ))?;
        staged.publish(&target, Some(&source), context)?;
        mutation::receipt(
            context,
            "modify",
            &target.path,
            None,
            false,
            Some(&source.bytes),
            Some(&result),
        )?;
        Ok(Output {
            bytes_written: size,
        })
    }
}
