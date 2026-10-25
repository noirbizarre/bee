//! The stdio transport, exercised through the real binary.
//!
//! These spawn `bee` the way an ACPv2 client would, because the properties
//! under test (a clean stdout, exit codes, shutdown on EOF) belong to the
//! process, not to the library.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

// Every wait is bounded: a hung agent must fail the test, not stall CI.
const TIMEOUT: Duration = Duration::from_secs(20);

/// A running `bee` and the plumbing to talk to it.
struct Agent {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Raw stdout lines, in order.
    lines: Receiver<String>,
    stderr: Option<thread::JoinHandle<String>>,
    _isolated: tempfile::TempDir,
}

impl Agent {
    fn spawn() -> Self {
        Self::configured("")
    }

    fn configured(config: &str) -> Self {
        // An empty directory as home, config home and working directory, so a
        // developer's real configuration (or a malformed one) cannot change
        // what these tests observe. Keep it alive until the child is reaped.
        let isolated = tempfile::tempdir().expect("a temp directory");
        let path = bee::config::project_config_path(isolated.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, support::native_bash_config(config)).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_bee"))
            .env("HOME", isolated.path())
            .env("XDG_CONFIG_HOME", isolated.path().join("xdg"))
            .env_remove("BEE_CONFIG")
            .current_dir(isolated.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the bee binary starts");

        let stdout = child.stdout.take().expect("stdout is piped");
        let (sender, lines) = mpsc::channel();
        // A reader thread, because a blocking `read_line` on the test thread
        // could not be given a timeout.
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });

        let stdin = child.stdin.take();
        let mut stderr = child.stderr.take().expect("stderr is piped");
        // Drain while the child runs, so diagnostics cannot fill the pipe and
        // deadlock protocol progress. Retain only a bounded diagnostic prefix.
        let stderr = thread::spawn(move || {
            let mut captured = Vec::new();
            let mut buffer = [0; 8192];
            while let Ok(count) = stderr.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let keep = count.min((64 * 1024_usize).saturating_sub(captured.len()));
                captured.extend_from_slice(&buffer[..keep]);
            }
            String::from_utf8_lossy(&captured).into_owned()
        });
        Self {
            child,
            stdin,
            lines,
            stderr: Some(stderr),
            _isolated: isolated,
        }
    }

    fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is still open");
        writeln!(stdin, "{line}").expect("bee reads its stdin");
        stdin.flush().expect("bee reads its stdin");
    }

    fn send(&mut self, message: &Value) {
        self.send_raw(&message.to_string());
    }

    /// The next stdout line, which must be a JSON-RPC message.
    fn recv(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(TIMEOUT)
            .expect("bee answers within the timeout");
        serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("stdout line is not JSON ({error}): {line:?}"))
    }

    fn initialize(&mut self, id: u64) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": 2,
                "info": { "name": "bee-test", "version": "0" },
            },
        }));
        self.recv()
    }

    /// Close stdin, as a client does to say it is done, and wait for exit.
    fn finish(mut self) -> (std::process::ExitStatus, String) {
        drop(self.stdin.take());

        // Polled rather than `wait()`ed, so a process that ignores EOF fails
        // the test instead of hanging it.
        let deadline = std::time::Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("the child can be polled") {
                break status;
            }
            if std::time::Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("bee did not exit after its stdin closed");
            }
            thread::sleep(Duration::from_millis(20));
        };

        let stderr = self.stderr.take().unwrap().join().unwrap();
        (status, stderr)
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // Assertion failures must not leave a child holding temporary files.
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[rstest::rstest]
#[case::cancel(false)]
#[case::close(true)]
fn pending_inference_can_be_stopped_over_stdio_and_the_session_recovers(#[case] close: bool) {
    let root = tempfile::tempdir().unwrap();
    let (pending, release) = support::Response::json(support::chat_text("late answer")).gated();
    let http = support::Server::start(vec![
        pending,
        support::Response::json(support::chat_text("recovered")),
    ]);
    let mut agent = Agent::configured(&format!(
        "[providers.test]\nmodel='test'\n[providers.test.protocols.openai-chat]\nbase_url='{}'",
        http.url
    ));
    // Supporting interaction modes on the client does not make Bee advertise
    // or invoke flows it has not implemented.
    agent.send(
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":2,"info":{"name":"compatibility-test","version":"0"},
            "capabilities":{"elicitation":{"form":{},"url":{}}}
        }}),
    );
    let initialized = agent.recv();
    assert_eq!(
        initialized["result"]["capabilities"],
        json!({"session":{"mcp":{"acp":{}}}})
    );
    agent
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"cancelled-question"}]}}));
    assert_eq!(agent.recv()["id"], 3);
    http.request();
    if close {
        agent.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/close","params":{"sessionId":session}}));
    } else {
        agent.send(
            &json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}),
        );
    }
    loop {
        let message = agent.recv();
        assert_eq!(message["method"], "session/update");
        assert_eq!(message["params"]["sessionId"], session);
        let update = &message["params"]["update"];
        assert_ne!(update["sessionUpdate"], "agent_message_chunk");
        if update["state"] == "idle" {
            assert_eq!(update["stopReason"], "cancelled");
            break;
        }
    }
    if !close {
        agent.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/close","params":{"sessionId":session}}));
    }
    assert_eq!(agent.recv()["id"], 4, "idle precedes the close response");
    agent.send(&json!({"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"closed"}]}}));
    assert_eq!(agent.recv()["error"]["code"], -32602);
    agent.send(&json!({"jsonrpc":"2.0","id":6,"method":"session/resume","params":{"sessionId":session,"cwd":root.path()}}));
    let snapshot = agent.recv();
    assert_eq!(snapshot["params"]["update"]["state"], "idle");
    assert!(snapshot["params"]["update"].get("stopReason").is_none());
    assert_eq!(agent.recv()["id"], 6);
    release.send(()).unwrap();
    agent.send(&json!({"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"recover"}]}}));
    assert_eq!(agent.recv()["id"], 7);
    let mut text = String::new();
    loop {
        let message = agent.recv();
        assert_eq!(message["method"], "session/update");
        let update = &message["params"]["update"];
        if let Some(chunk) = update["content"]["text"].as_str() {
            text.push_str(chunk);
        }
        if update["state"] == "idle" {
            assert_eq!(update["stopReason"], "end_turn");
            break;
        }
    }
    assert_eq!(text, "recovered");
    let recovered = http.request().body.to_string();
    assert!(!recovered.contains("cancelled-question"));
    assert!(!recovered.contains("late answer"));
    agent.send(&json!({"jsonrpc":"2.0","id":8,"method":"session/list","params":{}}));
    let listed = agent.recv();
    assert_eq!(
        listed["id"], 8,
        "no duplicate idle or late answer follows recovery"
    );
    assert_eq!(listed["result"]["sessions"][0]["sessionId"], session);
    let (status, stderr) = agent.finish();
    assert!(status.success());
    assert!(stderr.is_empty());
}

#[test]
fn resume_cursors_preserve_wire_order_and_reject_invalid_requests_without_reopening() {
    let server = support::Server::start(vec![support::Response::json(support::chat_text(
        "previous answer",
    ))]);
    let mut agent = Agent::configured(&format!(
        "[providers.test]\nmodel='test'\n[providers.test.protocols.openai-chat]\nbase_url='{}'",
        server.url
    ));
    agent.initialize(1);
    let cwd = agent._isolated.path().to_path_buf();
    agent.send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"previous question"}]}}));
    assert_eq!(agent.recv()["id"], 3);
    while agent.recv()["params"]["update"]["state"] != "idle" {}
    server.request();

    for cursor in [None, Some(Value::Null), Some(json!({"type":"start"}))] {
        agent.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/close","params":{"sessionId":session}}));
        assert_eq!(agent.recv()["id"], 4);
        for invalid in [
            json!({"type":"future_cursor"}),
            json!("start"),
            json!({}),
            json!({"type":5}),
        ] {
            agent.send(&json!({"jsonrpc":"2.0","id":5,"method":"session/resume","params":{"sessionId":session,"cwd":cwd,"replayFrom":invalid}}));
            let rejected = agent.recv();
            assert_eq!(rejected["id"], 5);
            assert_eq!(rejected["error"]["code"], -32602);
            // A rejected resume must not publish a snapshot or reopen the session.
            agent.send(&json!({"jsonrpc":"2.0","id":6,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"still closed"}]}}));
            let closed = agent.recv();
            assert_eq!(closed["id"], 6);
            assert_eq!(closed["error"]["code"], -32602);
        }
        let mut params = json!({"sessionId":session,"cwd":cwd});
        if let Some(cursor) = cursor {
            params["replayFrom"] = cursor;
        }
        agent.send(&json!({"jsonrpc":"2.0","id":7,"method":"session/resume","params":params}));
        let snapshot = agent.recv();
        assert_eq!(snapshot["method"], "session/update");
        assert_eq!(snapshot["params"]["sessionId"], session);
        assert_eq!(snapshot["params"]["update"]["state"], "idle");
        let resumed = agent.recv();
        assert_eq!(
            resumed["id"], 7,
            "only the current snapshot precedes the response"
        );
        assert!(resumed.get("result").is_some());
        // An ordered request detects any trailing history or duplicate snapshot.
        agent.send(&json!({"jsonrpc":"2.0","id":8,"method":"session/list","params":{}}));
        assert_eq!(agent.recv()["id"], 8);
    }
    let (status, stderr) = agent.finish();
    assert!(status.success());
    assert!(stderr.is_empty());
}

#[test]
fn bash_output_is_captured_without_corrupting_the_protocol_stream() {
    let server = support::Server::start(vec![
        support::Response::json(
            // Use only shell builtins so the Windows direct-child guarantee is sufficient.
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"bash-call","type":"function","function":{"name":"bash","arguments":"{\"command\":\"printf child-stdout; printf child-stderr >&2; while :; do :; done\"}"}}]},"finish_reason":"tool_calls"}]}),
        ),
        support::Response::json(
            json!({"choices":[{"index":0,"message":{"content":"done"},"finish_reason":"stop"}]}),
        ),
    ]);
    let mut agent = Agent::configured(&format!(
        "[tools.bash]\ntimeout_ms=1000\n[providers.test]\nmodel='test'\n[providers.test.protocols.openai-chat]\nbase_url='{}'",
        server.url
    ));
    agent.initialize(1);
    agent.send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":std::env::temp_dir()}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"run"}]}}));
    loop {
        // recv parses every stdout line, including any accidental child output.
        let message = agent.recv();
        if message["method"] == "session/request_permission" {
            assert_eq!(message["params"]["subject"]["toolCallId"], "bash-call");
            agent.send(&json!({"jsonrpc":"2.0","id":message["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}));
        }
        if message["params"]["update"]["state"] == "idle" {
            break;
        }
    }
    server.request();
    let result = server.request().body["messages"].to_string();
    assert!(result.contains("child-stdout"));
    assert!(result.contains("child-stderr"));
    assert!(result.contains("timed_out"));
    let (status, stderr) = agent.finish();
    assert!(status.success());
    assert!(stderr.is_empty());
}

#[rstest::rstest]
#[case::malformed(false)]
#[case::direct_close(true)]
#[test]
fn unapproved_stdio_commands_never_run_and_close_follows_request_cleanup(#[case] close: bool) {
    let root = tempfile::tempdir().unwrap();
    let server = support::Server::start(vec![
        support::Response::json(
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"protected","type":"function","function":{"name":"bash","arguments":"{\"command\":\"printf bad > sentinel\"}"}}]},"finish_reason":"tool_calls"}]}),
        ),
        support::Response::json(support::chat_text("declined")),
    ]);
    let mut agent = Agent::configured(&format!(
        "[providers.test]\nmodel='test'\nprotocols={{openai-chat={{base_url='{}'}}}}",
        server.url
    ));
    agent.initialize(1);
    agent
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"run"}]}}));
    let permission = loop {
        let message = agent.recv();
        if message["method"] == "session/request_permission" {
            break message;
        }
    };
    if close {
        agent.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/close","params":{"sessionId":session}}));
    } else {
        agent.send(&json!({"jsonrpc":"2.0","id":permission["id"],"result":{}}));
    }
    let mut cancelled = false;
    let mut terminal = false;
    loop {
        let message = agent.recv();
        if message["method"] == "$/cancel_request" {
            assert_eq!(message["params"]["requestId"], permission["id"]);
            assert!(!terminal);
            cancelled = true;
        }
        if message["params"]["update"]["state"] == "idle" {
            assert!(!terminal);
            terminal = true;
            if close {
                assert!(cancelled);
            } else {
                break;
            }
        }
        if message["id"] == 4 {
            assert!(close && terminal);
            assert!(message.get("result").is_some());
            break;
        }
    }
    if close {
        agent.send(&json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}));
        agent.send(&json!({"jsonrpc":"2.0","id":5,"method":"session/list","params":{}}));
        assert_eq!(agent.recv()["id"], 5);
    }
    assert!(!root.path().join("sentinel").exists());
    let (status, stderr) = agent.finish();
    assert!(status.success());
    assert!(stderr.is_empty());
}

#[test]
fn configured_turn_limits_reach_real_stdio_inference() {
    let server = support::Server::start(vec![support::Response::json(
        json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"a","type":"function","function":{"name":"list","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
    )]);
    let mut agent = Agent::configured(&format!(
        "[agent]\nmax_steps = 1\n[providers.test]\nmodel = 'test'\n[providers.test.protocols.openai-chat]\nbase_url = '{}'",
        server.url
    ));
    agent.initialize(1);
    agent.send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":std::env::temp_dir()}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"inspect"}]}}));
    loop {
        let message = agent.recv();
        let update = &message["params"]["update"];
        if update["sessionUpdate"] == "state_update" && update["state"] == "idle" {
            assert_eq!(update["stopReason"], "error");
            assert!(
                update
                    .to_string()
                    .contains("execution or response-size limit")
            );
            break;
        }
    }
    let request = server.request();
    let names: Vec<_> = request.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["bash", "edit", "glob", "grep", "list", "read", "write"]
    );
    server.assert_no_more_requests();
    let (status, stderr) = agent.finish();
    assert!(status.success());
    assert!(stderr.is_empty());
}

#[test]
fn context_truncation_reports_committed_history_and_failures_leave_it_intact_over_stdio() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note.txt"), "paired-result").unwrap();
    let mut overflow = support::Response::json(
        json!({"error":{"code":"context_length_exceeded","message":"upstream-secret"}}),
    );
    overflow.status = 400;
    let server = support::Server::start(vec![
        support::Response::json(support::chat_text("first-answer")),
        support::Response::json(support::chat_text("tiny-answer")),
        support::Response::json(
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"paired-call","type":"function","function":{"name":"read","arguments":"{\"path\":\"note.txt\"}"}}]},"finish_reason":"tool_calls"}]}),
        ),
        support::Response::json(support::chat_text("tool-answer")),
        overflow,
        support::Response::json(support::chat_text("after-answer")),
    ]);
    let first = format!(
        "{}\n[Resource link, contents not fetched: file:///reference]\nlast block",
        "old".repeat(1000)
    );
    let model = bee::llm::Model::new(
        support::profile("openai-chat", &server.url, "max_output_tokens=20"),
        "test".into(),
        None,
    )
    .unwrap();
    let registry = bee::tool::builtins().unwrap();
    let executor = bee::agent::NativeExecutor::new(&registry, support::native_context());
    let report = model
        .context(
            &[bee::llm::Message::User(first.clone())],
            &executor.definitions(),
        )
        .unwrap();
    let window = report.estimated_input + report.reserved_output + 400;
    let mut agent = Agent::configured(&format!(
        "[providers.test]\nmodel='test'\nmax_output_tokens=20\ncontext_window={window}\n[providers.test.protocols.openai-chat]\nbase_url='{}'",
        server.url
    ));
    agent.initialize(1);
    agent
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    let prompts = [
        json!([{"type":"text","text":"old".repeat(1000)},{"type":"resource_link","uri":"file:///reference","name":"reference"},{"type":"text","text":"last block"}]),
        json!([{"type":"text","text":"oversized".repeat(10000)}]),
        json!([{"type":"text","text":"tiny"}]),
        json!([{"type":"text","text":"second".repeat(500)}]),
        json!([{"type":"text","text":"rejected-prompt"}]),
        json!([{"type":"text","text":"after"}]),
    ];
    for (index, prompt) in prompts.into_iter().enumerate() {
        agent.send(&json!({"jsonrpc":"2.0","id":index+3,"method":"session/prompt","params":{"sessionId":session,"prompt":prompt}}));
        let mut reports = 0;
        let mut notices = 0;
        loop {
            let message = agent.recv();
            assert!(!message.to_string().contains("upstream-secret"));
            let update = &message["params"]["update"];
            if update["sessionUpdate"] == "usage_update" {
                reports += 1;
                assert_eq!(update["size"], window);
                assert_eq!(update["_meta"]["bee.dev/context"]["estimated"], true);
            }
            if update["sessionUpdate"] == "notice" {
                notices += 1;
                assert!(
                    update
                        .to_string()
                        .contains("Removed 1 oldest complete turn")
                );
            }
            if update["sessionUpdate"] == "state_update" && update["state"] == "idle" {
                let failed = index == 1 || index == 4;
                assert_eq!(
                    update["stopReason"],
                    if failed { "error" } else { "end_turn" }
                );
                assert_eq!(reports, usize::from(!failed));
                assert_eq!(notices, usize::from(index == 3));
                break;
            }
        }
    }
    let request = server.request();
    assert_eq!(request.body["messages"][0]["role"], "system");
    // Persistent instructions precede history without becoming an evictable user turn.
    let user = request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap();
    assert_eq!(user["content"], first);
    let recovered = server.request().body.to_string();
    assert!(recovered.contains("first-answer"));
    assert!(!recovered.contains("oversized"));
    let trimmed = server.request().body.to_string();
    assert!(!trimmed.contains("first-answer"));
    assert!(trimmed.contains("tiny-answer"));
    let paired = server.request().body.to_string();
    assert!(paired.contains("paired-call"));
    assert!(paired.contains("paired-result"));
    server.request();
    let after = server.request().body.to_string();
    assert!(after.contains("tool-answer"));
    assert!(after.contains("paired-call"));
    assert!(!after.contains("rejected-prompt"));
    server.assert_no_more_requests();
    let (status, stderr) = agent.finish();
    assert!(status.success(), "{stderr}");
}

#[test]
fn an_initialize_over_stdio_is_answered_with_the_bee_identity() {
    let mut agent = Agent::spawn();

    let response = agent.initialize(1);

    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["protocolVersion"], 2);
    assert_eq!(response["result"]["info"]["name"], "bee");
    agent.finish();
}

#[test]
fn the_initialize_response_advertises_sessions_and_only_acp_mcp_transport() {
    let mut agent = Agent::spawn();

    let response = agent.initialize(1);

    // Capabilities promise implemented behavior, including the exact MCP transport.
    assert_eq!(
        response["result"]["capabilities"]["session"],
        json!({"mcp":{"acp":{}}})
    );
    agent.finish();
}

#[test]
fn an_unconfigured_prompt_over_stdio_is_accepted_and_reports_an_actionable_failure() {
    let mut agent = Agent::spawn();
    agent.initialize(1);

    agent.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": { "cwd": std::env::temp_dir() },
    }));
    let created = agent.recv();
    let session = created["result"]["sessionId"].clone();
    agent.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": { "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] },
    }));

    // The acceptance response and the four updates are separate messages, so
    // read until the terminal one rather than assuming an interleaving.
    let mut accepted = None;
    let mut states = Vec::new();
    loop {
        let message = agent.recv();
        if message["id"] == 3 {
            accepted = Some(message);
        } else if message["method"] == "session/update" {
            let update = &message["params"]["update"];
            if update["sessionUpdate"] == "state_update" {
                states.push(update.clone());
                if update["state"] == "idle" {
                    break;
                }
            }
        }
    }

    assert!(accepted.is_some_and(|response| response["result"]["messageId"].is_string()));
    assert_eq!(states[0]["state"], "running");
    assert_eq!(states[1]["stopReason"], "error");
    assert!(
        states[1]
            .to_string()
            .contains("configure a provider and model")
    );
    let (status, stderr) = agent.finish();
    assert!(status.success(), "exit status was {status}");
    assert_eq!(stderr, "", "a prompt writes nothing to stderr");
}

#[test]
fn stdout_carries_only_json_rpc_messages() {
    let mut agent = Agent::spawn();

    // `recv` already fails on a non-JSON line; the exchange includes an error
    // path as well, since that is where a stray diagnostic would be printed.
    let first = agent.initialize(1);
    agent.send_raw("not json");
    let second = agent.recv();

    assert_eq!(first["jsonrpc"], "2.0");
    assert_eq!(second["jsonrpc"], "2.0");
    let (_, stderr) = agent.finish();
    assert_eq!(stderr, "", "a normal session writes nothing to stderr");
}

#[test]
fn a_malformed_line_gets_a_parse_error_and_the_connection_survives() {
    let mut agent = Agent::spawn();

    agent.send_raw("not json");
    let error = agent.recv();
    let response = agent.initialize(7);

    assert_eq!(error["error"]["code"], -32700);
    assert_eq!(response["id"], 7);
    assert_eq!(response["result"]["info"]["name"], "bee");
    agent.finish();
}

#[test]
fn a_request_with_invalid_params_gets_an_invalid_params_error() {
    let mut agent = Agent::spawn();

    agent.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {},
    }));
    let error = agent.recv();

    assert_eq!(error["id"], 1);
    assert_eq!(error["error"]["code"], -32602);
    agent.finish();
}

#[test]
fn a_method_before_initialization_is_rejected_as_an_invalid_request() {
    let mut agent = Agent::spawn();

    agent.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {}}));
    let error = agent.recv();

    assert_eq!(error["id"], 2);
    assert_eq!(error["error"]["code"], -32600);
    agent.finish();
}

#[test]
fn an_unknown_method_after_initialization_gets_method_not_found() {
    let mut agent = Agent::spawn();
    agent.initialize(1);

    agent.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "no/such/method", "params": {}}));
    let error = agent.recv();

    assert_eq!(error["id"], 2);
    assert_eq!(error["error"]["code"], -32601);
    agent.finish();
}

#[test]
fn closing_stdin_ends_the_process_with_success() {
    let mut agent = Agent::spawn();
    agent.initialize(1);

    let (status, stderr) = agent.finish();

    assert!(status.success(), "exit status was {status}");
    assert_eq!(stderr, "");
}

#[test]
fn a_client_that_never_speaks_can_disconnect_cleanly() {
    let (status, stderr) = Agent::spawn().finish();

    assert!(status.success(), "exit status was {status}");
    assert_eq!(stderr, "");
}

/// Open a session over `agent`, which must be initialized, and return its id.
fn new_session(agent: &mut Agent, id: u64) -> Value {
    agent.send(&json!({
        "jsonrpc": "2.0", "id": id, "method": "session/new",
        "params": { "cwd": std::env::temp_dir() },
    }));
    agent.recv()["result"]["sessionId"].clone()
}

#[test]
fn a_malformed_prompt_gets_invalid_params_and_the_next_prompt_still_runs() {
    let mut agent = Agent::spawn();
    agent.initialize(1);
    let session = new_session(&mut agent, 2);

    // `prompt` must be an array of content blocks.
    agent.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": { "sessionId": session, "prompt": "hi" },
    }));
    let error = agent.recv();
    agent.send(&json!({
        "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
        "params": { "sessionId": session, "prompt": [{ "type": "text", "text": "hi" }] },
    }));

    assert_eq!(error["id"], 3);
    assert_eq!(error["error"]["code"], -32602);
    // Recovery: the connection and the session are both still usable.
    let mut accepted = false;
    loop {
        let message = agent.recv();
        accepted |= message["id"] == 4 && message["result"]["messageId"].is_string();
        if message["params"]["update"]["state"] == "idle" {
            break;
        }
    }
    assert!(accepted, "the second prompt was accepted");
    let (status, stderr) = agent.finish();
    assert!(status.success(), "exit status was {status}");
    assert_eq!(stderr, "", "an error response writes nothing to stderr");
}

#[test]
fn a_prompt_with_unsupported_content_gets_invalid_params_over_the_wire() {
    let mut agent = Agent::spawn();
    agent.initialize(1);
    let session = new_session(&mut agent, 2);

    agent.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": {
            "sessionId": session,
            "prompt": [{ "type": "image", "data": "aGk=", "mimeType": "image/png" }],
        },
    }));
    let error = agent.recv();

    assert_eq!(error["id"], 3);
    assert_eq!(error["error"]["code"], -32602);
    let (status, stderr) = agent.finish();
    assert!(status.success(), "exit status was {status}");
    assert_eq!(stderr, "");
}

#[test]
fn a_malformed_cancel_notification_gets_no_reply_and_the_connection_survives() {
    let mut agent = Agent::spawn();
    agent.initialize(1);

    // A notification has no id, so there is nobody to answer. It must not
    // end the connection either.
    agent.send(&json!({
        "jsonrpc": "2.0", "method": "session/cancel",
        "params": { "sessionId": 42 },
    }));
    let created = new_session(&mut agent, 2);

    // The next message is the answer to `session/new`, not an error for the
    // notification.
    assert!(created.is_string());
    let (status, _stderr) = agent.finish();
    assert!(status.success(), "exit status was {status}");
}
