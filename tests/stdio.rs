//! The stdio transport, exercised through the real binary.
//!
//! These spawn `bee` the way an ACPv2 client would, because the properties
//! under test (a clean stdout, exit codes, shutdown on EOF) belong to the
//! process, not to the library.

#[path = "support/scenarios.rs"]
mod scenarios;
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
    _isolated: std::sync::Arc<tempfile::TempDir>,
}

impl Agent {
    fn spawn() -> Self {
        Self::configured("")
    }

    fn configured(config: &str) -> Self {
        Self::configured_with_state(config, None)
    }

    fn configured_with_state(config: &str, state: Option<&std::path::Path>) -> Self {
        Self::launch(config, state, None)
    }

    fn scenario(name: &str) -> Self {
        Self::launch("", None, Some(name))
    }

    fn scenario_in(name: &str, isolated: std::sync::Arc<tempfile::TempDir>) -> Self {
        Self::launch_in("", None, Some(name), isolated)
    }

    fn launch(config: &str, state: Option<&std::path::Path>, scenario: Option<&str>) -> Self {
        Self::launch_in(
            config,
            state,
            scenario,
            std::sync::Arc::new(tempfile::tempdir().expect("a temp directory")),
        )
    }

    fn launch_in(
        config: &str,
        state: Option<&std::path::Path>,
        scenario: Option<&str>,
        isolated: std::sync::Arc<tempfile::TempDir>,
    ) -> Self {
        // An empty directory as home, config home and working directory, so a
        // developer's real configuration (or a malformed one) cannot change
        // what these tests observe. Keep it alive until the child is reaped.
        // Production refresh is automatic, but transport tests must stay offline.
        let global = isolated.path().join("xdg/bee/config.toml");
        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        std::fs::write(
            global,
            if scenario.is_some() {
                "invalid toml ["
            } else {
                "[catalog]\nenabled=false\n"
            },
        )
        .unwrap();
        let path = bee::config::project_config_path(isolated.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            if scenario.is_some() {
                "invalid toml [".into()
            } else {
                support::native_bash_config(config)
            },
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bee"));
        command
            .env("HOME", isolated.path())
            .env("XDG_CONFIG_HOME", isolated.path().join("xdg"))
            .env("XDG_CACHE_HOME", isolated.path().join("cache"))
            .env("XDG_DATA_HOME", isolated.path().join("data"))
            .env(
                "XDG_STATE_HOME",
                state.map_or_else(|| isolated.path().join("state"), std::path::Path::to_owned),
            )
            .env_remove("BEE_CONFIG")
            .env_remove("BEE_PROFILE")
            .current_dir(isolated.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(scenario) = scenario {
            // Poison ambient selectors too: even explicit missing configuration
            // and profiles must be irrelevant to the deterministic launch path.
            command
                .args(["--scenario", scenario])
                .env("BEE_CONFIG", isolated.path().join("missing.toml"))
                .env("BEE_PROFILE", "missing-profile");
        }
        let mut child = command.spawn().expect("the bee binary starts");

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

    fn until_idle(&self) -> Vec<Value> {
        let mut messages = Vec::new();
        loop {
            let message = self.recv();
            let done = message["params"]["update"]["state"] == "idle";
            messages.push(message);
            if done {
                return messages;
            }
        }
    }

    fn until_idle_approving_tools(&mut self) -> Vec<Value> {
        let mut messages = Vec::new();
        loop {
            let message = self.recv();
            if message["method"] == "session/request_permission" {
                assert_eq!(message["params"]["subject"]["type"], "tool_call");
                assert_eq!(
                    message["params"]["subject"]["toolCall"]["status"],
                    "pending"
                );
                assert_eq!(message["params"]["options"].as_array().unwrap().len(), 4);
                self.send(&json!({"jsonrpc":"2.0","id":message["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_always"}}}));
            }
            let done = message["params"]["update"]["state"] == "idle";
            messages.push(message);
            if done {
                return messages;
            }
        }
    }

    fn resume(&mut self, session: &Value, cwd: &std::path::Path, replay: bool) -> Vec<Value> {
        let mut params = json!({"sessionId":session,"cwd":cwd});
        if replay {
            params["replayFrom"] = json!({"type":"start"});
        }
        self.send(&json!({"jsonrpc":"2.0","id":90,"method":"session/resume","params":params}));
        let mut messages: Vec<Value> = Vec::new();
        loop {
            let message = self.recv();
            if message["id"] == 90 {
                assert!(message.get("result").is_some(), "{message}");
                assert_eq!(
                    messages.last().unwrap()["params"]["update"]["state"],
                    "idle"
                );
                return messages;
            }
            messages.push(message);
        }
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

#[test]
fn standard_plan_messages_survive_process_restart_without_reexecuting_model_work() {
    for interrupted in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let initial = json!([{"content":"Inspect","priority":"high","status":"in_progress"}]);
        let revised = json!([
            {"content":"Verify","priority":"medium","status":"in_progress"},
            {"content":"Inspected","priority":"high","status":"completed"}
        ]);
        let call = |id: &str, entries: Value| {
            support::Response::json(
                json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":id,"type":"function","function":{"name":"update_plan","arguments":json!({"entries":entries}).to_string()}}]},"finish_reason":"tool_calls"}]}),
            )
        };
        let (blocked, release) = support::Response::json(support::chat_text("done")).gated();
        let terminal = if interrupted {
            blocked
        } else {
            support::Response::json(support::chat_text("done"))
        };
        let server = support::Server::start(vec![
            call("plan-a", initial.clone()),
            call("plan-b", revised.clone()),
            terminal,
        ]);
        let config = format!(
            "model='test/test'\n[accounts.test]\nprovider='custom'\nmodels=['test']\nprotocols={{openai-chat={{base_url='{}'}}}}",
            server.url
        );
        let mut first = Agent::configured_with_state(&config, Some(&state));
        first.initialize(1);
        first.send(
            &json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}),
        );
        let session = first.recv()["result"]["sessionId"].clone();
        first.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"work"}]}}));
        let mut plans = Vec::new();
        loop {
            let message = first.recv();
            if message["params"]["update"]["sessionUpdate"] == "plan_update" {
                assert_eq!(message["method"], "session/update");
                assert_eq!(message["params"]["sessionId"], session);
                plans.push(message["params"]["update"].clone());
            }
            if (interrupted && plans.len() == 2) || message["params"]["update"]["state"] == "idle" {
                break;
            }
        }
        assert_eq!(
            plans[0],
            json!({"sessionUpdate":"plan_update","plan":{"type":"items","planId":"plan-1","entries":initial}})
        );
        assert_eq!(
            plans[1],
            json!({"sessionUpdate":"plan_update","plan":{"type":"items","planId":"plan-1","entries":revised}})
        );
        server.request();
        server.request();
        server.request();
        if interrupted {
            // Kill without shutdown so recovery must reconcile the durable active plan.
            drop(first);
            let _ = release.send(());
        } else {
            assert!(first.finish().0.success());
        }
        let expected = json!({"sessionUpdate":"plan_update","plan":{"type":"items","planId":"plan-1","entries":[
            {"content":"Verify","priority":"medium","status":"cancelled"},
            {"content":"Inspected","priority":"high","status":"completed"}
        ]}});
        if !interrupted {
            assert_eq!(
                plans,
                vec![plans[0].clone(), plans[1].clone(), expected.clone()]
            );
        }
        let mut second = Agent::configured_with_state(&config, Some(&state));
        second.initialize(1);
        let replay = second.resume(&session, root.path(), true);
        let retained: Vec<_> = replay
            .iter()
            .filter(|message| message["params"]["update"]["sessionUpdate"] == "plan_update")
            .collect();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0]["params"]["update"], expected);
        // A conventional client can deserialize the raw update without metadata
        // conventions or assistant-text/Hive-specific parsing.
        let _: agent_client_protocol::schema::v2::UpdateSessionNotification =
            serde_json::from_value(retained[0]["params"].clone()).unwrap();
        server.assert_no_more_requests();
        assert!(second.finish().0.success());
    }
}

#[test]
fn a_new_process_restores_native_identity_transcript_selection_and_tool_context_without_reexecution()
 {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let server = support::Server::start(vec![
        support::Response::json(
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"saved-write","type":"function","function":{"name":"write","arguments":"{\"path\":\"sentinel\",\"content\":\"original\"}"}}]},"finish_reason":"tool_calls"}]}),
        ),
        support::Response::json(support::chat_text("durable answer")),
        support::Response::json(support::chat_text("continued")),
    ]);
    let config = format!(
        "model='test/test'\n[accounts.test]\nprovider='custom'\nmodels=['test','custom-model']\nprotocols={{openai-chat={{base_url='{}'}}}}",
        server.url
    );
    let mut first = Agent::configured_with_state(&config, Some(&state));
    first.initialize(1);
    first
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = first.recv()["result"]["sessionId"].clone();
    first.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/set_config_option","params":{"sessionId":session,"configId":"model","type":"id","value":"test/custom-model"}}));
    let selected = first.recv();
    assert!(selected.get("result").is_some(), "{selected}");
    let prompt = json!([{"type":"text","text":"remember this"},{"type":"resource_link","uri":"file:///reference","name":"reference"}]);
    first.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":session,"prompt":prompt}}));
    let first_turn = first.until_idle_approving_tools();
    assert_eq!(first_turn[0]["result"]["messageId"], "user-1");
    assert_eq!(
        first_turn.last().unwrap()["params"]["update"]["stopReason"],
        "end_turn"
    );
    server.request();
    server.request();
    assert_eq!(
        std::fs::read_to_string(root.path().join("sentinel")).unwrap(),
        "original"
    );
    let (status, stderr) = first.finish();
    assert!(status.success(), "{stderr}");
    // If replay reexecutes the historical write it will overwrite this value.
    std::fs::write(root.path().join("sentinel"), "changed after execution").unwrap();

    let mut second = Agent::configured_with_state(&config, Some(&state));
    second.initialize(1);
    second.send(
        &json!({"jsonrpc":"2.0","id":2,"method":"session/list","params":{"cwd":root.path()}}),
    );
    assert_eq!(second.recv()["result"]["sessions"][0]["sessionId"], session);
    let replay = second.resume(&session, root.path(), true);
    assert_eq!(replay.len(), 4);
    assert_eq!(replay[0]["params"]["update"]["messageId"], "user-1");
    assert_eq!(replay[0]["params"]["update"]["content"], prompt);
    assert_eq!(replay[1]["params"]["update"]["messageId"], "agent-1");
    assert_eq!(
        replay[2]["params"]["update"]["sessionUpdate"],
        "tool_call_update"
    );
    assert_eq!(replay[2]["params"]["update"]["toolCallId"], "tool-1-1");
    assert_eq!(replay[2]["params"]["update"]["content"][0]["type"], "diff");
    assert_eq!(
        replay[1]["params"]["update"]["content"][0]["text"],
        "durable answer"
    );
    assert_eq!(second.resume(&session, root.path(), true), replay);
    assert_eq!(second.resume(&session, root.path(), false).len(), 1);
    assert_eq!(
        std::fs::read_to_string(root.path().join("sentinel")).unwrap(),
        "changed after execution"
    );
    server.assert_no_more_requests();

    second.send(&json!({"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"continue"}]}}));
    let continued = second.until_idle();
    assert_eq!(continued[0]["result"]["messageId"], "user-2");
    let request = server.request();
    assert_eq!(request.body["model"], "custom-model");
    let history = request.body["messages"].to_string();
    assert!(history.contains("durable answer"));
    assert!(history.contains("saved-write"));
    assert!(history.contains("remember this"));
    assert!(second.finish().0.success());
}

#[test]
fn opaque_prompts_replay_after_restart_without_entering_current_or_later_model_input() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let server = support::Server::start(vec![
        support::Response::json(support::chat_text("mixed answer")),
        support::Response::json(support::chat_text("opaque answer")),
        support::Response::json(support::chat_text("continued answer")),
    ]);
    let config = format!(
        "model='test/test'\n[accounts.test]\nprovider='custom'\nmodels=['test']\nprotocols={{openai-chat={{base_url='{}'}}}}",
        server.url
    );
    let opaque = json!({"type":"_private_card","text":"opaque-text-sentinel","payload":[null,{"command":"opaque-command-sentinel"}],"_meta":{"vendor/key":"opaque-meta-sentinel"}});
    let future = json!({"type":"future_card_sentinel","payload":{"uri":"opaque-uri-sentinel"}});
    let mixed = json!([
        {"type":"text","text":"understood text","extra":"ignored-text-sentinel"},
        opaque,
        {"type":"resource_link","uri":"file:///reference","name":"reference","extra":"ignored-link-sentinel"},
        future
    ]);
    let normalized = json!([
        {"type":"text","text":"understood text"}, opaque,
        {"type":"resource_link","uri":"file:///reference","name":"reference"}, future
    ]);
    let unknown_only = json!([future, opaque]);
    let marker = "[Prompt contains content Bee does not understand; its contents were not sent to the model.]";
    let projection = "understood text\n[Resource link, contents not fetched: file:///reference]";
    let mut first = Agent::configured_with_state(&config, Some(&state));
    first.initialize(1);
    first
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = first.recv()["result"]["sessionId"].clone();
    for (id, prompt, expected_echo, expected_input) in [
        (3, mixed, normalized.clone(), projection),
        (4, unknown_only.clone(), unknown_only.clone(), marker),
    ] {
        first.send(&json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{"sessionId":session,"prompt":prompt,"extra":"ignored-request-sentinel"}}));
        let messages = first.until_idle();
        let response = messages.iter().find(|message| message["id"] == id).unwrap();
        let user = messages
            .iter()
            .find(|message| message["params"]["update"]["sessionUpdate"] == "user_message")
            .unwrap();
        assert_eq!(
            user["params"]["update"]["messageId"],
            response["result"]["messageId"]
        );
        assert_eq!(user["params"]["update"]["content"], expected_echo);
        assert_eq!(
            messages.last().unwrap()["params"]["update"]["stopReason"],
            "end_turn"
        );
        let request = server.request();
        let users: Vec<_> = request.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .collect();
        assert_eq!(users.last().unwrap()["content"], expected_input);
        assert_eq!(users[0]["content"], projection);
        assert!(!request.body.to_string().contains("sentinel"));
        assert!(!request.body.to_string().contains("_private_card"));
    }
    assert!(first.finish().0.success());

    let mut second = Agent::configured_with_state(&config, Some(&state));
    second.initialize(1);
    let replay = second.resume(&session, root.path(), true);
    let users: Vec<_> = replay
        .iter()
        .filter(|message| message["params"]["update"]["sessionUpdate"] == "user_message")
        .collect();
    assert_eq!(users.len(), 2);
    assert_eq!(users[0]["params"]["update"]["content"], normalized);
    assert_eq!(users[0]["params"]["update"]["messageId"], "user-1");
    assert_eq!(users[1]["params"]["update"]["content"], unknown_only);
    assert_eq!(users[1]["params"]["update"]["messageId"], "user-2");
    assert_eq!(second.resume(&session, root.path(), true), replay);
    server.assert_no_more_requests();

    second.send(&json!({"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"continue"}]}}));
    second.until_idle();
    let request = server.request();
    let users: Vec<_> = request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .collect();
    assert_eq!(users.len(), 3);
    assert_eq!(users[0]["content"], projection);
    assert_eq!(users[1]["content"], marker);
    assert_eq!(users[2]["content"], "continue");
    let history = request.body.to_string();
    assert!(history.contains("mixed answer"));
    assert!(history.contains("opaque answer"));
    assert!(!history.contains("sentinel"));
    assert!(!history.contains("_private_card"));
    server.assert_no_more_requests();
    assert!(second.finish().0.success());
}

#[test]
fn profile_ownership_isolates_history_and_models_across_process_restarts() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let server = support::Server::start(vec![
        support::Response::json(support::chat_text("work answer")),
        support::Response::json(support::chat_text("personal answer")),
        support::Response::json(support::chat_text("work continued")),
    ]);
    let shared = format!(
        "[accounts.shared]\nprovider='custom'\nprotocols={{openai-chat={{base_url='{}'}}}}\nmodels=['base','work-model','personal-model','saved-work-model']\n[profiles.work]\naccounts=['shared']\nmodel='shared/work-model'\n[profiles.personal]\naccounts=['shared']\nmodel='shared/personal-model'\n",
        server.url
    );
    let work = format!("profile='work'\n{shared}");
    let personal = format!("profile='personal'\n{shared}");
    let mut first = Agent::configured_with_state(&work, Some(&state));
    first.initialize(1);
    first
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = first.recv()["result"]["sessionId"].clone();
    first.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/set_config_option","params":{"sessionId":session,"configId":"model","type":"id","value":"shared/saved-work-model"}}));
    assert!(first.recv().get("result").is_some());
    first.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"work context"}]}}));
    first.until_idle();
    assert_eq!(server.request().body["model"], "saved-work-model");
    assert!(first.finish().0.success());
    let journal = state
        .join("bee/sessions")
        .join(session.as_str().unwrap())
        .join("session.jsonl");
    // A torn tail proves that rejected attachment cannot perform recovery.
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(b"{\"partial\":")
        .unwrap();
    let before = std::fs::read(&journal).unwrap();

    let mut other = Agent::configured_with_state(&personal, Some(&state));
    other.initialize(1);
    other.send(&json!({"jsonrpc":"2.0","id":2,"method":"session/list","params":{}}));
    assert_eq!(other.recv()["result"]["sessions"], json!([]));
    for replay in [false, true] {
        let mut params = json!({"sessionId":session,"cwd":root.path()});
        if replay {
            params["replayFrom"] = json!({"type":"start"});
        }
        other.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/resume","params":params}));
        let rejected = other.recv();
        assert_eq!(rejected["id"], 3, "no replay or notices precede rejection");
        assert!(
            rejected["error"]["data"]
                .as_str()
                .unwrap()
                .contains("different profile")
        );
        assert_eq!(std::fs::read(&journal).unwrap(), before);
    }
    other
        .send(&json!({"jsonrpc":"2.0","id":4,"method":"session/new","params":{"cwd":root.path()}}));
    let personal_session = other.recv()["result"]["sessionId"].clone();
    other.send(&json!({"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{"sessionId":personal_session,"prompt":[{"type":"text","text":"personal context"}]}}));
    other.until_idle();
    let request = server.request();
    assert_eq!(request.body["model"], "personal-model");
    assert!(
        !request.body["messages"]
            .to_string()
            .contains("work context")
    );

    // The rejected resume released its lock even while the other runtime lives.
    let mut resumed = Agent::configured_with_state(&work, Some(&state));
    resumed.initialize(1);
    resumed.send(&json!({"jsonrpc":"2.0","id":2,"method":"session/list","params":{}}));
    let listed = resumed.recv();
    assert_eq!(listed["result"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["result"]["sessions"][0]["sessionId"], session);
    resumed.resume(&session, root.path(), true);
    resumed.send(&json!({"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"continue work"}]}}));
    resumed.until_idle();
    assert_eq!(server.request().body["model"], "saved-work-model");
    assert!(resumed.finish().0.success());
    assert!(other.finish().0.success());
}

#[test]
fn a_removed_selected_connection_cannot_fall_back_on_resume() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let connections = "[accounts.one]\nprovider='openai'\ndiscovery=false\nmodels=['chosen']\n[accounts.two]\nprovider='openai'\ndiscovery=false\nmodels=['chosen']\n";
    let config = format!(
        "profile='work'\n[profiles.work]\naccounts=['one','two']\nmodel='one/chosen'\n{connections}"
    );
    let mut first = Agent::configured_with_state(&config, Some(&state));
    first.initialize(1);
    first
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = first.recv()["result"]["sessionId"].clone();
    first.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/set_config_option","params":{"sessionId":session,"configId":"model","type":"id","value":"two/chosen"}}));
    assert!(first.recv().get("result").is_some());
    assert!(first.finish().0.success());
    let narrowed = format!("profile='work'\n[profiles.work]\naccounts=['one']\n{connections}");
    let mut second = Agent::configured_with_state(&narrowed, Some(&state));
    second.initialize(1);
    second.send(&json!({"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":session,"cwd":root.path(),"replayFrom":{"type":"start"}}}));
    let rejected = second.recv();
    assert!(
        rejected["error"]["data"]
            .as_str()
            .unwrap()
            .contains("outside the active profile")
    );
    assert!(second.finish().0.success());
}

#[test]
fn qualified_choices_preserve_account_identity_and_raw_wire_suffixes_across_restart() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let first_endpoint = support::Server::start(Vec::new());
    let second_endpoint = support::Server::start(vec![
        support::Response::json(support::chat_text("first answer")),
        support::Response::json(support::chat_text("resumed answer")),
    ]);
    let config = |models: &str| {
        format!(
            "[accounts.first]\nprovider='custom'\nmodels=['vendor/model']\nprotocols.openai-chat.base_url='{}'\n[accounts.second]\nprovider='custom'\nmodels={models}\nprotocols.openai-chat.base_url='{}'",
            first_endpoint.url, second_endpoint.url
        )
    };
    let mut agent = Agent::configured_with_state(&config("['vendor/model']"), Some(&state));
    agent.initialize(1);
    agent
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let created = agent.recv();
    let session = created["result"]["sessionId"].clone();
    let wire = created.to_string();
    assert!(wire.contains("first/vendor/model") && wire.contains("second/vendor/model"));
    assert!(wire.contains("Choose a model"));
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/set_config_option","params":{"sessionId":session,"configId":"model","type":"id","value":"second/vendor/model"}}));
    assert!(agent.recv().get("result").is_some());
    agent.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"hello"}]}}));
    agent.until_idle();
    assert_eq!(second_endpoint.request().body["model"], "vendor/model");
    assert!(agent.finish().0.success());

    let mut resumed = Agent::configured_with_state(&config("['vendor/model']"), Some(&state));
    resumed.initialize(1);
    resumed.resume(&session, root.path(), true);
    resumed.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"continue"}]}}));
    resumed.until_idle();
    assert_eq!(second_endpoint.request().body["model"], "vendor/model");
    assert!(resumed.finish().0.success());

    let mut excluded = Agent::configured_with_state(&config("[]"), Some(&state));
    excluded.initialize(1);
    excluded.resume(&session, root.path(), true);
    excluded.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"cannot infer"}]}}));
    let stopped = excluded.until_idle();
    assert_eq!(
        stopped.last().unwrap()["params"]["update"]["stopReason"],
        "error"
    );
    assert!(excluded.finish().0.success());
}

#[test]
fn competing_processes_can_list_but_cannot_attach_until_the_owner_closes() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let mut first = Agent::configured_with_state("", Some(&state));
    first.initialize(1);
    first
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = first.recv()["result"]["sessionId"].clone();
    let mut second = Agent::configured_with_state("", Some(&state));
    second.initialize(1);
    second.send(&json!({"jsonrpc":"2.0","id":2,"method":"session/list","params":{}}));
    assert_eq!(second.recv()["result"]["sessions"][0]["sessionId"], session);
    second.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/resume","params":{"sessionId":session,"cwd":root.path()}}));
    let rejected = second.recv();
    assert!(
        rejected["error"]["data"]
            .as_str()
            .unwrap()
            .contains("owned by another")
    );
    // A conflict for one ID cannot stall a different session.
    second
        .send(&json!({"jsonrpc":"2.0","id":4,"method":"session/new","params":{"cwd":root.path()}}));
    assert_ne!(second.recv()["result"]["sessionId"], session);
    first.send(
        &json!({"jsonrpc":"2.0","id":5,"method":"session/close","params":{"sessionId":session}}),
    );
    assert!(first.recv().get("result").is_some());
    first.send(
        &json!({"jsonrpc":"2.0","id":6,"method":"session/close","params":{"sessionId":session}}),
    );
    assert!(
        first.recv().get("result").is_some(),
        "closing a detached durable session is idempotent"
    );
    assert_eq!(second.resume(&session, root.path(), true).len(), 1);
    assert!(first.finish().0.success());
    assert!(second.finish().0.success());
}

#[test]
fn storage_failure_after_acceptance_emits_one_error_idle_and_preserves_committed_context() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (blocked, release) =
        support::Response::json(support::chat_text("uncommitted secret answer")).gated();
    let server = support::Server::start(vec![
        support::Response::json(support::chat_text("committed answer")),
        blocked,
        support::Response::json(support::chat_text("recovered")),
    ]);
    let config = format!(
        "model='test/test'\n[accounts.test]\nprovider='custom'\nmodels=['test']\nprotocols={{openai-chat={{base_url='{}'}}}}",
        server.url
    );
    let mut agent = Agent::configured_with_state(&config, Some(&state));
    agent.initialize(1);
    agent
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"committed user"}]}}));
    agent.until_idle();
    server.request();
    let path = state
        .join("bee/sessions")
        .join(session.as_str().unwrap())
        .join("session.jsonl");
    agent.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"failed user"}]}}));
    server.request();
    // Remove the owned path only after durable acceptance, before the gated
    // provider can emit text. The production writer must refuse to acknowledge
    // output into an unlinked/replaced journal, even though its FD is still open.
    let saved = root.path().join("private-storage-error.jsonl");
    std::fs::rename(&path, &saved).unwrap();
    std::fs::create_dir(&path).unwrap();
    release.send(()).unwrap();
    let failed = agent.until_idle();
    assert_eq!(failed[0]["result"]["messageId"], "user-2");
    assert_eq!(
        failed.last().unwrap()["params"]["update"]["stopReason"],
        "error"
    );
    assert!(
        !serde_json::to_string(&failed)
            .unwrap()
            .contains("private-storage-error")
    );
    assert!(
        !serde_json::to_string(&failed)
            .unwrap()
            .contains("uncommitted secret")
    );
    agent.send(&json!({"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"still poisoned"}]}}));
    assert!(agent.recv().get("error").is_some());
    agent.send(
        &json!({"jsonrpc":"2.0","id":6,"method":"session/close","params":{"sessionId":session}}),
    );
    assert_eq!(
        agent.recv()["id"],
        6,
        "no duplicate terminal idle may follow the failure"
    );
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(saved, &path).unwrap();
    let replay = agent.resume(&session, root.path(), true);
    assert!(
        serde_json::to_string(&replay)
            .unwrap()
            .contains("failed user")
    );
    agent.send(&json!({"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"continue"}]}}));
    let resumed = agent.until_idle();
    assert_eq!(resumed[0]["result"]["messageId"], "user-3");
    let context = server.request().body["messages"].to_string();
    assert!(context.contains("committed answer"));
    assert!(!context.contains("failed user"));
    assert!(!context.contains("uncommitted secret"));
    assert!(agent.finish().0.success());
}

#[test]
fn repeated_replay_is_bounded_without_deleting_history_or_preventing_state_only_resume() {
    let server = support::Server::start(vec![support::Response::json(support::chat_text(
        "retained answer",
    ))]);
    let config = format!(
        "model='test/test'\n[history]\nreplay_max_bytes=2048\n[accounts.test]\nprovider='custom'\nmodels=['test']\nprotocols={{openai-chat={{base_url='{}'}}}}",
        server.url
    );
    let mut agent = Agent::configured(&config);
    agent.initialize(1);
    let cwd = agent._isolated.path().to_owned();
    agent.send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"retained user"}]}}));
    agent.until_idle();
    server.request();
    assert_eq!(agent.resume(&session, &cwd, true).len(), 3);
    agent.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/resume","params":{"sessionId":session,"cwd":cwd,"replayFrom":{"type":"start"}}}));
    let rejected = agent.recv();
    assert_eq!(
        rejected["id"], 4,
        "budget refusal must precede any partial history"
    );
    assert!(
        rejected["error"]["data"]
            .as_str()
            .unwrap()
            .contains("byte budget")
    );
    assert_eq!(agent.resume(&session, &cwd, false).len(), 1);
    agent.send(&json!({"jsonrpc":"2.0","id":5,"method":"session/list","params":{}}));
    assert_eq!(agent.recv()["result"]["sessions"][0]["sessionId"], session);
    server.assert_no_more_requests();
    assert!(agent.finish().0.success());
}

#[test]
fn a_resume_response_releases_its_request_gate_before_the_next_request() {
    let mut agent = Agent::spawn();
    agent.initialize(1);
    let cwd = agent._isolated.path().to_owned();
    agent.send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    // Each response is a readiness barrier, even when the worker publishes it
    // before the async resume continuation gets another turn on the reactor.
    for index in 0..200 {
        assert_eq!(agent.resume(&session, &cwd, index % 2 == 0).len(), 1);
    }
    assert!(agent.finish().0.success());
}

#[rstest::rstest]
#[case::cancelled("cancel")]
#[case::interrupted("kill")]
#[case::failed("failure")]
#[case::refused("refusal")]
fn unsuccessful_partial_answers_survive_restart_without_entering_inference_context(
    #[case] stop: &str,
) {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let first = format!(
        "data: {}\n\n",
        json!({"choices":[{"index":0,"delta":{"content":"visible partial"},"finish_reason":null}]})
    );
    let end = if stop == "refusal" {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]})
        )
    } else {
        "data: malformed\n\n".into()
    };
    let (partial, release) =
        support::Response::sse(format!("{first}{end}")).gated_after(first.len());
    let server = support::Server::start(vec![
        support::Response::json(support::chat_text("successful context")),
        partial,
        support::Response::json(support::chat_text("recovered")),
    ]);
    let config = format!(
        "model='test/test'\n[accounts.test]\nprovider='custom'\nmodels=['test']\nprotocols={{openai-chat={{base_url='{}'}}}}",
        server.url
    );
    let mut agent = Agent::configured_with_state(&config, Some(&state));
    agent.initialize(1);
    agent
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"keep this"}]}}));
    agent.until_idle();
    server.request();
    agent.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"unsuccessful user"}]}}));
    loop {
        let message = agent.recv();
        if message["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
            break;
        }
    }
    server.request();
    // Replay during live streaming is serialized with durable output and leaves
    // the active turn running; it must never restart inference.
    agent.send(&json!({"jsonrpc":"2.0","id":6,"method":"session/resume","params":{"sessionId":session,"cwd":root.path(),"replayFrom":{"type":"start"}}}));
    let mut live_replay = Vec::new();
    loop {
        let message = agent.recv();
        if message["id"] == 6 {
            break;
        }
        live_replay.push(message);
    }
    assert_eq!(
        live_replay[3]["params"]["update"]["content"][0]["text"],
        "visible partial"
    );
    assert_eq!(
        live_replay.last().unwrap()["params"]["update"]["state"],
        "running"
    );
    match stop {
        "kill" => {
            agent.child.kill().unwrap();
            agent.child.wait().unwrap();
        }
        "cancel" => {
            agent.send(
                &json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}),
            );
            assert_eq!(
                agent.until_idle().last().unwrap()["params"]["update"]["stopReason"],
                "cancelled"
            );
        }
        _ => {
            release.send(()).unwrap();
            assert_eq!(
                agent.until_idle().last().unwrap()["params"]["update"]["stopReason"],
                if stop == "refusal" {
                    "refusal"
                } else {
                    "error"
                }
            );
        }
    }
    if !matches!(stop, "failure" | "refusal") {
        let _ = release.send(());
    }
    if stop == "kill" {
        drop(agent);
    } else {
        assert!(agent.finish().0.success());
    }

    let mut restored = Agent::configured_with_state(&config, Some(&state));
    restored.initialize(1);
    let replay = restored.resume(&session, root.path(), true);
    let messages: Vec<_> = replay
        .iter()
        .filter(|message| message["params"]["update"]["sessionUpdate"] != "notice")
        .collect();
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[2]["params"]["update"]["messageId"], "user-2");
    assert_eq!(
        messages[3]["params"]["update"]["content"][0]["text"],
        "visible partial"
    );
    restored.send(&json!({"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"next"}]}}));
    let next = restored.until_idle();
    assert_eq!(next[0]["result"]["messageId"], "user-3");
    let context = server.request().body["messages"].to_string();
    assert!(context.contains("successful context"));
    assert!(!context.contains("unsuccessful user"));
    assert!(!context.contains("visible partial"));
    assert!(restored.finish().0.success());
}

fn thought_exchange(protocol: &str, refusal: bool) -> (String, String, Value) {
    use support::event;
    if protocol == "openai-responses" {
        let reasoning = json!({"id":"private-provider-id","type":"reasoning","summary":[{"type":"summary_text","text":"visible thought"}],"encrypted_content":"private-encrypted-sentinel"});
        let answer = json!({"id":"answer","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]});
        let prefix = event(
            "response.output_item.added",
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"private-provider-id","type":"reasoning"}}),
        ) + &event(
            "response.reasoning_summary_part.added",
            json!({"type":"response.reasoning_summary_part.added","output_index":0,"item_id":"private-provider-id","summary_index":0,"part":{"type":"summary_text","text":""}}),
        ) + &event(
            "response.reasoning_text.delta",
            json!({"type":"response.reasoning_text.delta","output_index":0,"item_id":"private-provider-id","delta":"private-raw-sentinel"}),
        ) + &event(
            "response.reasoning_summary_text.delta",
            json!({"type":"response.reasoning_summary_text.delta","output_index":0,"item_id":"private-provider-id","summary_index":0,"delta":"visible thought"}),
        );
        let mut complete =
            json!({"status":"completed","output":[reasoning.clone(),answer.clone()]});
        if refusal {
            complete["status"] = "incomplete".into();
            complete["incomplete_details"] = json!({"reason":"content_filter"});
        }
        let kind = if refusal {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let suffix = event(
            "response.output_item.done",
            json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
        ) + &event(
            "response.output_item.added",
            json!({"type":"response.output_item.added","output_index":1,"item":{"id":"answer","type":"message"}}),
        ) + &event(
            "response.output_item.done",
            json!({"type":"response.output_item.done","output_index":1,"item":answer}),
        ) + &event(kind, json!({"type":kind,"response":complete}));
        (prefix, suffix, json!({"status":"completed","output":[]}))
    } else {
        let prefix = event(
            "message_start",
            json!({"type":"message_start","message":{"type":"message","role":"assistant","content":[]}}),
        ) + &event(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        ) + &event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"visible thought"}}),
        );
        let suffix = event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"private-signature-sentinel"}}),
        ) + &event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ) + &event(
            "content_block_start",
            json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"private-redacted-sentinel"}}),
        ) + &event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":1}),
        ) + &event(
            "content_block_start",
            json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":"answer"}}),
        ) + &event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":2}),
        ) + &event(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":if refusal { "refusal" } else { "end_turn" }}}),
        ) + &event("message_stop", json!({"type":"message_stop"}));
        (
            prefix,
            suffix,
            json!({"type":"message","role":"assistant","content":[{"type":"text","text":"next"}],"stop_reason":"end_turn"}),
        )
    }
}

#[rstest::rstest]
fn production_tool_steps_and_new_turns_get_distinct_thought_message_ids(
    #[values("openai-responses", "anthropic-messages")] protocol: &str,
) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note.txt"), "tool-result").unwrap();
    let thought = if protocol == "openai-responses" {
        json!({"id":"private-provider-id","type":"reasoning","summary":[{"type":"summary_text","text":"visible"}],"encrypted_content":"private-encrypted-sentinel"})
    } else {
        json!({"type":"thinking","thinking":"visible","signature":"private-signature-sentinel"})
    };
    let (first, second) = if protocol == "openai-responses" {
        (
            json!({"status":"completed","output":[thought.clone(),{"type":"function_call","id":"read-item","call_id":"read-call","name":"read","arguments":"{\"path\":\"note.txt\"}"}]}),
            json!({"status":"completed","output":[thought,{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}),
        )
    } else {
        (
            json!({"type":"message","role":"assistant","content":[thought.clone(),{"type":"tool_use","id":"read-call","name":"read","input":{"path":"note.txt"}}],"stop_reason":"tool_use"}),
            json!({"type":"message","role":"assistant","content":[thought,{"type":"text","text":"done"}],"stop_reason":"end_turn"}),
        )
    };
    let server = support::Server::start(vec![
        support::Response::json(first),
        support::Response::json(second.clone()),
        support::Response::json(second),
    ]);
    let mut agent = Agent::configured(&format!(
        "model='test/test'\n[accounts.test]\nprovider='custom'\nmodels=['test']\n[accounts.test.protocols.{protocol}]\nbase_url='{}'",
        server.url
    ));
    agent.initialize(1);
    agent
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    let mut wire = Vec::new();
    for (id, expected) in [
        (3, vec!["thought-1-0-0-0", "thought-1-1-0-0"]),
        (4, vec!["thought-2-0-0-0"]),
    ] {
        agent.send(&json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"work"}]}}));
        let updates = agent.until_idle_approving_tools();
        assert_eq!(
            updates.last().unwrap()["params"]["update"]["stopReason"],
            "end_turn"
        );
        let ids: Vec<_> = updates
            .iter()
            .filter(|m| m["params"]["update"]["sessionUpdate"] == "agent_thought_chunk")
            .map(|m| m["params"]["update"]["messageId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, expected);
        wire.extend(updates);
    }
    server.request();
    let continuation = server.request().body.to_string();
    assert!(continuation.contains("tool-result"));
    assert!(continuation.contains("private-"));
    server.request();
    assert!(!serde_json::to_string(&wire).unwrap().contains("private-"));
    assert!(agent.finish().0.success());
}

#[rstest::rstest]
fn display_thoughts_use_standard_wire_entities_and_survive_terminal_outcomes_and_restart(
    #[values("openai-responses", "anthropic-messages")] protocol: &str,
    #[values(
        "success",
        "malformed",
        "failure",
        "cancel",
        "close",
        "kill",
        "refusal"
    )]
    stop: &str,
) {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (prefix, mut suffix, next) = thought_exchange(protocol, stop == "refusal");
    if stop == "malformed" {
        suffix = "data: invalid-json\n\n".into();
    }
    if stop == "failure" {
        suffix = if protocol == "openai-responses" {
            support::event(
                "response.failed",
                json!({"type":"response.failed","response":{"error":{"message":"private-error-sentinel"}}}),
            )
        } else {
            support::event(
                "error",
                json!({"type":"error","error":{"type":"overloaded_error","message":"private-error-sentinel"}}),
            )
        };
    }
    let (response, release) =
        support::Response::sse(format!("{prefix}{suffix}")).gated_after(prefix.len());
    let server = support::Server::start(vec![response, support::Response::json(next)]);
    let config = format!(
        "model='test/test'\n[accounts.test]\nprovider='custom'\nmodels=['test']\n[accounts.test.protocols.{protocol}]\nbase_url='{}'",
        server.url
    );
    let mut agent = Agent::configured_with_state(&config, Some(&state));
    agent.initialize(1);
    agent
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"thought question"}]}}));
    let mut wire = Vec::new();
    let chunk = loop {
        let message = agent.recv();
        wire.push(message.clone());
        if message["params"]["update"]["sessionUpdate"] == "agent_thought_chunk" {
            break message;
        }
    };
    assert_eq!(chunk["params"]["sessionId"], session);
    assert_eq!(
        chunk["params"]["update"],
        json!({"sessionUpdate":"agent_thought_chunk","messageId":"thought-1-0-0-0","content":{"type":"text","text":"visible thought"}})
    );
    server.request();
    agent.send(&json!({"jsonrpc":"2.0","id":90,"method":"session/resume","params":{"sessionId":session,"cwd":root.path(),"replayFrom":{"type":"start"}}}));
    let mut live = Vec::new();
    loop {
        let message = agent.recv();
        if message["id"] == 90 {
            break;
        }
        live.push(message);
    }
    let thought = live
        .iter()
        .find(|m| m["params"]["update"]["sessionUpdate"] == "agent_thought")
        .unwrap()["params"]["update"]
        .clone();
    assert_eq!(
        thought,
        json!({"sessionUpdate":"agent_thought","messageId":"thought-1-0-0-0","content":[{"type":"text","text":"visible thought"}]})
    );
    assert_eq!(live.last().unwrap()["params"]["update"]["state"], "running");
    wire.extend(live);
    match stop {
        "kill" => {
            agent.child.kill().unwrap();
            agent.child.wait().unwrap();
        }
        "cancel" | "close" => {
            if stop == "close" {
                agent.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/close","params":{"sessionId":session}}));
            } else {
                agent.send(&json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}));
            }
            let ended = agent.until_idle();
            assert_eq!(
                ended.last().unwrap()["params"]["update"]["stopReason"],
                "cancelled"
            );
            wire.extend(ended);
            if stop == "close" {
                assert_eq!(agent.recv()["id"], 4);
            }
        }
        _ => {
            release.send(()).unwrap();
            let ended = agent.until_idle();
            assert_eq!(
                ended.last().unwrap()["params"]["update"]["stopReason"],
                match stop {
                    "success" => "end_turn",
                    "refusal" => "refusal",
                    _ => "error",
                }
            );
            wire.extend(ended);
        }
    }
    if matches!(stop, "kill" | "cancel" | "close") {
        let _ = release.send(());
    }
    if stop == "kill" {
        drop(agent);
    } else {
        agent.send(&json!({"jsonrpc":"2.0","id":5,"method":"session/list","params":{}}));
        assert_eq!(
            agent.recv()["id"],
            5,
            "no late thought follows terminal idle"
        );
        let (status, stderr) = agent.finish();
        assert!(status.success());
        assert!(!stderr.contains("private-"));
    }
    let mut restored = Agent::configured_with_state(&config, Some(&state));
    restored.initialize(1);
    for _ in 0..2 {
        let replay = restored.resume(&session, root.path(), true);
        let thoughts: Vec<_> = replay
            .iter()
            .filter(|m| m["params"]["update"]["sessionUpdate"] == "agent_thought")
            .collect();
        assert_eq!(thoughts.len(), 1);
        assert_eq!(thoughts[0]["params"]["update"], thought);
        wire.extend(replay);
    }
    let state_only = restored.resume(&session, root.path(), false);
    assert_eq!(state_only.len(), 1);
    // A different session cannot inherit this session's thought entities.
    restored
        .send(&json!({"jsonrpc":"2.0","id":6,"method":"session/new","params":{"cwd":root.path()}}));
    let other = restored.recv()["result"]["sessionId"].clone();
    assert_eq!(restored.resume(&other, root.path(), true).len(), 1);
    restored.send(&json!({"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"next question"}]}}));
    wire.extend(restored.until_idle());
    let request = server.request().body.to_string();
    assert_eq!(request.contains("visible thought"), stop == "success");
    assert_eq!(request.contains("thought question"), stop == "success");
    assert!(!serde_json::to_string(&wire).unwrap().contains("private-"));
    let (status, stderr) = restored.finish();
    assert!(status.success());
    assert!(!stderr.contains("private-"));
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
        "model='test/test'\n[accounts.test]\nprovider='custom'\nmodels=['test']\n[accounts.test.protocols.openai-chat]\nbase_url='{}'",
        http.url
    ));
    // Client interaction support does not trigger a form or login by itself:
    // this fixture exercises cancellation while the model response is pending.
    agent.send(
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":2,"info":{"name":"compatibility-test","version":"0"},
            "capabilities":{"elicitation":{"form":{},"url":{}}}
        }}),
    );
    let initialized = agent.recv();
    assert_eq!(
        initialized["result"]["capabilities"],
        json!({"session":{"mcp":{"acp":{},"stdio":{},"http":{}}}})
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
        "model='test/test'\n[accounts.test]\nprovider='custom'\nmodels=['test']\n[accounts.test.protocols.openai-chat]\nbase_url='{}'",
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
        if params["replayFrom"]["type"] == "start" {
            let user = agent.recv();
            assert_eq!(user["params"]["update"]["sessionUpdate"], "user_message");
            assert_eq!(user["params"]["update"]["messageId"], "user-1");
            let answer = agent.recv();
            assert_eq!(answer["params"]["update"]["sessionUpdate"], "agent_message");
            assert_eq!(answer["params"]["update"]["messageId"], "agent-1");
        }
        let snapshot = agent.recv();
        assert_eq!(snapshot["method"], "session/update");
        assert_eq!(snapshot["params"]["sessionId"], session);
        assert_eq!(snapshot["params"]["update"]["state"], "idle");
        let resumed = agent.recv();
        assert_eq!(
            resumed["id"], 7,
            "replay and the current snapshot precede the response"
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
        "model='test/test'\n[tools.bash]\ntimeout_ms=1000\n[accounts.test]\nprovider='custom'\nmodels=['test']\n[accounts.test.protocols.openai-chat]\nbase_url='{}'",
        server.url
    ));
    agent.initialize(1);
    agent.send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":std::env::temp_dir()}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"run"}]}}));
    let mut terminal = None;
    let mut referenced = false;
    let mut output = Vec::new();
    let mut exited = false;
    loop {
        // recv parses every stdout line, including any accidental child output.
        let message = agent.recv();
        if message["method"] == "session/request_permission" {
            assert_eq!(message["params"]["subject"]["toolCallId"], "tool-1-1");
            agent.send(&json!({"jsonrpc":"2.0","id":message["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}));
        }
        let update = &message["params"]["update"];
        match update["sessionUpdate"].as_str() {
            Some("terminal_update") => {
                terminal = Some(update["terminalId"].clone());
                exited |= update["exitStatus"].is_object();
            }
            Some("tool_call_content_chunk") if update["content"]["type"] == "terminal" => {
                assert_eq!(Some(update["content"]["terminalId"].clone()), terminal);
                referenced = true;
            }
            Some("terminal_output_chunk") => {
                use base64::Engine;
                assert_eq!(Some(update["terminalId"].clone()), terminal);
                output.extend(
                    base64::engine::general_purpose::STANDARD
                        .decode(update["data"].as_str().unwrap())
                        .unwrap(),
                );
            }
            _ => {}
        }
        if message["params"]["update"]["state"] == "idle" {
            break;
        }
    }
    assert!(referenced && exited);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("child-stdout") && output.contains("child-stderr"));
    server.request();
    let result = server.request().body["messages"].to_string();
    assert!(result.contains("child-stdout"));
    assert!(result.contains("child-stderr"));
    assert!(result.contains("timed_out"));
    let (status, stderr) = agent.finish();
    assert!(status.success());
    assert!(stderr.is_empty());
}

#[test]
fn every_file_change_operation_is_serialized_as_standard_v2_diff_content() {
    let root = tempfile::tempdir().unwrap();
    let calls = [
        ("write", json!({"path":"original","content":"old\n"})),
        (
            "edit",
            json!({"path":"original","old_text":"old","new_text":"new"}),
        ),
        ("copy", json!({"source":"original","destination":"copied"})),
        ("move", json!({"source":"copied","destination":"moved"})),
        ("delete", json!({"path":"moved"})),
        ("delete", json!({"path":42})),
    ];
    let mut responses = calls.iter().enumerate().map(|(i,(name,args))| support::Response::json(json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":format!("call-{i}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]}))).collect::<Vec<_>>();
    responses.push(support::Response::json(support::chat_text("done")));
    let server = support::Server::start(responses);
    let mut agent = Agent::configured(&format!(
        "model='test/test'\n[accounts.test]\nprovider='custom'\nmodels=['test']\nprotocols={{openai-chat={{base_url='{}'}}}}",
        server.url
    ));
    agent.initialize(1);
    agent
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"work"}]}}));
    let messages = agent.until_idle_approving_tools();
    let diffs = messages
        .iter()
        .filter_map(|m| {
            let update = &m["params"]["update"];
            (update["sessionUpdate"] == "tool_call_content_chunk"
                && update["content"]["type"] == "diff")
                .then_some(update)
        })
        .collect::<Vec<_>>();
    assert_eq!(diffs.len(), 5);
    for (index, operation) in ["add", "modify", "copy", "move", "delete"]
        .iter()
        .enumerate()
    {
        assert_eq!(diffs[index]["toolCallId"], format!("tool-1-{}", index + 1));
        assert_eq!(
            diffs[index]["content"]["changes"][0]["operation"],
            *operation
        );
        assert!(
            std::path::Path::new(
                diffs[index]["content"]["changes"][0]["path"]
                    .as_str()
                    .unwrap()
            )
            .is_absolute()
        );
        if cfg!(windows) && *operation == "delete" {
            assert!(
                diffs[index]["content"].get("patch").is_none(),
                "Windows has no authoritative executable mode"
            );
        } else {
            assert_eq!(diffs[index]["content"]["patch"]["format"], "git_patch");
            assert!(
                diffs[index]["content"]["patch"]["text"]
                    .as_str()
                    .unwrap()
                    .starts_with("diff --git \"")
            );
        }
    }
    assert!(
        messages
            .iter()
            .any(|m| m["params"]["update"]["toolCallId"] == "tool-1-6"
                && m["params"]["update"]["status"] == "failed")
    );
    assert!(agent.finish().0.success());
}

#[rstest::rstest]
#[case::malformed(false, "bash", json!({}))]
#[case::null_outcome(false, "bash", json!({"outcome":null}))]
#[case::wrong_outcome(false, "write", json!({"outcome":42}))]
#[case::missing_id(false, "write", json!({"outcome":{"outcome":"selected"}}))]
#[case::null_id(false, "bash", json!({"outcome":{"outcome":"selected","optionId":null}}))]
#[case::numeric_id(false, "write", json!({"outcome":{"outcome":"selected","optionId":42}}))]
#[case::unknown_id(false, "write", json!({"outcome":{"outcome":"selected","optionId":"unoffered"}}))]
#[case::unknown_outcome(false, "bash", json!({"outcome":{"outcome":"future"}}))]
#[case::cancelled(false, "write", json!({"outcome":{"outcome":"cancelled"}}))]
#[case::reject_always(false, "write", json!({"outcome":{"outcome":"selected","optionId":"reject_always"}}))]
#[case::request_error(false, "write", Value::Null)]
#[case::direct_close(true, "bash", json!({}))]
#[case::close_mutation(true, "write", json!({}))]
#[test]
fn unapproved_stdio_commands_never_run_and_close_follows_request_cleanup(
    #[case] close: bool,
    #[case] tool: &str,
    #[case] response: Value,
) {
    let root = tempfile::tempdir().unwrap();
    let arguments = if tool == "bash" {
        json!({"command":"printf bad > sentinel"})
    } else {
        json!({"path":"sentinel","content":"bad"})
    };
    let server = support::Server::start(vec![
        support::Response::json(
            json!({"choices":[{"index":0,"message":{"tool_calls":[{"id":"protected","type":"function","function":{"name":tool,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]}),
        ),
        support::Response::json(support::chat_text("declined")),
    ]);
    let mut agent = Agent::configured(&format!(
        "model='test/test'\n[accounts.test]\nprovider='custom'\nmodels=['test']\nprotocols={{openai-chat={{base_url='{}'}}}}",
        server.url
    ));
    agent.initialize(1);
    agent
        .send(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path()}}));
    let session = agent.recv()["result"]["sessionId"].clone();
    agent.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"run"}]}}));
    let permission = loop {
        let message = agent.recv();
        assert_ne!(message["params"]["update"]["status"], "in_progress");
        assert_ne!(
            message["params"]["update"]["sessionUpdate"],
            "terminal_update"
        );
        assert_ne!(message["params"]["update"]["content"]["type"], "diff");
        if message["method"] == "session/request_permission" {
            break message;
        }
    };
    assert_eq!(permission["params"]["sessionId"], session);
    assert_eq!(permission["params"]["options"].as_array().unwrap().len(), 4);
    if tool == "bash" {
        assert_eq!(
            permission["params"]["subject"],
            json!({"type":"command","command":"printf bad > sentinel","cwd":root.path(),"toolCallId":"tool-1-1"})
        );
    } else {
        let subject = &permission["params"]["subject"];
        assert_eq!(subject["type"], "tool_call");
        assert_eq!(subject["toolCall"]["toolCallId"], "tool-1-1");
        assert_eq!(subject["toolCall"]["rawInput"], arguments);
        assert_eq!(subject["toolCall"]["status"], "pending");
    }
    if close {
        agent.send(&json!({"jsonrpc":"2.0","id":4,"method":"session/close","params":{"sessionId":session}}));
    } else if response.is_null() {
        agent.send(&json!({"jsonrpc":"2.0","id":permission["id"],"error":{"code":-32603,"message":"CLIENT-SECRET"}}));
    } else {
        agent.send(&json!({"jsonrpc":"2.0","id":permission["id"],"result":response}));
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
        agent.send(&json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_always"}}}));
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
        "model='test/test'\n[agent]\nmax_steps = 1\n[accounts.test]\nprovider='custom'\nmodels=['test']\n[accounts.test.protocols.openai-chat]\nbase_url = '{}'",
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
        [
            "bash",
            "copy",
            "delete",
            "edit",
            "glob",
            "grep",
            "list",
            "move",
            "read",
            "update_plan",
            "write"
        ]
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
    let mut definitions = executor.definitions();
    // ACP's turn-owned operation also consumes the model's context budget.
    definitions.push(bee::acp::plan_tool());
    let report = model
        .context(&[bee::llm::Message::User(first.clone())], &definitions)
        .unwrap();
    let window = report.estimated_input + report.reserved_output + 400;
    let mut agent = Agent::configured(&format!(
        "model='test/test'\n[accounts.test]\nprovider='custom'\nmodels=['test']\nmax_output_tokens=20\ncontext_window={window}\n[accounts.test.protocols.openai-chat]\nbase_url='{}'",
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
    let recovered = server.request().body["messages"].to_string();
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
fn the_initialize_response_advertises_all_implemented_mcp_transports() {
    let mut agent = Agent::spawn();

    let response = agent.initialize(1);

    // Capabilities promise implemented behavior, including the exact MCP transport.
    assert_eq!(
        response["result"]["capabilities"]["session"],
        json!({"mcp":{"acp":{},"http":{},"stdio":{}}})
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
            .contains("choose an available account-qualified model")
    );
    assert!(states[1].to_string().contains("over ACP before prompting"));
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
fn invalid_and_unadvertised_content_is_rejected_before_insertion() {
    let mut agent = Agent::spawn();
    agent.initialize(1);
    let session = new_session(&mut agent, 2);
    let mut prompts = vec![json!("not an array")];
    for block in [
        json!(null),
        json!(7),
        json!([]),
        json!({}),
        json!({"type":null}),
        json!({"type":42}),
        json!({"type":"text"}),
        json!({"type":"text","text":7}),
        json!({"type":"image"}),
        json!({"type":"image","data":7,"mimeType":"image/png"}),
        json!({"type":"audio"}),
        json!({"type":"audio","data":"aGk=","mimeType":7}),
        json!({"type":"resource_link","uri":"file:///x"}),
        json!({"type":"resource_link","name":"x","uri":7}),
        json!({"type":"resource"}),
        json!({"type":"resource","resource":7}),
        json!({"type":"image","data":"aGk=","mimeType":"image/png"}),
        json!({"type":"audio","data":"aGk=","mimeType":"audio/wav"}),
        json!({"type":"resource","resource":{"uri":"file:///x","text":"hello"}}),
        json!({"type":"resource","resource":{"uri":"file:///x","blob":"aGk="}}),
    ] {
        // A valid sibling must not let a malformed or unsupported block be skipped.
        prompts.push(json!([{"type":"text","text":"must not be inserted"}, block]));
    }
    for (index, prompt) in prompts.into_iter().enumerate() {
        let id = index + 3;
        agent.send(&json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{"sessionId":session,"prompt":prompt}}));
        let error = agent.recv();
        // Reading the error first also proves no acceptance/update preceded it.
        assert_eq!(error["id"], id, "{error}");
        assert_eq!(error["error"]["code"], -32602, "{error}");
    }
    agent.send(&json!({"jsonrpc":"2.0","id":100,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"valid"}]}}));
    let messages = agent.until_idle();
    assert_eq!(messages[0]["result"]["messageId"], "user-1");
    assert_eq!(
        messages[1]["params"]["update"]["content"],
        json!([{"type":"text","text":"valid"}])
    );
    assert!(agent.finish().0.success());
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
