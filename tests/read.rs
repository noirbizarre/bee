//! Observable filesystem reading through the native dispatch boundary.

use bee::tool::{Context, Registry, read::Read};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

async fn read(root: &TempDir, input: Value, budget: usize) -> Value {
    let mut registry = Registry::default();
    registry.register(Read).unwrap();
    let context = Context::new(root.path(), CancellationToken::new(), budget).unwrap();
    let output = registry.invoke("read", &input, &context).await.unwrap();
    assert!(output.json.len() <= budget);
    serde_json::from_str(&output.json).unwrap()
}

#[tokio::test]
async fn line_windows_preserve_numbers_and_report_remaining_content() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("text"), "\u{feff}héllo\r\n\nlast").unwrap();
    assert_eq!(
        read(&root, json!({"path":"text"}), 4096).await,
        json!({"data":{"content":"1: héllo\n2: \n3: last\n","truncated":false,"next_line":null}})
    );
    assert_eq!(
        read(&root, json!({"path":"text","offset":2,"limit":1}), 4096).await,
        json!({"data":{"content":"2: \n","truncated":true,"next_line":3}})
    );
    assert_eq!(
        read(&root, json!({"path":"text","offset":4}), 4096).await["data"]["content"],
        ""
    );
}

#[tokio::test]
async fn byte_budgets_return_valid_partial_results_and_reject_giant_lines() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("text"), "é\"\\\n".repeat(100)).unwrap();
    let output = read(&root, json!({"path":"text"}), 256).await;
    assert_eq!(output["data"]["truncated"], true);
    assert!(!output["data"]["content"].as_str().unwrap().is_empty());
    std::fs::write(root.path().join("text"), "a".repeat(10000)).unwrap();
    assert_eq!(
        read(&root, json!({"path":"text"}), 256).await["error"]["code"],
        "output_limit"
    );
}

#[tokio::test]
async fn unsupported_files_and_invalid_requests_are_actionable_errors() {
    let root = TempDir::new().unwrap();
    for bytes in [vec![0xff], vec![b'a', 0, b'b']] {
        std::fs::write(root.path().join("binary"), bytes).unwrap();
        assert_eq!(
            read(&root, json!({"path":"binary"}), 4096).await["error"]["code"],
            "execution"
        );
    }
    for input in [
        json!({"path":"missing"}),
        json!({"path":"."}),
        json!({"path":"bad\0path"}),
        json!({"path":"binary","offset":0}),
        json!({"path":"binary","limit":0}),
        json!({"path":"binary","offset":usize::MAX,"limit":1}),
    ] {
        assert!(read(&root, input, 4096).await.get("error").is_some());
    }
}

#[tokio::test]
async fn the_execution_base_allows_explicit_paths_outside_it() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("child")).unwrap();
    std::fs::write(root.path().join("text"), "outside").unwrap();
    for path in [root.path().join("text"), root.path().join("child/../text")] {
        assert_eq!(
            read(&root, json!({"path":path}), 4096).await["data"]["content"],
            "1: outside\n"
        );
    }
    std::fs::write(root.path().join("empty"), "").unwrap();
    assert_eq!(
        read(&root, json!({"path":"empty"}), 256).await["data"]["truncated"],
        false
    );
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_symlinks_are_followed_and_dangling_links_fail() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("target"), "linked").unwrap();
    std::os::unix::fs::symlink("target", root.path().join("link")).unwrap();
    assert_eq!(
        read(&root, json!({"path":"link"}), 4096).await["data"]["content"],
        "1: linked\n"
    );
    std::fs::remove_file(root.path().join("target")).unwrap();
    assert_eq!(
        read(&root, json!({"path":"link"}), 4096).await["error"]["code"],
        "execution"
    );
}

#[tokio::test]
async fn cancellation_stops_a_long_range_scan() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("text"), "line\n".repeat(10000)).unwrap();
    let mut registry = Registry::default();
    registry.register(Read).unwrap();
    let token = CancellationToken::new();
    let context = Context::new(root.path(), token.clone(), 256).unwrap();
    let input = json!({"path":"text","offset":9999});
    let call = registry.invoke("read", &input, &context);
    let cancel = async {
        tokio::task::yield_now().await;
        token.cancel();
    };
    let (output, ()) = tokio::join!(call, cancel);
    assert!(matches!(output, Err(bee::tool::Error::Cancelled)));
}
