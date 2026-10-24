//! Exact replacements and failure preservation through native dispatch.

use bee::tool::{Context, Registry, edit::Edit};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

async fn edit(root: &TempDir, input: Value, budget: usize) -> Value {
    let mut registry = Registry::default();
    registry.register(Edit).unwrap();
    let context = Context::new(root.path(), CancellationToken::new(), budget).unwrap();
    let output = registry.invoke("edit", &input, &context).await.unwrap();
    assert!(output.json.len() <= budget);
    serde_json::from_str(&output.json).unwrap()
}

#[tokio::test]
async fn unique_replacements_preserve_bom_mixed_endings_and_final_newline_state() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("text");
    std::fs::write(&path, "\u{feff}héllo\r\nsecond\nlast").unwrap();
    let expected = "\u{feff}héllo\r\nchanged\nlast";
    assert_eq!(
        edit(
            &root,
            json!({"path":"text","old_text":"second","new_text":"changed"}),
            256
        )
        .await,
        json!({"data":{"bytes_written":expected.len()}})
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    assert_eq!(
        edit(
            &root,
            json!({"path":path,"old_text":"changed","new_text":"changed"}),
            256
        )
        .await["data"]["bytes_written"],
        0
    );
    assert_eq!(
        edit(
            &root,
            json!({"path":"text","old_text":"changed\n","new_text":""}),
            256
        )
        .await["data"]["bytes_written"],
        "\u{feff}héllo\r\nlast".len()
    );
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "\u{feff}héllo\r\nlast"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn absent_ambiguous_overlapping_and_line_ending_mismatches_leave_sources_unchanged() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("text");
    for (source, search) in [
        ("hello", "absent"),
        ("one one", "one"),
        ("aaa", "aa"),
        ("ééé", "éé"),
        ("a\r\nb", "a\nb"),
    ] {
        std::fs::write(&path, source).unwrap();
        let result = edit(
            &root,
            json!({"path":"text","old_text":search,"new_text":"replacement"}),
            256,
        )
        .await;
        assert_eq!(result["error"]["code"], "execution");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }
}

#[tokio::test]
async fn invalid_requests_and_unsupported_sources_fail_without_modification() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("text");
    std::fs::write(&path, "original").unwrap();
    for input in [
        json!({"path":"text","old_text":"","new_text":"x"}),
        json!({"path":"text","old_text":"original","new_text":"\0"}),
        json!({"path":"text","old_text":"\0","new_text":"x"}),
        json!({"path":"text","old_text":"original","new_text":"x","unknown":1}),
        json!({"path":"text","old_text":"original","new_text":"é".repeat(129)}),
        json!({"path":"missing","old_text":"x","new_text":"y"}),
        json!({"path":".","old_text":"x","new_text":"y"}),
        json!({"path":"bad\0path","old_text":"x","new_text":"y"}),
    ] {
        assert!(edit(&root, input, 256).await.get("error").is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }
    for bytes in [vec![0xff], vec![b'x', 0], vec![b'x'; 257]] {
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            edit(
                &root,
                json!({"path":"text","old_text":"x","new_text":"y"}),
                256
            )
            .await["error"]["code"],
            "execution"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    std::fs::write(&path, format!("x{}", "a".repeat(255))).unwrap();
    assert_eq!(
        edit(
            &root,
            json!({"path":"text","old_text":"x","new_text":"é"}),
            256
        )
        .await["error"]["code"],
        "invalid_arguments"
    );
    assert_eq!(
        edit(
            &root,
            json!({"path":"text","old_text":"x","new_text":"y"}),
            256
        )
        .await["data"]["bytes_written"],
        256
    );
}

#[cfg(unix)]
#[tokio::test]
async fn editing_symlinks_and_readonly_files_preserves_the_original() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let path = root.path().join("text");
    std::fs::write(&path, "original").unwrap();
    std::os::unix::fs::symlink("text", root.path().join("link")).unwrap();
    assert_eq!(
        edit(
            &root,
            json!({"path":"link","old_text":"original","new_text":"x"}),
            256
        )
        .await["error"]["code"],
        "execution"
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert_eq!(
        edit(
            &root,
            json!({"path":"text","old_text":"original","new_text":"x"}),
            256
        )
        .await["error"]["code"],
        "execution"
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
}

#[tokio::test]
async fn cancellation_during_edit_preserves_the_source_and_removes_staging() {
    let root = TempDir::new().unwrap();
    let original = format!("unique{}", "x".repeat(60000));
    std::fs::write(root.path().join("text"), &original).unwrap();
    let mut registry = Registry::default();
    registry.register(Edit).unwrap();
    let token = CancellationToken::new();
    let context = Context::new(root.path(), token.clone(), 65536).unwrap();
    let input = json!({"path":"text","old_text":"unique","new_text":"changed"});
    let cancel = async {
        // Wait until staging actually exists rather than relying on timing sleeps.
        while std::fs::read_dir(root.path()).unwrap().count() == 1 {
            tokio::task::yield_now().await;
        }
        token.cancel();
    };
    let (result, ()) = tokio::join!(registry.invoke("edit", &input, &context), cancel);
    assert!(matches!(result, Err(bee::tool::Error::Cancelled)));
    assert_eq!(
        std::fs::read_to_string(root.path().join("text")).unwrap(),
        original
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
