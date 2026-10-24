//! Discovery defaults and bounded results through the real native registry.

use bee::tool::{Context, Registry, glob::Glob, list::List};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

async fn invoke(root: &TempDir, tool: &str, input: Value, budget: usize) -> Value {
    let mut registry = Registry::default();
    registry.register(Glob).unwrap();
    registry.register(List).unwrap();
    let context = Context::new(root.path(), CancellationToken::new(), budget).unwrap();
    let output = registry.invoke(tool, &input, &context).await.unwrap();
    assert!(output.json.len() <= budget);
    serde_json::from_str(&output.json).unwrap()
}

fn names(output: &Value) -> Vec<&str> {
    output["data"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn glob_filters_hidden_and_ignored_paths_while_list_shows_every_entry() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("nested")).unwrap();
    std::fs::create_dir(root.path().join(".hidden-dir")).unwrap();
    for path in [
        "z.rs",
        "a.rs",
        "ignored.rs",
        ".hidden.rs",
        "nested/yes.rs",
        "nested/no.rs",
        ".hidden-dir/inside.rs",
    ] {
        std::fs::write(root.path().join(path), "").unwrap();
    }
    std::fs::write(root.path().join(".gitignore"), "ignored.rs\n").unwrap();
    std::fs::write(root.path().join("nested/.gitignore"), "*.rs\n!yes.rs\n").unwrap();
    let found = invoke(&root, "glob", json!({"pattern":"**/*.rs"}), 4096).await;
    assert_eq!(names(&found), ["a.rs", "nested/yes.rs", "z.rs"]);
    let hidden = invoke(
        &root,
        "glob",
        json!({"pattern":"**/*.rs","include_hidden":true}),
        4096,
    )
    .await;
    assert_eq!(
        names(&hidden),
        [
            ".hidden-dir/inside.rs",
            ".hidden.rs",
            "a.rs",
            "nested/yes.rs",
            "z.rs"
        ]
    );
    let ignored = invoke(
        &root,
        "glob",
        json!({"pattern":"**/*.rs","include_ignored":true}),
        4096,
    )
    .await;
    assert_eq!(
        names(&ignored),
        [
            "a.rs",
            "ignored.rs",
            "nested/no.rs",
            "nested/yes.rs",
            "z.rs"
        ]
    );
    let listed = invoke(&root, "list", json!({}), 4096).await;
    assert_eq!(
        names(&listed),
        [
            ".gitignore",
            ".hidden-dir",
            ".hidden.rs",
            "a.rs",
            "ignored.rs",
            "nested",
            "z.rs"
        ]
    );
    assert_eq!(listed["data"]["entries"][5]["kind"], "directory");
}

#[tokio::test]
async fn repository_excludes_and_ignore_files_apply_without_global_configuration() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join(".git/info")).unwrap();
    for path in ["excluded", "ignored", "visible"] {
        std::fs::write(root.path().join(path), "").unwrap();
    }
    std::fs::write(root.path().join(".git/info/exclude"), "excluded\n").unwrap();
    std::fs::write(root.path().join(".ignore"), "ignored\n").unwrap();
    let found = invoke(&root, "glob", json!({"pattern":"*"}), 4096).await;
    assert_eq!(names(&found), ["visible"]);
}

#[tokio::test]
async fn count_and_byte_limits_select_a_deterministic_lexical_prefix() {
    let root = TempDir::new().unwrap();
    // Reverse creation order catches truncation of an arbitrary enumeration prefix.
    for i in (0..100).rev() {
        std::fs::write(root.path().join(format!("{i:03}-é'")), "").unwrap();
    }
    for tool in ["glob", "list"] {
        let arguments = if tool == "glob" {
            json!({"pattern":"*","limit":2})
        } else {
            json!({"limit":2})
        };
        let found = invoke(&root, tool, arguments, 4096).await;
        assert_eq!(names(&found), ["000-é'", "001-é'"]);
        assert_eq!(found["data"]["truncated"], true);
        let arguments = if tool == "glob" {
            json!({"pattern":"*"})
        } else {
            json!({})
        };
        let found = invoke(&root, tool, arguments, 256).await;
        assert_eq!(found["data"]["truncated"], true);
        assert_eq!(names(&found)[0], "000-é'");
    }
}

#[tokio::test]
async fn glob_syntax_is_root_relative_and_errors_are_actionable() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    for path in ["a.rs", "b.txt", "dir/c.rs"] {
        std::fs::write(root.path().join(path), "").unwrap();
    }
    for (pattern, expected) in [
        ("*.rs", vec!["a.rs"]),
        ("**/*.{rs,txt}", vec!["a.rs", "b.txt", "dir/c.rs"]),
        ("[ab].?s", vec!["a.rs"]),
        ("none", vec![]),
    ] {
        let output = invoke(&root, "glob", json!({"pattern":pattern}), 4096).await;
        assert_eq!(names(&output), expected);
        assert_eq!(output["data"]["truncated"], false);
    }
    for input in [
        json!({"pattern":"["}),
        json!({"pattern":"*","path":"missing"}),
        json!({"pattern":"*","path":"a.rs"}),
        json!({"pattern":"*","limit":0}),
        json!({"pattern":"*","limit":10001}),
    ] {
        assert!(
            invoke(&root, "glob", input, 4096)
                .await
                .get("error")
                .is_some()
        );
    }
    let output = invoke(
        &root,
        "list",
        json!({"path":root.path().join("dir/..").join("dir")}),
        4096,
    )
    .await;
    assert_eq!(names(&output), ["c.rs"]);
    assert!(
        invoke(&root, "list", json!({"path":"a.rs"}), 4096)
            .await
            .get("error")
            .is_some()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_are_reported_without_recursive_traversal() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    std::fs::write(root.path().join("dir/file"), "").unwrap();
    for (target, link) in [("dir", "link"), (".", "dir/loop"), ("absent", "dangling")] {
        std::os::unix::fs::symlink(target, root.path().join(link)).unwrap();
    }
    let output = invoke(&root, "glob", json!({"pattern":"**/*"}), 4096).await;
    assert_eq!(names(&output), ["dangling", "dir/file", "dir/loop", "link"]);
    assert_eq!(output["data"]["entries"][0]["kind"], "symlink");
    for tool in ["glob", "list"] {
        let input = if tool == "glob" {
            json!({"pattern":"*","path":"link"})
        } else {
            json!({"path":"link"})
        };
        assert_eq!(
            names(&invoke(&root, tool, input, 4096).await),
            ["file", "loop"]
        );
    }
}

// macOS filesystems reject this fixture at creation with an illegal byte sequence.
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn ambiguous_non_utf8_names_are_rejected_without_lossy_results() {
    use std::os::unix::ffi::OsStringExt;
    let root = TempDir::new().unwrap();
    std::fs::write(
        root.path().join(std::ffi::OsString::from_vec(vec![0xff])),
        "",
    )
    .unwrap();
    for tool in ["glob", "list"] {
        let input = if tool == "glob" {
            json!({"pattern":"*"})
        } else {
            json!({})
        };
        assert_eq!(
            invoke(&root, tool, input, 4096).await["error"]["code"],
            "execution"
        );
    }
    let directory = root.path().join(std::ffi::OsString::from_vec(vec![0xfe]));
    std::fs::create_dir(&directory).unwrap();
    std::fs::remove_file(root.path().join(std::ffi::OsString::from_vec(vec![0xff]))).unwrap();
    assert_eq!(
        invoke(&root, "glob", json!({"pattern":"*"}), 4096).await["error"]["code"],
        "execution"
    );
}

#[tokio::test]
async fn discovery_cancellation_aborts_instead_of_returning_partial_success() {
    let root = TempDir::new().unwrap();
    for i in 0..100 {
        std::fs::write(root.path().join(i.to_string()), "").unwrap();
    }
    let mut registry = Registry::default();
    registry.register(Glob).unwrap();
    registry.register(List).unwrap();
    for tool in ["glob", "list"] {
        let token = CancellationToken::new();
        let context = Context::new(root.path(), token.clone(), 256).unwrap();
        let input = if tool == "glob" {
            json!({"pattern":"*"})
        } else {
            json!({})
        };
        let cancel = async {
            tokio::task::yield_now().await;
            token.cancel();
        };
        let (output, ()) = tokio::join!(registry.invoke(tool, &input, &context), cancel);
        assert!(matches!(output, Err(bee::tool::Error::Cancelled)));
    }
}

#[tokio::test]
async fn variable_length_entries_have_order_independent_byte_truncation() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let paths = ["a".repeat(80), "b".repeat(100), "c".repeat(60), "d".into()];
    for name in &paths {
        std::fs::write(first.path().join(name), "").unwrap();
    }
    for name in paths.iter().rev() {
        std::fs::write(second.path().join(name), "").unwrap();
    }
    for tool in ["glob", "list"] {
        let input = if tool == "glob" {
            json!({"pattern":"*"})
        } else {
            json!({})
        };
        let one = invoke(&first, tool, input.clone(), 256).await;
        let two = invoke(&second, tool, input, 256).await;
        assert_eq!(one, two);
        assert_eq!(names(&one), [paths[0].as_str()]);
    }
}

#[tokio::test]
async fn depth_limited_traversal_explicitly_reports_incompleteness() {
    let root = TempDir::new().unwrap();
    let mut path = root.path().to_path_buf();
    for _ in 0..65 {
        path.push("d");
    }
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("file"), "").unwrap();
    let output = invoke(&root, "glob", json!({"pattern":"**/*"}), 4096).await;
    assert_eq!(output["data"]["truncated"], true);
    assert!(names(&output).is_empty());
}

#[tokio::test]
async fn malformed_ignore_rules_are_not_silently_discarded() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join(".ignore"), "{invalid\n").unwrap();
    let output = invoke(&root, "glob", json!({"pattern":"*"}), 4096).await;
    assert_eq!(output["error"]["code"], "execution");
}

#[cfg(unix)]
#[tokio::test]
async fn inaccessible_directories_produce_errors_when_permissions_are_enforced() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let child = root.path().join("locked");
    std::fs::create_dir(&child).unwrap();
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o0)).unwrap();
    // Privileged users can read mode-000 directories; exercise the error only
    // when the OS actually denies access, and always restore cleanup access.
    let denied = std::fs::read_dir(&child).is_err();
    let glob = invoke(&root, "glob", json!({"pattern":"**/*"}), 4096).await;
    let list = invoke(&root, "list", json!({"path":"locked"}), 4096).await;
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
    if denied {
        assert_eq!(glob["error"]["code"], "execution");
        assert_eq!(list["error"]["code"], "execution");
    }
}
