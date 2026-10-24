//! Bounded staging shared by mutations; publication is the irreversible boundary.

use std::fs::{File, Metadata};
use std::io::{Read, Write};
use std::path::PathBuf;

use tempfile::NamedTempFile;

use super::{Code, Context, Failure, fs};

pub(super) fn failure(message: &'static str) -> Failure {
    Failure::new(Code::Execution, message)
}

pub(super) fn io(error: std::io::Error) -> Failure {
    // OS errors may contain user paths or contents; expose only deliberate guidance.
    failure(match error.kind() {
        std::io::ErrorKind::NotFound => {
            "Path or parent directory is missing; create parents first or correct the path."
        }
        std::io::ErrorKind::AlreadyExists => {
            "Destination already exists; inspect it and explicitly request overwrite if intended."
        }
        std::io::ErrorKind::PermissionDenied => {
            "Cannot modify the file; check file and parent-directory write permissions."
        }
        _ => {
            "File update failed; check the target, parent directory, permissions and available space."
        }
    })
}

pub(super) fn check(context: &Context) -> Result<(), Failure> {
    if context.cancellation().is_cancelled() {
        return Err(failure(
            "File update cancelled before publication; retry if still needed.",
        ));
    }
    Ok(())
}

pub(super) fn text(value: &str, context: &Context) -> Result<(), Failure> {
    if value.len() > context.output_limit() || value.contains('\0') {
        return Err(Failure::new(
            Code::InvalidArguments,
            "Text must contain no NUL bytes and fit the execution byte budget; use smaller content or increase the budget.",
        ));
    }
    Ok(())
}

fn metadata(path: &std::path::Path) -> Result<Option<Metadata>, Failure> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !meta.permissions().readonly() => Ok(Some(meta)),
        Ok(_) => Err(failure(
            "Expected a writable regular file; symlinks, readonly files, directories and special files cannot be modified.",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(error)),
    }
}

fn same(left: &Metadata, right: &Metadata) -> bool {
    let portable = left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.permissions() == right.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        portable && left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(not(unix))]
    portable
}

pub(super) struct Target {
    path: PathBuf,
    original: Option<Metadata>,
}

pub(super) struct Snapshot {
    pub bytes: String,
    metadata: Metadata,
}

impl Target {
    pub fn new(path: &str, context: &Context) -> Result<Self, Failure> {
        check(context)?;
        let path = fs::path(context, path)?;
        let original = metadata(&path)?;
        Ok(Self { path, original })
    }

    pub fn exists(&self) -> bool {
        self.original.is_some()
    }

    pub fn snapshot(&self, context: &Context) -> Result<Snapshot, Failure> {
        let before = metadata(&self.path)?.ok_or_else(conflict)?;
        let mut file = File::open(&self.path).map_err(io)?;
        let opened = file.metadata().map_err(io)?;
        if !opened.is_file() || !same(&before, &opened) {
            return Err(conflict());
        }
        let mut bytes = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            check(context)?;
            // Probe one byte beyond the bound without allocating an unbounded source buffer.
            let remaining = (context.output_limit() - bytes.len() + 1).min(chunk.len());
            let size = file.read(&mut chunk[..remaining]).map_err(io)?;
            if size == 0 {
                break;
            }
            if size > context.output_limit() - bytes.len() {
                return Err(failure(
                    "Source exceeds the execution byte budget; increase the budget or choose a smaller file.",
                ));
            }
            bytes.extend_from_slice(&chunk[..size]);
        }
        if !same(&opened, &file.metadata().map_err(io)?)
            || !metadata(&self.path)?.is_some_and(|current| same(&opened, &current))
        {
            return Err(conflict());
        }
        let bytes = String::from_utf8(bytes).ok().filter(|s| !s.contains('\0')).ok_or_else(|| {
            failure("Unsupported binary or invalid UTF-8 source; choose a UTF-8 text file without NUL bytes.")
        })?;
        Ok(Snapshot {
            bytes,
            metadata: opened,
        })
    }

    pub fn validate(&self, expected: Option<&Snapshot>, context: &Context) -> Result<(), Failure> {
        check(context)?;
        match (&self.original, metadata(&self.path)?) {
            (None, None) => {}
            (Some(before), Some(now)) if same(before, &now) => {}
            _ => return Err(conflict()),
        }
        if let Some(expected) = expected {
            let current = self.snapshot(context)?;
            if current.bytes != expected.bytes || !same(&current.metadata, &expected.metadata) {
                return Err(conflict());
            }
        }
        check(context)
    }
}

fn conflict() -> Failure {
    failure(
        "Destination changed or disappeared during the update; read it again and retry against its current contents.",
    )
}

pub(super) struct Staged(NamedTempFile);

impl Staged {
    pub async fn new(target: &Target, content: &str, context: &Context) -> Result<Self, Failure> {
        check(context)?;
        let parent = target
            .path
            .parent()
            .ok_or_else(|| failure("Choose a file path with an existing parent directory."))?;
        let mut staged = Self(NamedTempFile::new_in(parent).map_err(io)?);
        for chunk in content.as_bytes().chunks(8192) {
            tokio::task::yield_now().await;
            check(context)?;
            staged.0.write_all(chunk).map_err(io)?;
        }
        // Apply modes after writing so staging does not bypass readonly target checks.
        #[cfg(unix)]
        if let Some(original) = &target.original {
            staged
                .0
                .as_file()
                .set_permissions(original.permissions())
                .map_err(io)?;
        }
        staged.0.as_file().sync_all().map_err(io)?;
        // Even empty writes expose a cancellable checkpoint before publication.
        tokio::task::yield_now().await;
        check(context)?;
        Ok(staged)
    }

    pub fn publish(
        self,
        target: &Target,
        expected: Option<&Snapshot>,
        context: &Context,
    ) -> Result<(), Failure> {
        target.validate(expected, context)?;
        // No await between validation and publication: still optimistic, not compare-and-swap.
        let result = if target.exists() {
            self.0.persist(&target.path)
        } else {
            self.0.persist_noclobber(&target.path)
        };
        result.map(|_| ()).map_err(|error| io(error.error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn context(root: &tempfile::TempDir) -> Context {
        Context::new(root.path(), CancellationToken::new(), 65536).unwrap()
    }

    #[tokio::test]
    async fn concurrent_changes_removal_and_replacement_are_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let context = context(&root);
        let path = root.path().join("text");
        for change in ["content", "remove", "replace", "directory"] {
            std::fs::write(&path, "original").unwrap();
            let target = Target::new("text", &context).unwrap();
            let source = target.snapshot(&context).unwrap();
            let staged = Staged::new(&target, "intended", &context).await.unwrap();
            match change {
                // Same-length changes must not evade conflict detection.
                "content" => std::fs::write(&path, "external").unwrap(),
                "remove" => std::fs::remove_file(&path).unwrap(),
                "replace" => {
                    let other = root.path().join("other");
                    std::fs::write(&other, "external").unwrap();
                    std::fs::remove_file(&path).unwrap();
                    std::fs::rename(other, &path).unwrap();
                }
                _ => {
                    std::fs::remove_file(&path).unwrap();
                    std::fs::create_dir(&path).unwrap();
                }
            }
            assert!(staged.publish(&target, Some(&source), &context).is_err());
            match change {
                "remove" => assert!(!path.exists()),
                "directory" => std::fs::remove_dir(&path).unwrap(),
                _ => {
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), "external");
                    std::fs::remove_file(&path).unwrap();
                }
            }
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn competing_creators_and_failed_publication_preserve_existing_contents() {
        let root = tempfile::tempdir().unwrap();
        let context = context(&root);
        let target = Target::new("text", &context).unwrap();
        let staged = Staged::new(&target, "intended", &context).await.unwrap();
        std::fs::write(root.path().join("text"), "concurrent").unwrap();
        assert!(staged.publish(&target, None, &context).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("text")).unwrap(),
            "concurrent"
        );

        // Remove staging to exercise a real persistence failure after target validation.
        let target = Target::new("text", &context).unwrap();
        let staged = Staged::new(&target, "intended", &context).await.unwrap();
        #[cfg(windows)]
        {
            // Windows can prevent unlinking an open tempfile; cleanup is tested separately.
            drop(staged);
        }
        #[cfg(not(windows))]
        {
            std::fs::remove_file(staged.0.path()).unwrap();
            assert!(staged.publish(&target, None, &context).is_err());
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("text")).unwrap(),
            "concurrent"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cancelled_or_dropped_staging_never_publishes_and_cleans_up() {
        let root = tempfile::tempdir().unwrap();
        let context = context(&root);
        std::fs::write(root.path().join("text"), "original").unwrap();
        let target = Target::new("text", &context).unwrap();
        let content = "x".repeat(65536);
        {
            let mut future = Box::pin(Staged::new(&target, &content, &context));
            assert!(futures::poll!(&mut future).is_pending());
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
            // Dropping the pending future owns cleanup, without an async destructor.
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        let staged = Staged::new(&target, "intended", &context).await.unwrap();
        context.cancellation().cancel();
        assert!(staged.publish(&target, None, &context).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("text")).unwrap(),
            "original"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
