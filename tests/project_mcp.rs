//! Filesystem and secret boundaries of project-owned MCP configuration.
use bee::{
    config::Environment,
    mcp::config::{Transport, discover},
};

fn project() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".agents/mcp.d")).unwrap();
    root
}
fn http(id: &str) -> String {
    format!("[servers.{id}]\ntype='http'\nurl='https://example.test/mcp'\n")
}

#[test]
fn canonical_toml_aggregate_and_sorted_single_server_drop_ins_compose() {
    let root = project();
    std::fs::write(root.path().join(".agents/mcp.toml"), http("a")).unwrap();
    std::fs::write(root.path().join(".agents/mcp.d/z.toml"), http("z")).unwrap();
    std::fs::write(root.path().join(".agents/mcp.d/b.toml"), http("b")).unwrap();
    // JSON is deliberately not a canonical Bee source.
    std::fs::write(root.path().join(".agents/mcp.json"), "not JSON").unwrap();
    let definitions = discover(root.path(), &Environment::default()).unwrap();
    assert_eq!(
        definitions
            .iter()
            .map(|d| d.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "z"]
    );
}

#[test]
fn canonical_files_suppress_claude_fallback_even_when_empty_or_invalid() {
    let root = project();
    std::fs::write(
        root.path().join(".mcp.json"),
        r#"{"mcpServers":{"fallback":{"type":"http","url":"https://example.test"}}}"#,
    )
    .unwrap();
    assert_eq!(
        discover(root.path(), &Environment::default())
            .unwrap()
            .len(),
        1
    );
    let canonical = root.path().join(".agents/mcp.toml");
    std::fs::write(&canonical, "").unwrap();
    assert!(
        discover(root.path(), &Environment::default())
            .unwrap()
            .is_empty()
    );
    std::fs::write(&canonical, "invalid = [").unwrap();
    assert!(discover(root.path(), &Environment::default()).is_err());
}

#[test]
fn duplicates_name_both_sources_without_echoing_identifiers() {
    let root = project();
    std::fs::write(
        root.path().join(".agents/mcp.toml"),
        http("private_sentinel"),
    )
    .unwrap();
    std::fs::write(
        root.path().join(".agents/mcp.d/second.toml"),
        http("private_sentinel"),
    )
    .unwrap();
    let error = discover(root.path(), &Environment::default()).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("duplicate"));
    assert!(message.contains("mcp.toml") && message.contains("second.toml"));
    assert!(!format!("{error:?} {error}").contains("private_sentinel"));
}

#[test]
fn each_drop_in_has_exactly_one_definition() {
    let root = project();
    std::fs::write(root.path().join(".agents/mcp.d/empty.toml"), "").unwrap();
    assert!(
        discover(root.path(), &Environment::default())
            .unwrap_err()
            .to_string()
            .contains("exactly one")
    );
}

#[test]
fn git_files_and_directories_select_the_nearest_project_boundary() {
    let root = project();
    std::fs::write(root.path().join(".git"), "gitdir: private").unwrap();
    std::fs::write(root.path().join(".agents/mcp.toml"), http("outer")).unwrap();
    let nested = root.path().join("nested/deeper");
    std::fs::create_dir_all(&nested).unwrap();
    assert_eq!(
        discover(&nested, &Environment::default()).unwrap()[0].id,
        "outer"
    );
    std::fs::create_dir_all(root.path().join("nested/.git")).unwrap();
    assert!(
        discover(&nested, &Environment::default())
            .unwrap()
            .is_empty()
    );
    std::fs::remove_dir(root.path().join("nested/.git")).unwrap();
    std::fs::remove_file(root.path().join(".git")).unwrap();
    assert!(
        discover(&nested, &Environment::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn environment_references_resolve_without_disclosing_values_in_debug_or_errors() {
    let root = project();
    std::fs::write(
        root.path().join(".agents/mcp.toml"),
        format!(
            "{}\n[servers.api.headers]\nAuthorization={{env='PRIVATE_TOKEN'}}\n",
            http("api")
        ),
    )
    .unwrap();
    let environment = Environment::default().with_variable("PRIVATE_TOKEN", "private-sentinel");
    let definitions = discover(root.path(), &environment).unwrap();
    assert!(!format!("{definitions:?}").contains("private-sentinel"));
    let Transport::Http { headers, .. } = &definitions[0].transport else {
        panic!()
    };
    assert_eq!(headers["authorization"], "private-sentinel");
    let error = discover(root.path(), &Environment::default()).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("PRIVATE_TOKEN"));
}

#[test]
fn claude_duplicate_keys_and_unsupported_fields_are_rejected_without_values() {
    let root = project();
    for json in [
        r#"{"mcpServers":{"x":{"url":"https://private-sentinel","url":"https://private-sentinel"}}}"#,
        r#"{"mcpServers":{"x":{"type":"sse","url":"https://private-sentinel"}}}"#,
        r#"{"mcpServers":{"x":{"url":"https://private-sentinel","secret":"private-sentinel"}}}"#,
    ] {
        std::fs::write(root.path().join(".mcp.json"), json).unwrap();
        let error = discover(root.path(), &Environment::default()).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("private-sentinel"));
    }
}

#[test]
fn unsafe_urls_reserved_headers_and_oversized_sources_are_rejected() {
    let root = project();
    for text in [
        "[servers.a]\ntype='http'\nurl='http://external.test'",
        "[servers.a]\ntype='http'\nurl='https://secret@example.test'",
        "[servers.a]\ntype='http'\nurl='https://example.test#secret'",
        "[servers.a]\ntype='http'\nurl='https://example.test'\n[servers.a.headers]\nMcp-Session-Id='secret'",
    ] {
        std::fs::write(root.path().join(".agents/mcp.toml"), text).unwrap();
        assert!(discover(root.path(), &Environment::default()).is_err());
    }
    std::fs::write(
        root.path().join(".agents/mcp.toml"),
        " ".repeat(1024 * 1024 + 1),
    )
    .unwrap();
    assert!(discover(root.path(), &Environment::default()).is_err());
}

#[cfg(unix)]
#[test]
fn symlinks_cannot_import_executable_configuration_from_outside_the_project() {
    let root = project();
    let external = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(external.path(), http("external")).unwrap();
    std::os::unix::fs::symlink(external.path(), root.path().join(".agents/mcp.toml")).unwrap();
    assert!(
        discover(root.path(), &Environment::default())
            .unwrap_err()
            .to_string()
            .contains("escapes")
    );
}
