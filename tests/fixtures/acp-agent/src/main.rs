//! Deterministic JSON-RPC peer with explicit request barriers rather than timing sleeps.
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

fn send(value: Value) {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{value}").unwrap();
    stdout.flush().unwrap();
}

fn reply(id: &Value, result: Value) {
    send(json!({"jsonrpc":"2.0","id":id,"result":result}));
}
fn update(value: Value) {
    send(
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":value}}),
    );
}
fn state(value: &str) {
    update(json!({"sessionUpdate":"state_update","state":value}));
}
fn output() {
    update(
        json!({"sessionUpdate":"agent_message_chunk","messageId":"m","content":{"type":"text","text":"first"}}),
    );
    update(
        json!({"sessionUpdate":"agent_message","messageId":"m","content":[{"type":"text","text":"replacement"}]}),
    );
}

fn elicit(mode: &str, scope: Value) {
    let mut params = scope;
    params["message"] = json!("Provide input");
    if mode == "url" {
        params["mode"] = json!("url");
        params["elicitationId"] = json!("url-1");
        params["url"] = json!("https://example.invalid/login");
    } else {
        params["mode"] = json!("form");
        params["requestedSchema"] = json!({"type":"object","properties":{"count":{"type":"integer","minimum":1}},"required":["count"]});
    }
    send(json!({"jsonrpc":"2.0","id":"elicitation","method":"elicitation/create","params":params}));
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "descendant" {
        std::fs::write(
            std::env::var_os("FIXTURE_DESCENDANT_PID").unwrap(),
            std::process::id().to_string(),
        )
        .unwrap();
        loop {
            std::thread::park();
        }
    }
    if mode == "descendants" {
        // Inherit the SDK-created process group without introducing a shell dependency.
        let _child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("descendant")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
    }
    if let Some(path) = std::env::var_os("FIXTURE_PID") {
        std::fs::write(path, std::process::id().to_string()).unwrap();
    }
    let mut setup = None;
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        let id = &request["id"];
        match request["method"].as_str() {
            Some("initialize") => {
                if let Some(path) = std::env::var_os("FIXTURE_CAPABILITIES") {
                    std::fs::write(path, request["params"]["capabilities"].to_string()).unwrap();
                }
                if mode == "hang-init" {
                    continue;
                }
                if mode == "malformed" {
                    reply(id, json!({"nonsense":true}));
                    continue;
                }
                let version = if mode == "v1" { 1 } else { 2 };
                let capabilities = if mode == "no-sessions" {
                    json!({})
                } else {
                    json!({"session":{}})
                };
                reply(
                    id,
                    json!({"protocolVersion":version,"info":{"name":"fixture","version":"0"},"capabilities":capabilities}),
                );
            }
            Some("session/new") => {
                if mode == "hang-new" {
                    continue;
                }
                if mode == "form-setup" {
                    setup = Some(id.clone());
                    elicit("form", json!({"requestId":id}));
                } else {
                    reply(id, json!({"sessionId":"s"}));
                }
            }
            Some("session/prompt") => {
                state("idle");
                if mode == "eof" {
                    return;
                }
                if mode == "stderr" {
                    for _ in 0..2000 {
                        eprintln!("SECRET_FIXTURE_STDERR");
                    }
                    std::process::exit(7);
                }
                if mode == "reject" {
                    send(
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"SECRET_FIXTURE_ERROR"}}),
                    );
                    continue;
                }
                if mode == "form" || mode == "url" {
                    state("requires_action");
                    reply(id, json!({"messageId":"u"}));
                    elicit(&mode, json!({"sessionId":"s","toolCallId":"tool-1"}));
                    continue;
                }
                if mode == "permission" || mode == "permission-wait" {
                    state("requires_action");
                    send(
                        json!({"jsonrpc":"2.0","id":"permission","method":"session/request_permission","params":{
                            "sessionId":"s","title":"Choose","options":[
                                {"optionId":"yes","name":"Yes","kind":"allow_once"},
                                {"optionId":"no","name":"No","kind":"reject_once"}
                            ]
                        }}),
                    );
                    reply(id, json!({"messageId":"u"}));
                    continue;
                }
                state("running");
                if mode == "projection" {
                    reply(id, json!({"messageId":"u"}));
                    output();
                    update(
                        json!({"sessionUpdate":"agent_message","messageId":"m","content":null,"_meta":{"kept":1,"removed":2}}),
                    );
                    update(
                        json!({"sessionUpdate":"agent_message_chunk","messageId":"m","content":{"type":"text","text":"after clear"}}),
                    );
                    update(
                        json!({"sessionUpdate":"agent_message","messageId":"m","_meta":{"removed":null,"added":3}}),
                    );
                    state("idle");
                    continue;
                }
                if mode == "idle-first" {
                    output();
                    state("idle");
                    reply(id, json!({"messageId":"u"}));
                } else {
                    reply(id, json!({"messageId":"u"}));
                    if mode == "hang" || mode == "ignore-cancel" {
                        continue;
                    }
                    if mode == "flood" {
                        for _ in 0..1000 {
                            output();
                        }
                    } else {
                        output();
                    }
                    state("idle");
                }
            }
            Some("session/cancel") => {
                if mode != "ignore-cancel" {
                    state("idle");
                }
            }
            Some("session/close") => {
                if mode == "ignore-close" {
                    continue;
                }
                if let Some(path) = std::env::var_os("FIXTURE_CLOSED") {
                    std::fs::write(path, "closed").unwrap();
                }
                reply(id, json!({}));
            }
            None if id == "permission" => {
                if let Some(path) = std::env::var_os("FIXTURE_PERMISSION") {
                    std::fs::write(path, request["result"].to_string()).unwrap();
                }
                if mode != "permission-wait" {
                    output();
                    state("idle");
                }
            }
            None if id == "elicitation" => {
                if let Some(path) = std::env::var_os("FIXTURE_ELICITATION") {
                    std::fs::write(path, request.to_string()).unwrap();
                }
                if let Some(id) = setup.take() {
                    reply(&id, json!({"sessionId":"s"}));
                } else {
                    if mode == "url" {
                        send(
                            json!({"jsonrpc":"2.0","method":"elicitation/complete","params":{"elicitationId":"url-1"}}),
                        );
                    }
                    output();
                    state("idle");
                }
            }
            _ => {}
        }
    }
}
