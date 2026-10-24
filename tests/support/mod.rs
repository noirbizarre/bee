//! Local wire fixtures shared by the protocol tests.

// Each integration-test crate uses a different subset of these helpers.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use bee::config::{Config, Environment, project_config_path};
use bee::provider::{Profile, Providers};
use serde_json::{Value, json};

/// Select native Git Bash explicitly on Windows, where PATH may prefer WSL.
pub fn native_bash_config(text: &str) -> String {
    #[cfg(windows)]
    {
        let shell = std::path::PathBuf::from(std::env::var_os("ProgramFiles").unwrap())
            .join("Git/bin/bash.exe");
        assert!(
            shell.is_file(),
            "the Windows test host must have Git Bash installed"
        );
        let setting = format!("shell = '{}'\n", shell.display());
        if text.contains("[tools.bash]") {
            text.replacen("[tools.bash]", &format!("[tools.bash]\n{setting}"), 1)
        } else {
            format!("{text}\n[tools.bash]\n{setting}")
        }
    }
    #[cfg(not(windows))]
    text.into()
}

/// Exercise the host's native Bash without changing process-global PATH.
pub fn bash_settings() -> bee::tool::bash::Settings {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("bash.toml");
    std::fs::write(&path, native_bash_config("")).unwrap();
    let config = Config::load(&Environment::new(None, None, Some(path)), root.path()).unwrap();
    bee::tool::bash::Settings::from_config(&config).unwrap()
}

pub struct Response {
    pub status: u16,
    pub kind: &'static str,
    pub body: String,
    pub delay: Duration,
    pub pause: Option<(usize, Duration)>,
}

impl Response {
    pub fn json(value: Value) -> Self {
        Self {
            status: 200,
            kind: "application/json",
            body: value.to_string(),
            delay: Duration::ZERO,
            pause: None,
        }
    }

    pub fn sse(body: String) -> Self {
        Self {
            status: 200,
            kind: "text/event-stream",
            body,
            delay: Duration::ZERO,
            pause: None,
        }
    }
}

pub struct Request {
    pub head: String,
    pub body: Value,
}

pub struct Server {
    pub url: String,
    requests: Receiver<Request>,
}

impl Server {
    pub fn start(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/prefix", listener.local_addr().unwrap());
        let (tx, requests) = mpsc::channel();
        std::thread::spawn(move || {
            for response in responses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut byte = [0];
                while !bytes.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                }
                let head = String::from_utf8(bytes).unwrap();
                let len: usize = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                let mut body = vec![0; len];
                socket.read_exact(&mut body).unwrap();
                tx.send(Request {
                    head,
                    body: serde_json::from_slice(&body).unwrap(),
                })
                .unwrap();
                std::thread::sleep(response.delay);
                let head = format!(
                    "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.status,
                    response.kind,
                    response.body.len()
                );
                if socket.write_all(head.as_bytes()).is_ok() {
                    if let Some((index, delay)) = response.pause {
                        if socket
                            .write_all(&response.body.as_bytes()[..index])
                            .is_err()
                        {
                            continue;
                        }
                        std::thread::sleep(delay);
                        let _ = socket.write_all(&response.body.as_bytes()[index..]);
                        continue;
                    }
                    // Tiny writes exercise framing independently of JSON boundaries.
                    for chunk in response.body.as_bytes().chunks(3) {
                        if socket.write_all(chunk).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self { url, requests }
    }

    pub fn request(&self) -> Request {
        self.requests.recv_timeout(Duration::from_secs(10)).unwrap()
    }

    pub fn assert_no_more_requests(&self) {
        // Call after inference completes, so a retry cannot still be in flight.
        assert!(
            self.requests.try_recv().is_err(),
            "unexpected extra HTTP request"
        );
    }

    pub async fn request_async(&self) -> Request {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(request) = self.requests.try_recv() {
                    return request;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap()
    }
}

pub fn providers(protocol: &str, url: &str, settings: &str) -> Providers {
    configured_providers(&format!(
        "[providers.test]\nmodel = 'private-model'\n{settings}\n[providers.test.protocols.{protocol}]\nbase_url = '{url}'\n"
    ))
}

pub fn configured_providers(text: &str) -> Providers {
    let root = tempfile::tempdir().unwrap();
    let path = project_config_path(root.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    let env = Environment::new(None, None, None);
    Providers::from_config(&Config::load(&env, root.path()).unwrap()).unwrap()
}

pub fn profile(protocol: &str, url: &str, settings: &str) -> Profile {
    providers(protocol, url, settings)
        .get("test")
        .unwrap()
        .clone()
}

pub fn event(name: &str, value: Value) -> String {
    format!("event: {name}\r\ndata: {value}\r\n\r\n")
}

pub fn chat_text(text: &str) -> Value {
    json!({"choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}})
}

pub fn tool() -> bee::llm::Tool {
    bee::llm::Tool {
        name: "echo".into(),
        description: "Echo input".into(),
        parameters: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
    }
}

pub struct Echo;

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeInput {
    #[schemars(length(min = 1))]
    text: String,
}

pub struct NativeEcho;

impl bee::tool::Tool for NativeEcho {
    type Input = NativeInput;
    type Output = String;

    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo nonempty text"
    }

    async fn execute(
        &self,
        input: NativeInput,
        _: &bee::tool::Context,
    ) -> Result<String, bee::tool::Failure> {
        if input.text == "fail" {
            return Err(bee::tool::Failure::new(
                bee::tool::Code::Execution,
                "Choose different text.",
            ));
        }
        Ok(input.text)
    }
}

pub fn native_registry() -> bee::tool::Registry {
    let mut registry = bee::tool::Registry::default();
    registry.register(NativeEcho).unwrap();
    registry
}

pub fn native_context() -> bee::tool::Context {
    bee::tool::Context::new(
        "/session/project",
        tokio_util::sync::CancellationToken::new(),
        bee::tool::MAX_OUTPUT_BYTES,
    )
    .unwrap()
}

impl bee::agent::Executor for Echo {
    fn execute<'a>(
        &'a self,
        call: &'a bee::llm::Call,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<bee::agent::ToolResult, bee::llm::Error>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Ok(bee::agent::ToolResult {
                output: call.arguments["text"].as_str().unwrap().into(),
                is_error: false,
            })
        })
    }
}
