//! Bounded, line-oriented UTF-8 file reading.

use std::fs::File;
use std::io::{BufRead, BufReader};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Code, Context, Failure, Tool, fs};

/// Read a regular text file, following an explicitly requested symlink.
pub struct Read;

/// A 1-based line window; paths resolve against the execution base.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Absolute path or path relative to the execution base.
    #[schemars(length(min = 1))]
    pub path: String,
    /// First line to return, starting at one.
    #[serde(default = "offset")]
    #[schemars(range(min = 1))]
    pub offset: usize,
    /// Maximum number of lines to return.
    #[serde(default = "limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
}

fn offset() -> usize {
    1
}
fn limit() -> usize {
    2000
}

/// Numbered content with an explicit continuation when more data exists.
#[derive(Serialize)]
pub struct Output {
    /// Text formatted as `line: content`, with normalized line endings.
    pub content: String,
    /// Whether the requested window or byte budget omitted content.
    pub truncated: bool,
    /// First omitted line, or null at EOF.
    pub next_line: Option<usize>,
}

impl Tool for Read {
    fn permissions(&self, input: &Input) -> Vec<crate::permissions::Check> {
        vec![crate::permissions::Check::path(
            "read",
            &input.path,
            crate::permissions::Effect::Allow,
        )]
    }
    fn kind(&self) -> &'static str {
        "read"
    }
    type Input = Input;
    type Output = Output;

    fn name(&self) -> &str {
        "read"
    }
    fn target(&self, input: &Input) -> Option<super::Target> {
        Some(super::Target {
            path: input.path.clone().into(),
            directory: false,
            read: true,
        })
    }
    fn description(&self) -> &str {
        "Read a UTF-8 regular file with 1-based offset and limit (defaults 1 and 2000). Returns numbered lines and explicit truncation. Relative paths use cwd; absolute paths, .. and explicit symlinks are allowed: not a sandbox. Binary/NUL content and lines exceeding the byte budget are unsupported."
    }

    async fn execute(&self, input: Input, context: &Context) -> Result<Output, Failure> {
        if input.offset == 0 || input.limit == 0 || input.offset.checked_add(input.limit).is_none()
        {
            return Err(Failure::new(
                Code::InvalidArguments,
                "Use positive offset and limit whose sum fits a line number.",
            ));
        }
        let path = fs::path(context, &input.path)?;
        // Inspect before opening so FIFOs/devices cannot hang an ordinary read.
        if !std::fs::metadata(&path).map_err(fs::io)?.is_file() {
            return Err(Failure::new(
                Code::Execution,
                "Expected a regular file; use list for directories.",
            ));
        }
        // Skill activation and displayed content must refer to one immutable revision.
        let mut reader: Box<dyn BufRead + Send> = if let Some(snapshot) = &context.read_snapshot {
            Box::new(std::io::Cursor::new(snapshot.as_bytes().to_vec()))
        } else {
            Box::new(BufReader::with_capacity(
                8192,
                File::open(path).map_err(fs::io)?,
            ))
        };
        let mut result = Output {
            content: String::new(),
            truncated: false,
            next_line: None,
        };
        // Reserve worst-case metadata/line-number overhead, including the success envelope.
        let budget = context.output_limit().saturating_sub(128);
        let mut used = 0;
        let mut number = 1usize;
        loop {
            tokio::task::yield_now().await;
            if context.cancellation().is_cancelled() {
                return Ok(result);
            }
            if reader.fill_buf().map_err(fs::io)?.is_empty() {
                break;
            }
            if number >= input.offset && number - input.offset >= input.limit {
                result.truncated = true;
                result.next_line = Some(number);
                break;
            }
            let mut line = Vec::new();
            loop {
                let bytes = reader.fill_buf().map_err(fs::io)?;
                if bytes.is_empty() {
                    break;
                }
                let size = bytes
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |i| i + 1);
                if line.len() + size > context.output_limit() {
                    if result.content.is_empty() {
                        return Err(fs::limit());
                    }
                    result.truncated = true;
                    result.next_line = Some(number);
                    return Ok(result);
                }
                let complete = bytes[size - 1] == b'\n';
                line.extend_from_slice(&bytes[..size]);
                reader.consume(size);
                if complete {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let text = std::str::from_utf8(&line)
                .ok()
                .filter(|s| !s.contains('\0'))
                .ok_or_else(|| {
                    Failure::new(
                        Code::Execution,
                        "Unsupported binary or invalid UTF-8 content; choose a UTF-8 text file.",
                    )
                })?;
            if number >= input.offset {
                let text = if number == 1 {
                    text.trim_start_matches('\u{feff}')
                } else {
                    text
                };
                let text = text.strip_suffix('\n').unwrap_or(text);
                let text = text.strip_suffix('\r').unwrap_or(text);
                let numbered = format!("{number}: {text}\n");
                // Each line is escaped once, avoiding quadratic serialization of a growing result.
                let size = serde_json::to_string(&numbered)
                    .expect("string serializes")
                    .len()
                    - 2;
                if size > budget - used {
                    if result.content.is_empty() {
                        return Err(fs::limit());
                    }
                    result.truncated = true;
                    result.next_line = Some(number);
                    break;
                }
                used += size;
                result.content.push_str(&numbered);
            }
            number = number.checked_add(1).ok_or_else(fs::limit)?;
        }
        Ok(result)
    }
}
