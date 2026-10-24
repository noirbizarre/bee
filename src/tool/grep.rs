//! Embedded ripgrep matching with bounded, cooperatively cancellable scanning.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use ignore::WalkBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Code, Context, Failure, Tool, discovery, fs};

/// Search regular text files under an ignore-aware directory root.
pub struct Grep;

/// Line-oriented regex search with independent content and path filtering.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Unicode Rust regex; matching never spans logical lines.
    #[schemars(length(min = 1, max = 4096))]
    pub pattern: String,
    /// Directory relative to cwd, or an absolute directory.
    #[serde(default = "path")]
    #[schemars(length(min = 1))]
    pub path: String,
    /// Enable Unicode case-insensitive content matching, not glob matching.
    #[serde(default)]
    pub case_insensitive: bool,
    /// Root-relative globs; any may match, or all files if empty.
    #[serde(default)]
    #[schemars(length(max = 100))]
    pub include: Vec<String>,
    /// Root-relative globs that take precedence over includes.
    #[serde(default)]
    #[schemars(length(max = 100))]
    pub exclude: Vec<String>,
    /// Traverse hidden names without disabling ignore rules.
    #[serde(default)]
    pub include_hidden: bool,
    /// Disable local ignore rules without disabling hidden filtering.
    #[serde(default)]
    pub include_ignored: bool,
    /// Maximum matching lines; the complete serialized byte budget also applies.
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

/// One matching logical line, regardless of the number of occurrences.
#[derive(Serialize)]
pub struct Match {
    /// UTF-8 path relative to the requested root, with forward slashes.
    pub path: String,
    /// One-based logical line number.
    pub line_number: u64,
    /// Complete line without its LF/CRLF ending or initial file BOM.
    pub line: String,
}

/// Deterministic, bounded search results with explicit omissions.
#[derive(Serialize)]
pub struct Output {
    /// Sorted by path, then numeric line number.
    pub matches: Vec<Match>,
    /// Matching lines were omitted, or traversal reached depth 64.
    pub truncated: bool,
    /// Files discarded because scanned content contained NUL or invalid UTF-8.
    pub skipped_binary_files: u64,
}

impl Tool for Grep {
    type Input = Input;
    type Output = Output;

    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents using a Unicode Rust regex (no lookaround/backreferences or multiline matches). Required pattern (1–4096 bytes); directory path defaults to cwd. Optional case_insensitive, include/exclude root-relative case-sensitive globs, include_hidden/include_ignored, and limit (default 1000, max 10000). Respects local ignore rules by default. Returns root-relative paths, 1-based line_number and complete line, sorted by path/line, byte-bounded with explicit truncation. Binary/invalid UTF-8 files are skipped and counted; lines exceeding the output byte budget fail. Depth cap 64; skips nested symlinks and special files. Absolute paths, .. and explicit directory symlink roots are allowed; not a sandbox. Embedded search requires no rg executable."
    }
    fn target(&self, input: &Input) -> Option<super::Target> {
        Some(super::Target {
            path: input.path.clone().into(),
            directory: true,
            // Search snippets are not explicit skill activation reads.
            read: false,
        })
    }
    async fn execute(&self, input: Input, context: &Context) -> Result<Output, Failure> {
        if input.pattern.is_empty() || input.pattern.len() > 4096 {
            return Err(invalid("Use a regex pattern between 1 and 4096 bytes."));
        }
        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(input.case_insensitive)
            .line_terminator(Some(b'\n'))
            .ban_byte(Some(0))
            // Compilation and DFA caches are separate from retained output storage.
            .size_limit(super::MAX_OUTPUT_BYTES)
            .dfa_size_limit(super::MAX_OUTPUT_BYTES)
            .build(&input.pattern)
            .map_err(|_| invalid("Invalid or oversized regex; use a smaller single-line Rust regex without lookaround, backreferences, newline or NUL matches."))?;
        let include = globs(&input.include)?;
        let exclude = globs(&input.exclude)?;
        let root = discovery::root(context, &input.path)?;
        let mut selection = Selection::new(context, input.limit)?;
        let mut skipped_binary_files = 0u64;
        let walk = |directory: &Path| {
            let mut builder = WalkBuilder::new(directory);
            // One-level walkers avoid unbounded wide-directory buffering at FD limits.
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
            let relative = discovery::relative(entry.path(), &root)?;
            if kind.is_dir() {
                if stack.len() >= 64 {
                    selection.truncated = true;
                } else {
                    stack.push(walk(entry.path()));
                }
            } else if kind.is_file()
                && (include.is_empty() || include.is_match(&relative))
                && !exclude.is_match(&relative)
            {
                match scan(entry.path(), &relative, &matcher, &input, context).await? {
                    Some(file) => selection.merge(file),
                    None => skipped_binary_files = skipped_binary_files.saturating_add(1),
                }
            }
        }
        Ok(Output {
            matches: selection.finish(),
            truncated: selection.truncated,
            skipped_binary_files,
        })
    }
}

fn invalid(message: &str) -> Failure {
    Failure::new(Code::InvalidArguments, message)
}

fn globs(patterns: &[String]) -> Result<GlobSet, Failure> {
    if patterns.len() > 100 {
        return Err(invalid("Use at most 100 include or exclude globs."));
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if pattern.is_empty() || pattern.len() > 4096 {
            return Err(invalid("Use glob patterns between 1 and 4096 bytes."));
        }
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .backslash_escape(true)
                .build()
                .map_err(|_| invalid("Invalid glob; check brackets, braces and ** placement."))?,
        );
    }
    builder
        .build()
        .map_err(|_| invalid("Glob filters are too complex; use fewer or simpler globs."))
}

async fn scan(
    path: &Path,
    relative: &str,
    matcher: &RegexMatcher,
    input: &Input,
    context: &Context,
) -> Result<Option<Selection>, Failure> {
    // Recheck before opening to avoid ordinary FIFO/device hangs after enumeration.
    if !std::fs::metadata(path).map_err(fs::io)?.is_file() {
        return Err(Failure::new(
            Code::Execution,
            "Search entry changed; retry against regular files.",
        ));
    }
    let mut reader = BufReader::with_capacity(8192, File::open(path).map_err(fs::io)?);
    // Stage only a bounded prefix until EOF: a late binary byte invalidates this
    // file without evicting valid candidates from other files in global selection.
    let mut selection = Selection::new(context, input.limit)?;
    let mut number = 1u64;
    let mut line = Vec::new();
    loop {
        tokio::task::yield_now().await;
        if context.cancellation().is_cancelled() {
            return Ok(Some(selection));
        }
        let bytes = reader.fill_buf().map_err(fs::io)?;
        if bytes.is_empty() && line.is_empty() {
            return Ok(Some(selection));
        }
        let size = bytes
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        if bytes[..size].contains(&0) {
            return Ok(None);
        }
        if line.len() + size > context.output_limit() {
            return Err(fs::limit());
        }
        let complete = size == 0 || bytes[size - 1] == b'\n';
        line.extend_from_slice(&bytes[..size]);
        reader.consume(size);
        if !complete {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            return Ok(None);
        };
        let text = if number == 1 {
            text.strip_prefix('\u{feff}').unwrap_or(text)
        } else {
            text
        };
        let text = text
            .strip_suffix('\n')
            .map_or(text, |s| s.strip_suffix('\r').unwrap_or(s));
        if matcher.is_match(text.as_bytes()).map_err(|_| {
            Failure::new(
                Code::Execution,
                "Regex matching failed; simplify the search pattern.",
            )
        })? {
            selection.insert(Match {
                path: relative.into(),
                line_number: number,
                line: text.into(),
            });
        }
        number = number.checked_add(1).ok_or_else(fs::limit)?;
        line.clear();
    }
}

type Key = (String, u64);

struct Selection {
    entries: BTreeMap<Key, (Match, usize)>,
    bytes: usize,
    budget: usize,
    limit: usize,
    cutoff: Option<Key>,
    truncated: bool,
}

impl Selection {
    fn new(context: &Context, limit: usize) -> Result<Self, Failure> {
        if !(1..=10000).contains(&limit) {
            return Err(invalid("Use a result limit between 1 and 10000."));
        }
        Ok(Self {
            entries: BTreeMap::new(),
            bytes: 0,
            // Reserve envelope, array, truncation flag and a maximal u64 counter.
            budget: context.output_limit() - 128,
            limit,
            cutoff: None,
            truncated: false,
        })
    }

    fn insert(&mut self, entry: Match) {
        let key = (entry.path.clone(), entry.line_number);
        // A lexical gap cannot be filled by later entries without making byte
        // truncation depend on directory enumeration order.
        if self.cutoff.as_ref().is_some_and(|cutoff| key >= *cutoff) {
            return;
        }
        let size = serde_json::to_vec(&entry).expect("match serializes").len() + 1;
        self.bytes += size;
        self.entries.insert(key, (entry, size));
        while self.bytes > self.budget || self.entries.len() > self.limit {
            let (key, (_, size)) = self
                .entries
                .pop_last()
                .expect("nonempty oversized selection");
            self.bytes -= size;
            self.cutoff = Some(key);
            self.truncated = true;
        }
    }

    fn merge(&mut self, file: Self) {
        self.truncated |= file.truncated;
        for (_, (entry, _)) in file.entries {
            self.insert(entry);
        }
        if let Some(cutoff) = file.cutoff {
            if self.cutoff.as_ref().is_none_or(|current| cutoff < *current) {
                self.cutoff = Some(cutoff);
            }
            // The omitted file prefix may precede already retained global entries.
            while self
                .entries
                .last_key_value()
                .is_some_and(|(key, _)| Some(key) >= self.cutoff.as_ref())
            {
                let (_, (_, size)) = self.entries.pop_last().expect("entry exists");
                self.bytes -= size;
            }
        }
    }

    fn finish(&mut self) -> Vec<Match> {
        std::mem::take(&mut self.entries)
            .into_values()
            .map(|(entry, _)| entry)
            .collect()
    }
}
