//! The stdio transport, exercised through the real binary.
//!
//! These spawn `bee` the way an ACPv2 client would, because the properties
//! under test (a clean stdout, exit codes, shutdown on EOF) belong to the
//! process, not to the library.

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
}

impl Agent {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_bee"))
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
        Self {
            child,
            stdin,
            lines,
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

        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .expect("stderr is piped")
            .read_to_string(&mut stderr)
            .expect("stderr is readable");
        (status, stderr)
    }
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
fn the_initialize_response_advertises_no_session_capability() {
    let mut agent = Agent::spawn();

    let response = agent.initialize(1);

    // Advertising what is not implemented would send clients to methods Bee
    // cannot answer.
    assert!(response["result"]["capabilities"].get("session").is_none());
    agent.finish();
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
