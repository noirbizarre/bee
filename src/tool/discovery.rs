//! Bounded deterministic selection, shared by the two concrete discovery tools.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use super::{Code, Context, Failure, fs};

/// One root-relative entry; directory symlinks remain symlinks.
#[derive(Serialize)]
pub struct Entry {
    /// UTF-8 path using forward slashes, relative to the requested root.
    pub path: String,
    /// File, directory, symlink, or other special entry.
    pub kind: Kind,
}

/// Entry type without following symlink targets.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symlink, including dangling links.
    Symlink,
    /// Another filesystem entry such as a socket.
    Other,
}

impl From<std::fs::FileType> for Kind {
    fn from(value: std::fs::FileType) -> Self {
        if value.is_symlink() {
            Self::Symlink
        } else if value.is_dir() {
            Self::Directory
        } else if value.is_file() {
            Self::File
        } else {
            Self::Other
        }
    }
}

/// A lexical prefix bounded by both count and serialized bytes.
#[derive(Serialize)]
pub struct Output {
    /// Entries sorted lexically by their UTF-8 relative paths.
    pub entries: Vec<Entry>,
    /// More entries existed, or the documented traversal depth was reached.
    pub truncated: bool,
}

pub(super) fn root(context: &Context, value: &str) -> Result<std::path::PathBuf, Failure> {
    let path = fs::path(context, value)?;
    if !std::fs::metadata(&path).map_err(fs::io)?.is_dir() {
        return Err(Failure::new(
            Code::Execution,
            "Expected a directory root; use read for files.",
        ));
    }
    Ok(path)
}

pub(super) fn relative(path: &Path, root: &Path) -> Result<String, Failure> {
    let relative = path.strip_prefix(root).map_err(|_| {
        Failure::new(
            Code::Execution,
            "Could not resolve a root-relative entry; choose another root.",
        )
    })?;
    relative
        .components()
        .map(|component| {
            component.as_os_str().to_str().ok_or_else(|| {
                Failure::new(
                    Code::Execution,
                    "Unsupported non-UTF-8 filename; choose a directory with UTF-8 names.",
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("/"))
}

pub(super) struct Selection {
    entries: BTreeMap<String, (Kind, usize)>,
    bytes: usize,
    budget: usize,
    limit: usize,
    cutoff: Option<String>,
    pub truncated: bool,
}

impl Selection {
    pub fn new(context: &Context, limit: usize) -> Result<Self, Failure> {
        if !(1..=10000).contains(&limit) {
            return Err(Failure::new(
                Code::InvalidArguments,
                "Use a result limit between 1 and 10000.",
            ));
        }
        Ok(Self {
            entries: BTreeMap::new(),
            bytes: 0,
            budget: context.output_limit() - 64,
            limit,
            cutoff: None,
            truncated: false,
        })
    }

    pub fn insert(&mut self, path: String, kind: Kind) {
        // Once a prefix entry cannot fit, later paths cannot fill the gap without
        // making the result depend on filesystem enumeration order.
        if self.cutoff.as_ref().is_some_and(|cutoff| path >= *cutoff) {
            return;
        }
        let entry = Entry { path, kind };
        let size = serde_json::to_vec(&entry).expect("entry serializes").len() + 1;
        // Oversized entries participate in prefix selection too; failing only
        // when one is seen before a cutoff would make errors order-dependent.
        self.bytes += size;
        self.entries.insert(entry.path, (kind, size));
        while self.bytes > self.budget || self.entries.len() > self.limit {
            let (path, (_, size)) = self
                .entries
                .pop_last()
                .expect("nonempty oversized selection");
            self.bytes -= size;
            self.cutoff = Some(path);
            self.truncated = true;
        }
    }

    pub fn finish(self) -> Output {
        Output {
            entries: self
                .entries
                .into_iter()
                .map(|(path, (kind, _))| Entry { path, kind })
                .collect(),
            truncated: self.truncated,
        }
    }
}
