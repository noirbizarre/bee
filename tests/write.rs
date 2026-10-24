//! File creation and replacement through the native registry.

use bee::tool::{Context, Registry, write::Write};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

async fn write(root: &TempDir, input: Value, budget: usize) -> Value {
    let mut registry = Registry::default();
    registry.register(Write).unwrap();
    let context = Context::new(root.path(), CancellationToken::new(), budget).unwrap();
    let output = registry.invoke("write", &input, &context).await.unwrap();
    assert!(output.json.len() <= budget);
    serde_json::from_str(&output.json).unwrap()
}

#[tokio::test]
async fn complete_contents_are_created_and_replacement_requires_explicit_consent() {
    let root = TempDir::new().unwrap();
    for (name, content) in [("empty", ""), ("text", "\u{feff}héllo\r\nlast")] {
        assert_eq!(
            write(&root, json!({"path":name,"content":content}), 256).await,
            json!({"data":{"bytes_written":content.len()}})
        );
        assert_eq!(
            std::fs::read(root.path().join(name)).unwrap(),
            content.as_bytes()
        );
    }
    assert_eq!(
        write(&root, json!({"path":"text","content":"replacement"}), 256).await["error"]["code"],
        "execution"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("text")).unwrap(),
        "\u{feff}héllo\r\nlast"
    );
    assert_eq!(
        write(
            &root,
            json!({"path":"text","content":"replacement","overwrite":true}),
            256
        )
        .await["data"]["bytes_written"],
        11
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("text")).unwrap(),
        "replacement"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
}

#[tokio::test]
async fn invalid_paths_arguments_and_missing_parents_never_create_files() {
    let root = TempDir::new().unwrap();
    for input in [
        json!({"path":"","content":"x"}),
        json!({"path":"bad\0path","content":"x"}),
        json!({"path":".","content":"x","overwrite":true}),
        json!({"path":"missing/file","content":"x"}),
        json!({"path":"text","content":"x\0y"}),
        json!({"path":"text","content":"x","unknown":true}),
        json!({"path":"text","content":"x","overwrite":"true"}),
        json!({"path":"text","content":"é".repeat(129)}),
    ] {
        assert!(
            write(&root, input.clone(), 256)
                .await
                .get("error")
                .is_some(),
            "{input}"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
    assert_eq!(
        write(&root, json!({"path":"text","content":"é".repeat(128)}), 256).await["data"]["bytes_written"],
        256
    );
    // A regular file cannot be used as a parent even when overwriting is enabled.
    assert_eq!(
        write(
            &root,
            json!({"path":"text/child","content":"x","overwrite":true}),
            256
        )
        .await["error"]["code"],
        "execution"
    );
}

#[tokio::test]
async fn absolute_paths_and_parent_traversal_use_the_explicit_execution_base() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("child")).unwrap();
    for path in [
        root.path().join("absolute"),
        root.path().join("child/../relative"),
    ] {
        assert!(
            write(&root, json!({"path":path,"content":"outside"}), 256)
                .await
                .get("data")
                .is_some()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "outside");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn final_symlinks_and_special_files_are_rejected_without_touching_their_targets() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("target"), "original").unwrap();
    for (link, target) in [("link", "target"), ("dangling", "absent")] {
        std::os::unix::fs::symlink(target, root.path().join(link)).unwrap();
        assert_eq!(
            write(
                &root,
                json!({"path":link,"content":"x","overwrite":true}),
                256
            )
            .await["error"]["code"],
            "execution"
        );
        assert!(
            std::fs::symlink_metadata(root.path().join(link))
                .unwrap()
                .is_symlink()
        );
    }
    let socket = std::os::unix::net::UnixListener::bind(root.path().join("socket")).unwrap();
    assert_eq!(
        write(
            &root,
            json!({"path":"socket","content":"x","overwrite":true}),
            256
        )
        .await["error"]["code"],
        "execution"
    );
    drop(socket);
    assert_eq!(
        std::fs::read_to_string(root.path().join("target")).unwrap(),
        "original"
    );
    assert!(!root.path().join("absent").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn replacements_preserve_modes_and_readonly_files_are_rejected_even_as_root() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let path = root.path().join("text");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
    write(
        &root,
        json!({"path":"text","content":"new","overwrite":true}),
        256,
    )
    .await;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o750
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert_eq!(
        write(
            &root,
            json!({"path":"text","content":"blocked","overwrite":true}),
            256
        )
        .await["error"]["code"],
        "execution"
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "new");
}

#[tokio::test]
async fn cancellation_during_staging_leaves_the_original_and_no_temporary_file() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("text"), "original").unwrap();
    let mut registry = Registry::default();
    registry.register(Write).unwrap();
    let token = CancellationToken::new();
    let context = Context::new(root.path(), token.clone(), 65536).unwrap();
    let input = json!({"path":"text","content":"x".repeat(65536),"overwrite":true});
    let cancel = async {
        tokio::task::yield_now().await;
        token.cancel();
    };
    let (result, ()) = tokio::join!(registry.invoke("write", &input, &context), cancel);
    assert!(matches!(result, Err(bee::tool::Error::Cancelled)));
    assert_eq!(
        std::fs::read_to_string(root.path().join("text")).unwrap(),
        "original"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
