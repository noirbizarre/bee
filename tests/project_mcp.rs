//! Filesystem and secret boundaries of project-owned MCP configuration.
use bee::{
    config::Environment,
    mcp::config::{Transport, discover},
};

fn project() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".agents")).unwrap();
    root
}
fn http(id: &str) -> String {
    format!("[servers.{id}]\nurl='https://example.test/mcp'\n")
}

#[test]
fn canonical_toml_defines_all_servers_and_ignores_unrelated_resource_files() {
    let root = project();
    std::fs::write(
        root.path().join(".agents/mcp.toml"),
        format!("{}{}{}", http("z"), http("a"), http("b")),
    )
    .unwrap();
    std::fs::create_dir(root.path().join(".agents/mcp.d")).unwrap();
    std::fs::write(
        root.path().join(".agents/mcp.d/ignored.toml"),
        "invalid = [",
    )
    .unwrap();
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
fn stdio_command_arrays_preserve_literal_arguments_and_resolve_project_cwd() {
    let root = project();
    let executable = std::env::current_exe().unwrap();
    let arguments = ["--flag", "value with spaces", "", "$HOME; private-sentinel"];
    let command: Vec<_> = std::iter::once(executable.to_str().unwrap())
        .chain(arguments)
        .collect();
    std::fs::create_dir(root.path().join("work")).unwrap();
    std::fs::write(
        root.path().join(".agents/mcp.toml"),
        format!(
            "[servers.local]\ncommand={}\ncwd='work'\n",
            serde_json::to_string(&command).unwrap(),
        ),
    )
    .unwrap();
    let definitions = discover(root.path(), &Environment::default()).unwrap();
    let Transport::Stdio {
        command, args, cwd, ..
    } = &definitions[0].transport
    else {
        panic!()
    };
    assert_eq!(command, &executable.canonicalize().unwrap());
    assert_eq!(args, &arguments);
    assert_eq!(cwd, &root.path().join("work").canonicalize().unwrap());
    assert!(!format!("{definitions:?}").contains("private-sentinel"));
}

#[test]
fn canonical_definitions_reject_empty_commands_ambiguous_transports_and_redundant_fields() {
    let root = project();
    for definition in [
        "command=[]",
        "command=['']",
        "command='private-sentinel'",
        "command=['private-sentinel', 42]",
        "command=['private-sentinel']\nurl='https://example.test'",
        "command=['private-sentinel']\nheaders={Authorization='private-sentinel'}",
        "url='https://example.test'\ncwd='private-sentinel'",
        "url='https://example.test'\nenv={TOKEN='private-sentinel'}",
        "url='https://example.test'\ntype='http'",
        "command=['private-sentinel']\nargs=['private-sentinel']",
        "env={TOKEN='private-sentinel'}",
    ] {
        std::fs::write(
            root.path().join(".agents/mcp.toml"),
            format!("[servers.invalid]\n{definition}\n"),
        )
        .unwrap();
        let error = discover(root.path(), &Environment::default()).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("private-sentinel"));
        assert!(error.to_string().contains("mcp.toml"));
    }
}

#[test]
fn claude_stdio_retains_string_command_and_separate_args_with_optional_type() {
    let root = project();
    let executable = std::env::current_exe().unwrap();
    for explicit_type in [false, true] {
        let mut server =
            serde_json::json!({"command":executable, "args":["--flag", "literal argument"]});
        if explicit_type {
            server["type"] = "stdio".into();
        }
        std::fs::write(
            root.path().join(".mcp.json"),
            serde_json::json!({"mcpServers":{"legacy":server}}).to_string(),
        )
        .unwrap();
        let definitions = discover(root.path(), &Environment::default()).unwrap();
        let Transport::Stdio { command, args, .. } = &definitions[0].transport else {
            panic!()
        };
        assert_eq!(command, &executable.canonicalize().unwrap());
        assert_eq!(args, &["--flag", "literal argument"]);
    }
}

#[test]
fn the_canonical_file_suppresses_claude_fallback_even_when_empty_or_invalid() {
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
fn duplicate_server_tables_are_rejected_without_echoing_identifiers() {
    let root = project();
    std::fs::write(
        root.path().join(".agents/mcp.toml"),
        format!("{}{}", http("private_sentinel"), http("private_sentinel")),
    )
    .unwrap();
    let error = discover(root.path(), &Environment::default()).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("duplicate"));
    assert!(message.contains("mcp.toml"));
    assert!(!format!("{error:?} {error}").contains("private_sentinel"));
}

#[test]
fn unrelated_server_files_do_not_suppress_claude_fallback() {
    let root = project();
    std::fs::create_dir(root.path().join(".agents/mcp.d")).unwrap();
    std::fs::write(
        root.path().join(".agents/mcp.d/ignored.toml"),
        "invalid = [",
    )
    .unwrap();
    std::fs::write(
        root.path().join(".mcp.json"),
        r#"{"mcpServers":{"fallback":{"type":"http","url":"https://example.test"}}}"#,
    )
    .unwrap();
    assert_eq!(
        discover(root.path(), &Environment::default()).unwrap()[0].id,
        "fallback"
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
        r#"{"mcpServers":{"x":{"type":"stdio","url":"https://private-sentinel"}}}"#,
        r#"{"mcpServers":{"x":{"command":"private-sentinel","url":"https://private-sentinel"}}}"#,
        r#"{"mcpServers":{"x":{"url":"https://private-sentinel","args":["private-sentinel"]}}}"#,
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
        "[servers.a]\nurl='http://external.test'",
        "[servers.a]\nurl='https://secret@example.test'",
        "[servers.a]\nurl='https://example.test#secret'",
        "[servers.a]\nurl='https://example.test'\n[servers.a.headers]\nMcp-Session-Id='secret'",
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
