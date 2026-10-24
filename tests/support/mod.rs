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
    let root = tempfile::tempdir().unwrap();
    let path = project_config_path(root.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("[providers.test]\nmodel = 'private-model'\n{settings}\n[providers.test.protocols.{protocol}]\nbase_url = '{url}'\n")).unwrap();
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
