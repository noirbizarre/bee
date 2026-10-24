//! Embedded search contracts exercised through real registry validation/dispatch.

use bee::tool::{Context, Error, Registry, grep::Grep};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

async fn invoke(root: &TempDir, input: Value, budget: usize) -> Value {
    let mut registry = Registry::default();
    registry.register(Grep).unwrap();
    let context = Context::new(root.path(), CancellationToken::new(), budget).unwrap();
    let output = registry.invoke("grep", &input, &context).await.unwrap();
    assert!(output.json.len() <= budget);
    serde_json::from_str(&output.json).unwrap()
}

fn paths(output: &Value) -> Vec<&str> {
    output["data"]["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn unicode_regexes_match_normalized_numbered_lines_once() {
    let root = TempDir::new().unwrap();
    std::fs::write(
        root.path().join("note"),
        "\u{feff}Écho Écho\r\n\r\necho\nlast",
    )
    .unwrap();
    std::fs::write(root.path().join("empty"), "").unwrap();
    let output = invoke(
        &root,
        json!({"pattern":"^écho.*$","case_insensitive":true}),
        4096,
    )
    .await;
    assert_eq!(
        output,
        json!({"data":{"matches":[{"path":"note","line_number":1,"line":"Écho Écho"}],"truncated":false,"skipped_binary_files":0}})
    );
    assert!(paths(&invoke(&root, json!({"pattern":"écho"}), 4096).await).is_empty());
    let output = invoke(&root, json!({"pattern":"^$|last$"}), 4096).await;
    assert_eq!(output["data"]["matches"][0]["line_number"], 2);
    assert_eq!(output["data"]["matches"][1]["line_number"], 4);
    assert_eq!(output["data"]["matches"][1]["line"], "last");
}

#[tokio::test]
async fn filters_do_not_override_local_ignores_or_hidden_policy() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join("nested")).unwrap();
    std::fs::create_dir_all(root.path().join(".hidden-dir")).unwrap();
    std::fs::create_dir_all(root.path().join(".git/info")).unwrap();
    for path in [
        "a.rs",
        "b.txt",
        "ignored.rs",
        "excluded.rs",
        "local.rs",
        ".hidden.rs",
        "nested/yes.rs",
        "nested/no.rs",
        ".hidden-dir/a.rs",
    ] {
        std::fs::write(root.path().join(path), "needle").unwrap();
    }
    std::fs::write(root.path().join(".gitignore"), "ignored.rs\n").unwrap();
    std::fs::write(root.path().join(".ignore"), "local.rs\n").unwrap();
    std::fs::write(root.path().join(".git/info/exclude"), "excluded.rs\n").unwrap();
    std::fs::write(root.path().join("nested/.gitignore"), "*.rs\n!yes.rs\n").unwrap();
    let output = invoke(
        &root,
        json!({"pattern":"needle","include":["**/*.rs"]}),
        4096,
    )
    .await;
    assert_eq!(paths(&output), ["a.rs", "nested/yes.rs"]);
    let output = invoke(&root, json!({"pattern":"needle","include":["**/*.rs"],"include_hidden":true,"exclude":["**/yes.rs"]}), 4096).await;
    assert_eq!(paths(&output), [".hidden-dir/a.rs", ".hidden.rs", "a.rs"]);
    let output = invoke(&root, json!({"pattern":"NEEDLE","case_insensitive":true,"include_ignored":true,"include":["**/*.rs"],"exclude":["a.rs"]}), 4096).await;
    assert_eq!(
        paths(&output),
        [
            "excluded.rs",
            "ignored.rs",
            "local.rs",
            "nested/no.rs",
            "nested/yes.rs"
        ]
    );
    assert!(
        paths(
            &invoke(
                &root,
                json!({"pattern":"needle","case_insensitive":true,"include":["**/*.RS"]}),
                4096
            )
            .await
        )
        .is_empty()
    );
}

#[tokio::test]
async fn local_ignore_rules_also_apply_outside_git_checkouts() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join(".gitignore"), "ignored\n").unwrap();
    for path in ["visible", "ignored"] {
        std::fs::write(root.path().join(path), "needle").unwrap();
    }
    assert_eq!(
        paths(&invoke(&root, json!({"pattern":"needle"}), 4096).await),
        ["visible"]
    );
}

#[tokio::test]
async fn late_binary_bytes_discard_staged_matches_without_evicting_text_results() {
    let root = TempDir::new().unwrap();
    for (name, suffix) in [("a-nul", vec![0]), ("b-invalid", vec![0xff])] {
        let mut bytes = b"needle\n".repeat(2000);
        bytes.extend(suffix);
        std::fs::write(root.path().join(name), bytes).unwrap();
    }
    std::fs::write(root.path().join("c-text"), "needle").unwrap();
    let output = invoke(&root, json!({"pattern":"needle","limit":1}), 256).await;
    assert_eq!(paths(&output), ["c-text"]);
    assert_eq!(output["data"]["skipped_binary_files"], 2);
    assert_eq!(output["data"]["truncated"], false);
}

#[tokio::test]
async fn count_and_escaped_byte_limits_select_a_numeric_line_ordered_prefix() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    for (root, names) in [(&first, ["z", "a"]), (&second, ["a", "z"])] {
        for name in names {
            std::fs::write(root.path().join(name), "é\t\"\\\n".repeat(12)).unwrap();
        }
    }
    for budget in [256, 4096] {
        let input = json!({"pattern":".","limit":11});
        let one = invoke(&first, input.clone(), budget).await;
        let two = invoke(&second, input, budget).await;
        assert_eq!(one, two);
        assert_eq!(one["data"]["truncated"], true);
        assert!(paths(&one).iter().all(|path| *path == "a"));
        for (index, entry) in one["data"]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            assert_eq!(entry["line_number"], index + 1);
        }
        if budget == 4096 {
            assert_eq!(one["data"]["matches"][10]["line_number"], 11);
        }
    }
}

#[tokio::test]
async fn an_oversized_first_match_preserves_the_lexical_gap() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("z"), "x").unwrap();
    std::fs::write(root.path().join("a"), "\t".repeat(200)).unwrap();
    let output = invoke(&root, json!({"pattern":"."}), 256).await;
    assert_eq!(paths(&output), Vec::<&str>::new());
    assert_eq!(output["data"]["truncated"], true);
}

#[tokio::test]
async fn oversized_logical_lines_fail_even_without_a_match() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("file"), "x".repeat(8193)).unwrap();
    assert_eq!(
        invoke(&root, json!({"pattern":"absent"}), 4096).await["error"]["code"],
        "output_limit"
    );
}

#[tokio::test]
async fn invalid_inputs_and_regex_complexity_fail_without_echoing_patterns() {
    let root = TempDir::new().unwrap();
    for input in [
        json!({"pattern":"[secret"}),
        json!({"pattern":"(?=secret)"}),
        json!({"pattern":"(secret)\\1"}),
        json!({"pattern":"a\\nb"}),
        json!({"pattern":"\\pL{10000}"}),
        json!({"pattern":""}),
        json!({"pattern":"é".repeat(3000)}),
        json!({"pattern":"x","limit":0}),
        json!({"pattern":"x","limit":10001}),
        json!({"pattern":"x","unknown":true}),
        json!({"pattern":"x","include":["[secret"]}),
        json!({"pattern":"x","exclude":[""]}),
        json!({"pattern":"x","include":vec!["*";101]}),
        json!({"pattern":"x","exclude":["é".repeat(3000)]}),
    ] {
        let output = invoke(&root, input, 4096).await;
        assert_eq!(output["error"]["code"], "invalid_arguments");
        assert!(!output.to_string().contains("secret"));
    }
}

#[tokio::test]
async fn explicit_roots_resolve_against_the_execution_context() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("nested")).unwrap();
    std::fs::write(root.path().join("nested/file"), "needle").unwrap();
    for path in [json!("nested"), json!(root.path().join("nested/../nested"))] {
        assert_eq!(
            paths(&invoke(&root, json!({"pattern":"needle","path":path}), 4096).await),
            ["file"]
        );
    }
    for path in ["missing", "nested/file"] {
        assert_eq!(
            invoke(&root, json!({"pattern":"needle","path":path}), 4096).await["error"]["code"],
            "execution"
        );
    }
    std::fs::write(root.path().join(".ignore"), "{invalid\n").unwrap();
    assert_eq!(
        invoke(&root, json!({"pattern":"needle"}), 4096).await["error"]["code"],
        "execution"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn nested_symlinks_and_special_files_are_skipped_but_explicit_roots_are_followed() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    std::fs::write(root.path().join("dir/file"), "needle").unwrap();
    for (target, link) in [
        ("dir", "link"),
        ("file", "dir/file-link"),
        (".", "dir/loop"),
        ("absent", "dangling"),
    ] {
        std::os::unix::fs::symlink(target, root.path().join(link)).unwrap();
    }
    let _socket = std::os::unix::net::UnixListener::bind(root.path().join("socket")).unwrap();
    assert_eq!(
        paths(&invoke(&root, json!({"pattern":"needle"}), 4096).await),
        ["dir/file"]
    );
    assert_eq!(
        paths(&invoke(&root, json!({"pattern":"needle","path":"link"}), 4096).await),
        ["file"]
    );
}

// macOS filesystems reject the fixture before search can inspect it.
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn non_utf8_names_fail_instead_of_returning_ambiguous_paths() {
    use std::os::unix::ffi::OsStringExt;
    let root = TempDir::new().unwrap();
    std::fs::write(
        root.path().join(std::ffi::OsString::from_vec(vec![0xff])),
        "needle",
    )
    .unwrap();
    assert_eq!(
        invoke(&root, json!({"pattern":"needle"}), 4096).await["error"]["code"],
        "execution"
    );
}

#[tokio::test]
async fn depth_limited_search_explicitly_reports_incompleteness() {
    let root = TempDir::new().unwrap();
    let mut path = root.path().to_path_buf();
    for _ in 0..65 {
        path.push("d");
    }
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("file"), "needle").unwrap();
    let output = invoke(&root, json!({"pattern":"needle"}), 4096).await;
    assert!(paths(&output).is_empty());
    assert_eq!(output["data"]["truncated"], true);
}

#[tokio::test]
async fn cancellation_interrupts_a_single_large_nonmatching_file() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("file"), "no match here\n".repeat(100000)).unwrap();
    let mut registry = Registry::default();
    registry.register(Grep).unwrap();
    let input = json!({"pattern":"needle"});
    let token = CancellationToken::new();
    let context = Context::new(root.path(), token.clone(), 4096).unwrap();
    let cancel = async {
        // Allow traversal to reach scanning before cancellation on one runtime thread.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        token.cancel();
    };
    let (output, ()) = tokio::join!(registry.invoke("grep", &input, &context), cancel);
    assert!(matches!(output, Err(Error::Cancelled)));
    assert!(matches!(
        registry.invoke("grep", &input, &context).await,
        Err(Error::Cancelled)
    ));
}

#[tokio::test]
async fn dropping_search_leaves_no_detached_scanner_or_unrelated_cancellation() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("file"), "no match here\n".repeat(10000)).unwrap();
    let mut registry = Registry::default();
    registry.register(Grep).unwrap();
    let token = CancellationToken::new();
    let context = Context::new(root.path(), token.clone(), 4096).unwrap();
    let input = json!({"pattern":"needle"});
    {
        let pending = registry.invoke("grep", &input, &context);
        tokio::pin!(pending);
        assert!(futures::poll!(pending.as_mut()).is_pending());
    }
    assert!(!token.is_cancelled());
    // A fresh invocation works after the abandoned dispatch released its file state.
    assert!(registry.invoke("grep", &input, &context).await.is_ok());
}
