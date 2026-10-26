//! Complete bounded mutation manifests. Traversal never adopts unplanned entries.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Context, Failure, Target, mutation};

const MAX_ENTRIES: usize = 10_000;
const MAX_COPY: u64 = 64 * 1024 * 1024;

struct Anchor {
    path: PathBuf,
    #[cfg(unix)]
    file: File,
    // Windows MoveFileEx cannot rename a tree while inspection keeps descendant handles open.
    // Keep identity snapshots there; Unix traversal uses anchored no-follow directory descriptors.
    #[cfg(windows)]
    metadata: std::fs::Metadata,
    #[cfg(windows)]
    _parent: Option<Arc<Anchor>>,
}

struct Staging<'a> {
    directory: Option<tempfile::TempDir>,
    context: &'a Context,
}

impl Staging<'_> {
    fn path(&self) -> &Path {
        self.directory.as_ref().expect("owned staging").path()
    }
}

impl Drop for Staging<'_> {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            let path = directory.path().to_path_buf();
            if directory.close().is_err() {
                // Cancellation may skip ordinary results, so preserve actual cleanup failures as observations.
                let _ = self.context.emit(crate::presentation::Artifact::Note(format!("Copy staging cleanup failed at {}; inspect and remove that leftover staging directory.",path.display())));
            }
        }
    }
}

impl Anchor {
    fn validate(&self) -> Result<(), Failure> {
        let current = std::fs::symlink_metadata(&self.path).map_err(mutation::io)?;
        #[cfg(unix)]
        let opened = self.file.metadata().map_err(mutation::io)?;
        #[cfg(windows)]
        let opened = &self.metadata;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if current.dev() != opened.dev() || current.ino() != opened.ino() {
                return Err(conflict());
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if reparse(&current) || current.creation_time() != opened.creation_time() {
                return Err(conflict());
            }
            if let Some(parent) = &self._parent {
                parent.validate()?;
            }
        }
        Ok(())
    }
    fn open(path: &Path) -> Result<Arc<Self>, Failure> {
        #[cfg(unix)]
        let file = File::open(path).map_err(mutation::io)?;
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(7)
                .custom_flags(0x02000000 | 0x00200000)
                .open(path)
                .map_err(mutation::io)?
        };
        #[cfg(windows)]
        let metadata = {
            let metadata = file.metadata().map_err(mutation::io)?;
            if reparse(&metadata) {
                return Err(unsupported());
            }
            drop(file);
            metadata
        };
        #[cfg(windows)]
        let parent = path
            .parent()
            .filter(|p| *p != path)
            .map(Self::open)
            .transpose()?;
        Ok(Arc::new(Self {
            path: path.into(),
            #[cfg(unix)]
            file,
            #[cfg(windows)]
            metadata,
            #[cfg(windows)]
            _parent: parent,
        }))
    }

    fn child(self: &Arc<Self>, name: &std::ffi::OsStr) -> Result<Arc<Self>, Failure> {
        #[cfg(unix)]
        {
            use nix::{
                fcntl::{OFlag, openat},
                sys::stat::Mode,
            };
            let file = File::from(
                openat(
                    &self.file,
                    name,
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(unix_error)?,
            );
            Ok(Arc::new(Self {
                path: self.path.join(name),
                file,
            }))
        }
        #[cfg(windows)]
        {
            let child = Self::open(&self.path.join(name))?;
            Ok(child)
        }
    }

    fn names(&self) -> Result<Vec<OsString>, Failure> {
        let mut names = Vec::new();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let mut dir = nix::dir::Dir::openat(
                &self.file,
                ".",
                nix::fcntl::OFlag::O_RDONLY
                    | nix::fcntl::OFlag::O_DIRECTORY
                    | nix::fcntl::OFlag::O_CLOEXEC,
                nix::sys::stat::Mode::empty(),
            )
            .map_err(unix_error)?;
            for entry in dir.iter() {
                let entry = entry.map_err(unix_error)?;
                let name = entry.file_name().to_bytes();
                if matches!(name, b"." | b"..") {
                    continue;
                }
                if names.len() == MAX_ENTRIES {
                    return Err(limit());
                }
                names.push(std::ffi::OsStr::from_bytes(name).to_owned());
            }
        }
        #[cfg(windows)]
        for entry in std::fs::read_dir(&self.path).map_err(mutation::io)? {
            if names.len() == MAX_ENTRIES {
                return Err(limit());
            }
            names.push(entry.map_err(mutation::io)?.file_name());
        }
        names.sort();
        Ok(names)
    }
}

#[cfg(unix)]
fn unix_error(error: nix::errno::Errno) -> Failure {
    mutation::io(std::io::Error::from_raw_os_error(error as i32))
}

fn unsupported() -> Failure {
    mutation::failure(
        "Symlinks, reparse points, special files and nested mounts are unsupported; choose an ordinary file tree.",
    )
}
fn limit() -> Failure {
    mutation::failure(
        "Mutation exceeds 10000 entries, depth 64, manifest/presentation budget, or 64 MiB copy budget; split the operation.",
    )
}
fn conflict() -> Failure {
    mutation::failure(
        "Source tree changed during preparation; inspect the remaining paths and retry. Committed removals are not rolled back.",
    )
}

fn under(root: &Path, relative: &Path) -> PathBuf {
    // Joining an empty relative path adds a trailing separator, invalid for regular-file creation.
    if relative.as_os_str().is_empty() {
        root.into()
    } else {
        root.join(relative)
    }
}

#[cfg(windows)]
fn reparse(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0
}

#[derive(Clone, PartialEq, Eq)]
struct Identity {
    signature: Vec<u64>,
    key: Vec<u64>,
    directory: bool,
    size: u64,
    device: u64,
}

fn identity(parent: &Anchor, name: &std::ffi::OsStr) -> Result<Identity, Failure> {
    #[cfg(unix)]
    {
        use nix::{
            fcntl::AtFlags,
            sys::stat::{SFlag, fstatat},
        };
        let stat = fstatat(&parent.file, name, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(unix_error)?;
        let mode = SFlag::from_bits_truncate(stat.st_mode);
        if !matches!(mode & SFlag::S_IFMT, SFlag::S_IFDIR | SFlag::S_IFREG) {
            return Err(unsupported());
        }
        #[allow(clippy::unnecessary_cast)]
        let signature = vec![
            stat.st_dev as u64,
            stat.st_ino as u64,
            stat.st_mode as u64,
            stat.st_size as u64,
            stat.st_mtime as u64,
            stat.st_mtime_nsec as u64,
        ];
        Ok(Identity {
            key: signature[..3].to_vec(),
            signature,
            directory: mode.contains(SFlag::S_IFDIR),
            size: stat.st_size as u64,
            device: stat.st_dev as u64,
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(parent.path.join(name)).map_err(mutation::io)?;
        if reparse(&meta) || !(meta.is_file() || meta.is_dir()) {
            return Err(unsupported());
        }
        Ok(Identity {
            signature: vec![
                meta.file_size(),
                meta.creation_time(),
                meta.last_write_time(),
                u64::from(meta.file_attributes()),
            ],
            key: vec![meta.creation_time(), u64::from(meta.file_attributes())],
            directory: meta.is_dir(),
            size: meta.len(),
            device: 0,
        })
    }
}

struct Entry {
    parent: Arc<Anchor>,
    name: OsString,
    relative: PathBuf,
    original: Identity,
}

impl Entry {
    fn path(&self) -> PathBuf {
        self.parent.path.join(&self.name)
    }
    fn validate(&self, content: bool) -> Result<(), Failure> {
        self.parent.validate()?;
        let current = identity(&self.parent, &self.name)?;
        // Directory timestamps necessarily change during postorder deletion.
        let unchanged = if content || !self.original.directory {
            current == self.original
        } else {
            current.key == self.original.key && current.directory
        };
        if unchanged { Ok(()) } else { Err(conflict()) }
    }
    fn reader(&self) -> Result<File, Failure> {
        self.validate(true)?;
        #[cfg(unix)]
        {
            use nix::{
                fcntl::{OFlag, openat},
                sys::stat::Mode,
            };
            let file = File::from(
                openat(
                    &self.parent.file,
                    self.name.as_os_str(),
                    OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(unix_error)?,
            );
            use std::os::unix::fs::MetadataExt;
            let meta = file.metadata().map_err(mutation::io)?;
            if !meta.is_file()
                || meta.dev() != self.original.key[0]
                || meta.ino() != self.original.key[1]
            {
                return Err(conflict());
            }
            Ok(file)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .custom_flags(0x00200000)
                .open(self.path())
                .map_err(mutation::io)?;
            if reparse(&file.metadata().map_err(mutation::io)?) {
                return Err(conflict());
            }
            Ok(file)
        }
    }
    fn text(&self) -> Option<String> {
        if self.original.directory
            || self.original.size > (crate::presentation::PATCH_BYTES / 2) as u64
        {
            return None;
        }
        let mut bytes = Vec::new();
        self.reader()
            .ok()?
            .take((crate::presentation::PATCH_BYTES / 2 + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        self.validate(true).ok()?;
        if bytes.len() > crate::presentation::PATCH_BYTES / 2 || bytes.contains(&0) {
            return None;
        }
        String::from_utf8(bytes).ok()
    }
    fn remove(&self) -> Result<(), Failure> {
        self.validate(false)?;
        #[cfg(unix)]
        {
            nix::unistd::unlinkat(
                &self.parent.file,
                self.name.as_os_str(),
                if self.original.directory {
                    nix::unistd::UnlinkatFlags::RemoveDir
                } else {
                    nix::unistd::UnlinkatFlags::NoRemoveDir
                },
            )
            .map_err(unix_error)
        }
        #[cfg(windows)]
        {
            if self.original.directory {
                use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
                use windows_sys::Win32::Storage::FileSystem::{
                    FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
                    FILE_DISPOSITION_INFO_EX, FileDispositionInfoEx, SetFileInformationByHandle,
                };
                let directory = std::fs::OpenOptions::new()
                    .access_mode(0x00010000 | 0x80)
                    .share_mode(7)
                    .custom_flags(0x02000000 | 0x00200000)
                    .open(self.path()).map_err(mutation::io)?;
                if reparse(&directory.metadata().map_err(mutation::io)?) { return Err(conflict()); }
                self.validate(false)?;
                let disposition = FILE_DISPOSITION_INFO_EX {
                    Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
                };
                // SAFETY: the owned directory handle and correctly sized disposition live through the call.
                // POSIX unlink-on-close removes the name even while manifest anchors remain open;
                // ordinary RemoveDirectory would leave a pending child blocking its ancestor's deletion.
                let removed = unsafe { SetFileInformationByHandle(
                    directory.as_raw_handle(), FileDispositionInfoEx,
                    (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
                    std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
                ) };
                if removed == 0 { return Err(mutation::failure("Directory unlink failed; check permissions and use a filesystem supporting Windows POSIX deletion semantics. Earlier removals were not rolled back.")); }
                drop(directory);
                Ok(())
            } else {
                std::fs::remove_file(self.path())
            }
            .map_err(mutation::io)
        }
    }

    fn git_mode(&self) -> Option<u32> {
        #[cfg(unix)]
        {
            Some(if self.original.key[2] & 0o111 != 0 {
                0o100755
            } else {
                0o100644
            })
        }
        #[cfg(windows)]
        {
            None
        }
    }
}

pub(super) struct Manifest {
    source: PathBuf,
    destination: Option<PathBuf>,
    entries: Vec<Entry>,
}

pub(super) fn scopes(
    context: &Context,
    source: &str,
    destination: Option<&str>,
    recursive: bool,
    operation: &str,
) -> Vec<Target> {
    let mut prepared = context
        .prepared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Preparation is refreshed before each guidance barrier, not silently during execution.
    prepared.remove(&context.call_id);
    match Manifest::prepare(context, source, destination, recursive, operation) {
        Ok(manifest) => {
            if prepared
                .values()
                .filter_map(|m| m.as_ref().ok())
                .map(|m| m.entries.len())
                .sum::<usize>()
                + manifest.entries.len()
                > MAX_ENTRIES
            {
                prepared.insert(context.call_id.clone(), Err(limit()));
                return Vec::new();
            }
            let scopes = manifest.scopes();
            prepared.insert(context.call_id.clone(), Ok(Arc::new(manifest)));
            scopes
        }
        Err(error) => {
            prepared.insert(context.call_id.clone(), Err(error));
            Vec::new()
        }
    }
}

pub(super) fn prepared(
    context: &Context,
    source: &str,
    destination: Option<&str>,
    recursive: bool,
    operation: &str,
) -> Result<Arc<Manifest>, Failure> {
    if let Some(manifest) = context
        .prepared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&context.call_id)
    {
        let manifest = manifest?;
        manifest.validate()?;
        return Ok(manifest);
    }
    Manifest::prepare(context, source, destination, recursive, operation).map(Arc::new)
}

impl Manifest {
    pub fn prepare(
        context: &Context,
        source: &str,
        destination: Option<&str>,
        recursive: bool,
        operation: &str,
    ) -> Result<Self, Failure> {
        mutation::check(context)?;
        let requested = super::fs::path(context, source)?;
        let name = requested
            .file_name()
            .ok_or_else(|| {
                mutation::failure("Choose a file or directory below a filesystem root.")
            })?
            .to_owned();
        let parent = std::fs::canonicalize(requested.parent().ok_or_else(unsupported)?)
            .map_err(mutation::io)?;
        let source = parent.join(&name);
        if source.to_str().is_none() {
            return Err(unsupported());
        }
        let parent = Anchor::open(&parent)?;
        let original = identity(&parent, &name)?;
        if original.directory
            && operation != "copy"
            && std::fs::canonicalize(context.cwd())
                .map_err(mutation::io)?
                .starts_with(&source)
        {
            return Err(mutation::failure(
                "Cannot delete or move the session working directory or its ancestors; choose a different target.",
            ));
        }
        let destination = destination
            .map(|value| {
                let requested = super::fs::path(context, value)?;
                let name = requested.file_name().ok_or_else(unsupported)?;
                let parent = std::fs::canonicalize(requested.parent().ok_or_else(unsupported)?)
                    .map_err(mutation::io)?;
                let destination = parent.join(name);
                if destination == source || (original.directory && destination.starts_with(&source))
                {
                    return Err(mutation::failure(
                        "Destination must be outside the source tree and name a new path.",
                    ));
                }
                match std::fs::symlink_metadata(&destination) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(destination),
                    Err(e) => Err(mutation::io(e)),
                    Ok(_) => Err(mutation::failure(
                        "Destination exists; choose an absent exact destination path.",
                    )),
                }
            })
            .transpose()?;
        if original.directory && operation == "copy" && !recursive {
            return Err(mutation::failure(
                "Directory copy requires recursive:true; inspect the source tree first.",
            ));
        }
        let device = original.device;
        let mut manifest = Self {
            source,
            destination,
            entries: vec![Entry {
                parent,
                name,
                relative: PathBuf::new(),
                original,
            }],
        };
        let mut index = 0;
        let mut bytes = 0_u64;
        let mut path_bytes = 0usize;
        while index < manifest.entries.len() {
            mutation::check(context)?;
            let entry = &manifest.entries[index];
            bytes = bytes.saturating_add(if entry.original.directory {
                0
            } else {
                entry.original.size
            });
            path_bytes = path_bytes.saturating_add(entry.path().as_os_str().len() * 6 + 1024);
            if path_bytes > 2 * 1024 * 1024 || (operation == "copy" && bytes > MAX_COPY) {
                return Err(limit());
            }
            if entry.original.directory {
                let directory = entry.parent.child(&entry.name)?;
                let names = directory.names()?;
                if operation == "delete" && !recursive && !names.is_empty() {
                    return Err(mutation::failure(
                        "Directory is nonempty; inspect it and set recursive:true to delete its tree.",
                    ));
                }
                if manifest.entries.len() + names.len() > MAX_ENTRIES {
                    return Err(limit());
                }
                let relative = entry.relative.clone();
                for name in names {
                    if name.to_str().is_none() {
                        return Err(unsupported());
                    }
                    let original = identity(&directory, &name)?;
                    if original.device != device {
                        return Err(unsupported());
                    }
                    let relative = relative.join(&name);
                    if relative.components().count() > 64 {
                        return Err(limit());
                    }
                    manifest.entries.push(Entry {
                        parent: directory.clone(),
                        name,
                        relative,
                        original,
                    });
                }
            }
            index += 1;
        }
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn scopes(&self) -> Vec<Target> {
        self.entries
            .iter()
            .flat_map(|entry| {
                let mut targets = vec![Target {
                    path: entry.path(),
                    directory: entry.original.directory,
                    read: false,
                }];
                if let Some(destination) = &self.destination {
                    targets.push(Target {
                        path: under(destination, &entry.relative),
                        directory: entry.original.directory,
                        read: false,
                    });
                }
                targets
            })
            .collect()
    }

    fn validate(&self) -> Result<(), Failure> {
        let mut children = std::collections::BTreeMap::<PathBuf, Vec<OsString>>::new();
        for entry in &self.entries {
            children
                .entry(entry.parent.path.clone())
                .or_default()
                .push(entry.name.clone());
        }
        for names in children.values_mut() {
            names.sort();
        }
        for entry in &self.entries {
            entry.validate(true)?;
            if entry.original.directory {
                let actual = entry.parent.child(&entry.name)?.names()?;
                if actual.as_slice() != children.get(&entry.path()).map_or(&[][..], Vec::as_slice) {
                    return Err(conflict());
                }
            }
        }
        Ok(())
    }

    pub async fn delete(&self, context: &Context) -> Result<usize, Failure> {
        self.validate()?;
        self.reserve(context)?;
        let mut removed = 0;
        for entry in self.entries.iter().rev() {
            tokio::task::yield_now().await;
            mutation::check(context)?;
            let old = if self.entries.len() == 1 {
                entry.text()
            } else {
                None
            };
            if let Err(error) = entry.remove() {
                return Err(Failure::new(
                    super::Code::Execution,
                    format!(
                        "Deletion stopped after {removed} committed entries; remaining entries were not removed. Inspect the tree and retry. {error}"
                    ),
                ));
            }
            let patch = old.as_deref().and_then(|old| {
                crate::presentation::patch::generate_with_mode(
                    "delete",
                    &entry.path(),
                    None,
                    Some(old),
                    None,
                    entry.git_mode(),
                )
            });
            context.emit(crate::presentation::Artifact::Changes(
                vec![crate::presentation::Change {
                    operation: "delete".into(),
                    path: entry.path(),
                    old_path: None,
                    file_type: if entry.original.directory {
                        Some("directory".into())
                    } else {
                        old.as_ref().map(|_| "text".into())
                    },
                }],
                patch,
            ))?;
            removed += 1;
        }
        Ok(removed)
    }

    pub async fn copy(&self, context: &Context) -> Result<usize, Failure> {
        self.reserve(context)?;
        let destination = self.destination.as_ref().ok_or_else(unsupported)?;
        let parent = destination.parent().ok_or_else(unsupported)?;
        let staging = tempfile::Builder::new()
            .prefix(".bee-copy-")
            .tempdir_in(parent)
            .map_err(mutation::io)?;
        let staging = Staging {
            directory: Some(staging),
            context,
        };
        let root = staging.path().join("content");
        let mut copied = 0_u64;
        for entry in &self.entries {
            tokio::task::yield_now().await;
            mutation::check(context)?;
            let target = under(&root, &entry.relative);
            if entry.original.directory {
                std::fs::create_dir(&target).map_err(mutation::io)?;
            } else {
                let mut input = entry.reader()?;
                let mut output = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .map_err(mutation::io)?;
                let mut buffer = [0; 8192];
                loop {
                    mutation::check(context)?;
                    let n = input.read(&mut buffer).map_err(mutation::io)?;
                    if n == 0 {
                        break;
                    }
                    copied = copied.saturating_add(n as u64);
                    if copied > MAX_COPY {
                        return Err(limit());
                    }
                    output.write_all(&buffer[..n]).map_err(mutation::io)?;
                    tokio::task::yield_now().await;
                }
                output.sync_all().map_err(mutation::io)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    // Copy ordinary access bits, never setuid/setgid/sticky privilege bits.
                    output
                        .set_permissions(std::fs::Permissions::from_mode(
                            input.metadata().map_err(mutation::io)?.permissions().mode() & 0o777,
                        ))
                        .map_err(mutation::io)?;
                }
                entry.validate(true)?;
            }
        }
        self.validate()?;
        mutation::check(context)?;
        // No await between final validation, atomic publication, and committed receipts.
        let staging_parent = Anchor::open(staging.path())?;
        let destination_parent = Anchor::open(parent)?;
        rename_exclusive(
            &staging_parent,
            std::ffi::OsStr::new("content"),
            &destination_parent,
            destination.file_name().ok_or_else(unsupported)?,
        )?;
        self.receipts(context, "copy")?;
        Ok(self.entries.len())
    }

    pub async fn move_to(&self, context: &Context) -> Result<usize, Failure> {
        self.reserve(context)?;
        tokio::task::yield_now().await;
        self.validate()?;
        mutation::check(context)?;
        let destination = self.destination.as_ref().ok_or_else(unsupported)?;
        let destination_parent = Anchor::open(destination.parent().ok_or_else(unsupported)?)?;
        rename_exclusive(
            &self.entries[0].parent,
            &self.entries[0].name,
            &destination_parent,
            destination.file_name().ok_or_else(unsupported)?,
        )?;
        self.receipts(context, "move")?;
        Ok(self.entries.len())
    }

    fn receipts(&self, context: &Context, operation: &str) -> Result<(), Failure> {
        let destination = self.destination.as_ref().ok_or_else(unsupported)?;
        // One committed publication produces one bounded receipt, avoiding queue floods.
        let changes = self
            .entries
            .iter()
            .map(|entry| crate::presentation::Change {
                operation: operation.into(),
                path: under(destination, &entry.relative),
                old_path: Some(entry.path()),
                file_type: entry.original.directory.then(|| "directory".into()),
            })
            .collect();
        let patch = if self.entries.len() == 1 && !self.entries[0].original.directory {
            crate::presentation::patch::generate(
                operation,
                destination,
                Some(&self.source),
                None,
                None,
            )
        } else {
            None
        };
        context.emit(crate::presentation::Artifact::Changes(changes, patch))
    }

    fn reserve(&self, context: &Context) -> Result<(), Failure> {
        let bytes = self
            .entries
            .iter()
            .map(|entry| {
                let destination = self
                    .destination
                    .as_ref()
                    .map_or(0, |path| path.join(&entry.relative).as_os_str().len());
                (entry.path().as_os_str().len() + destination) * 6 + 1024
            })
            .sum::<usize>();
        // Recursive deletes may produce many patches; omit them rather than exceed receipt reservation.
        let patches = if self.entries.len() == 1 {
            crate::presentation::PATCH_BYTES
        } else {
            0
        };
        if bytes.saturating_add(patches) > 1024 * 1024 {
            return Err(limit());
        }
        context.emit(crate::presentation::Artifact::Reserve(
            bytes.saturating_add(patches),
        ))
    }
}

fn rename_exclusive(
    source_parent: &Anchor,
    source_name: &std::ffi::OsStr,
    destination_parent: &Anchor,
    destination_name: &std::ffi::OsStr,
) -> Result<(), Failure> {
    #[cfg(target_os = "linux")]
    let result = nix::fcntl::renameat2(
        &source_parent.file,
        source_name,
        &destination_parent.file,
        destination_name,
        nix::fcntl::RenameFlags::RENAME_NOREPLACE,
    )
    .map_err(|e| std::io::Error::from_raw_os_error(e as i32));
    #[cfg(target_os = "macos")]
    let result = {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source_name.as_bytes()).map_err(|_| unsupported())?;
        let destination =
            std::ffi::CString::new(destination_name.as_bytes()).map_err(|_| unsupported())?;
        // SAFETY: both C strings are NUL terminated and live throughout the call.
        let code = unsafe {
            nix::libc::renameatx_np(
                source_parent.file.as_raw_fd(),
                source.as_ptr(),
                destination_parent.file.as_raw_fd(),
                destination.as_ptr(),
                nix::libc::RENAME_EXCL,
            )
        };
        if code == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    };
    #[cfg(windows)]
    let result = {
        use std::os::windows::ffi::OsStrExt;
        let source = source_parent
            .path
            .join(source_name)
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let destination = destination_parent
            .path
            .join(destination_name)
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        // SAFETY: pointers refer to live NUL-terminated path buffers; flags never permit replacement/copy.
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                0,
            )
        } != 0
        {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let result: std::io::Result<()> = Err(std::io::ErrorKind::Unsupported.into());
    result.map_err(|_| mutation::failure("Exclusive rename failed; ensure destination is absent, source and destination share a filesystem, and the platform supports no-clobber rename."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn context(root: &tempfile::TempDir) -> Context {
        Context::new(
            root.path(),
            CancellationToken::new(),
            super::super::MAX_OUTPUT_BYTES,
        )
        .unwrap()
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn recursive_deletion_unlinks_nested_directories_while_other_manifests_hold_anchors() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("tree/nested/deeper")).unwrap();
        std::fs::write(root.path().join("tree/nested/deeper/file"), "contents").unwrap();
        let context = context(&root);
        let other = Manifest::prepare(&context, "tree", Some("copy"), true, "copy").unwrap();
        let deletion = Manifest::prepare(&context, "tree", None, true, "delete").unwrap();
        let inspected = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(7)
            .custom_flags(0x02000000 | 0x00200000)
            .open(root.path().join("tree/nested/deeper"))
            .unwrap();
        assert_eq!(deletion.delete(&context).await.unwrap(), 4);
        assert!(!root.path().join("tree").exists());
        drop(inspected);
        drop(other);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn directory_moves_release_inspection_handles_before_namespace_publication() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("source/nested")).unwrap();
        std::fs::write(root.path().join("source/nested/AGENTS.md"), "guidance").unwrap();
        let context = context(&root);
        let manifest =
            Manifest::prepare(&context, "source", Some("destination"), true, "move").unwrap();
        assert_eq!(manifest.move_to(&context).await.unwrap(), 3);
        assert!(!root.path().join("source").exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join("destination/nested/AGENTS.md")).unwrap(),
            "guidance"
        );
    }

    #[tokio::test]
    async fn an_escaped_patch_near_the_turn_limit_cannot_discard_a_committed_deletion_receipt() {
        use crate::presentation::{Event, State};
        use serde_json::json;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("escaped");
        std::fs::write(&path, "\u{1}".repeat(crate::presentation::PATCH_BYTES / 2)).unwrap();
        let mut state = State::default();
        state
            .apply(
                1,
                &Event::Accepted {
                    id: "filler".into(),
                    name: "tool".into(),
                    kind: "other".into(),
                    locations: Vec::new(),
                    input: json!({}),
                },
            )
            .unwrap();
        state
            .apply(
                1,
                &Event::Finished {
                    id: "filler".into(),
                    output: json!(null),
                    text: "x".repeat(
                        crate::presentation::TURN_BYTES
                            - crate::presentation::PATCH_BYTES
                            - 128 * 1024,
                    ),
                    failed: false,
                },
            )
            .unwrap();
        state
            .apply(
                1,
                &Event::Accepted {
                    id: "delete".into(),
                    name: "delete".into(),
                    kind: "delete".into(),
                    locations: Vec::new(),
                    input: json!({}),
                },
            )
            .unwrap();
        let state = Arc::new(std::sync::Mutex::new(state));
        let retained = state.clone();
        let context = context(&root).with_artifacts(move |_, artifact| {
            retained
                .lock()
                .unwrap()
                .apply(
                    1,
                    &Event::Artifact {
                        id: "delete".into(),
                        artifact,
                    },
                )
                .map(|_| ())
                .map_err(|_| limit())
        });
        Manifest::prepare(&context, "escaped", None, false, "delete")
            .unwrap()
            .delete(&context)
            .await
            .unwrap();
        assert!(!path.exists());
        let state = state.lock().unwrap();
        let snapshots = state.snapshots();
        assert_eq!(
            snapshots[1]["content"][0]["changes"][0]["operation"],
            "delete"
        );
        assert!(snapshots[1]["content"][0].get("patch").is_none());
        let restored: State =
            serde_json::from_value(serde_json::to_value(&*state).unwrap()).unwrap();
        assert_eq!(restored.snapshots(), snapshots);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deleting_an_executable_script_preserves_its_git_mode_in_the_receipt() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("script");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let patches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = patches.clone();
        let context = context(&root).with_artifacts(move |_, artifact| {
            if let crate::presentation::Artifact::Changes(_, Some(patch)) = artifact {
                observed.lock().unwrap().push(patch);
            }
            Ok(())
        });
        Manifest::prepare(&context, "script", None, false, "delete")
            .unwrap()
            .delete(&context)
            .await
            .unwrap();
        assert!(!path.exists());
        assert!(patches.lock().unwrap()[0].contains("deleted file mode 100755\n"));
    }

    #[tokio::test]
    async fn destination_races_never_replace_files_or_directories() {
        let root = tempfile::tempdir().unwrap();
        let context = context(&root);
        for directory in [false, true] {
            let source = root.path().join("source");
            if directory {
                std::fs::create_dir(&source).unwrap();
            } else {
                std::fs::write(&source, "source").unwrap();
            }
            let manifest =
                Manifest::prepare(&context, "source", Some("destination"), true, "move").unwrap();
            std::fs::write(root.path().join("destination"), "untouched").unwrap();
            assert!(manifest.move_to(&context).await.is_err());
            assert!(source.exists());
            assert_eq!(
                std::fs::read(root.path().join("destination")).unwrap(),
                b"untouched"
            );
            std::fs::remove_file(root.path().join("destination")).unwrap();
            if directory {
                std::fs::remove_dir(source).unwrap();
            } else {
                std::fs::remove_file(source).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn cancellation_after_a_deletion_preserves_exact_committed_receipts_and_remaining_entries()
     {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("tree")).unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(root.path().join("tree").join(name), name).unwrap();
        }
        let token = CancellationToken::new();
        let cancelled = token.clone();
        let receipts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = receipts.clone();
        let context = Context::new(root.path(), token, super::super::MAX_OUTPUT_BYTES)
            .unwrap()
            .with_artifacts(move |_, artifact| {
                if let crate::presentation::Artifact::Changes(changes, _) = artifact {
                    observed.lock().unwrap().extend(changes);
                    cancelled.cancel();
                }
                Ok(())
            });
        let manifest = Manifest::prepare(&context, "tree", None, true, "delete").unwrap();
        assert!(manifest.delete(&context).await.is_err());
        let receipts = receipts.lock().unwrap();
        assert_eq!(receipts.len(), 1);
        assert!(!receipts[0].path.exists());
        assert_eq!(
            std::fs::read_dir(root.path().join("tree")).unwrap().count(),
            2
        );
    }

    #[tokio::test]
    async fn new_entries_are_not_adopted_after_preparation_and_copy_failure_leaves_no_destination()
    {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("source")).unwrap();
        std::fs::write(root.path().join("source/old"), "old").unwrap();
        let context = context(&root);
        let manifest = Manifest::prepare(&context, "source", Some("copy"), true, "copy").unwrap();
        std::fs::write(root.path().join("source/new"), "new").unwrap();
        assert!(manifest.copy(&context).await.is_err());
        assert!(!root.path().join("copy").exists());
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "staging is cleaned up"
        );
    }

    #[test]
    fn recursive_opt_in_and_self_descendant_and_cwd_rejections_precede_side_effects() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("tree")).unwrap();
        std::fs::write(root.path().join("tree/file"), "file").unwrap();
        let context = context(&root);
        assert!(Manifest::prepare(&context, "tree", None, false, "delete").is_err());
        assert!(Manifest::prepare(&context, "tree", Some("copy"), false, "copy").is_err());
        assert!(Manifest::prepare(&context, "tree", Some("tree/inside"), true, "move").is_err());
        assert!(Manifest::prepare(&context, "tree", Some("tree"), true, "copy").is_err());
        assert!(
            Manifest::prepare(
                &context,
                root.path().to_str().unwrap(),
                None,
                true,
                "delete"
            )
            .is_err()
        );
        assert!(root.path().join("tree/file").exists());
    }

    #[test]
    fn oversized_copies_and_deep_trees_fail_during_complete_preparation() {
        let root = tempfile::tempdir().unwrap();
        File::create(root.path().join("large"))
            .unwrap()
            .set_len(MAX_COPY + 1)
            .unwrap();
        let context = context(&root);
        assert!(Manifest::prepare(&context, "large", Some("copy"), false, "copy").is_err());
        let mut path = root.path().join("tree");
        std::fs::create_dir(&path).unwrap();
        for _ in 0..65 {
            path = path.join("x");
            std::fs::create_dir(&path).unwrap();
        }
        assert!(Manifest::prepare(&context, "tree", None, true, "delete").is_err());
        assert!(!root.path().join("copy").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nested_symlinks_and_replaced_parent_directories_cannot_redirect_deletion() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("tree")).unwrap();
        std::fs::create_dir(root.path().join("outside")).unwrap();
        std::fs::write(root.path().join("outside/file"), "outside").unwrap();
        symlink(root.path().join("outside"), root.path().join("tree/link")).unwrap();
        let context = context(&root);
        assert!(Manifest::prepare(&context, "tree", None, true, "delete").is_err());
        std::fs::remove_file(root.path().join("tree/link")).unwrap();
        std::fs::write(root.path().join("tree/file"), "inside").unwrap();
        let manifest = Manifest::prepare(&context, "tree/file", None, false, "delete").unwrap();
        std::fs::rename(root.path().join("tree"), root.path().join("original")).unwrap();
        symlink(root.path().join("outside"), root.path().join("tree")).unwrap();
        assert!(manifest.delete(&context).await.is_err());
        assert_eq!(
            std::fs::read(root.path().join("outside/file")).unwrap(),
            b"outside"
        );
        assert!(root.path().join("original/file").exists());
    }
}
