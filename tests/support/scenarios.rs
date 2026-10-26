//! Real-process transcripts for the deterministic launch path.

use super::{Agent, Value, json};

fn start(name: &str, form: bool) -> (Agent, Value) {
    let mut agent = Agent::scenario(name);
    agent.send(
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":2,"info":{"name":"scenario-client","version":"0"},
            "capabilities":if form {json!({"elicitation":{"form":{}}})} else {json!({})}
        }}),
    );
    assert!(agent.recv().get("result").is_some());
    let session = new_session(&mut agent, 2);
    (agent, session)
}

fn new_session(agent: &mut Agent, id: u64) -> Value {
    agent.send(&json!({"jsonrpc":"2.0","id":id,"method":"session/new","params":{"cwd":agent._isolated.path()}}));
    let response = agent.recv();
    assert_eq!(response["id"], id);
    assert!(
        response["result"]["configOptions"].is_null()
            || response["result"]["configOptions"] == json!([])
    );
    assert!(response["result"]["sessionId"].is_string(), "{response}");
    response["result"]["sessionId"].clone()
}

fn prompt(agent: &mut Agent, session: &Value, id: u64) {
    agent.send(
        &json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{
            "sessionId":session,"prompt":[{"type":"text","text":"ignored fixture prompt"}]
        }}),
    );
    let accepted = agent.recv();
    assert_eq!(accepted["id"], id);
    assert!(accepted.get("result").is_some(), "{accepted}");
    let echo = agent.recv();
    assert_eq!(echo["params"]["sessionId"], *session);
    assert_eq!(echo["params"]["update"]["sessionUpdate"], "user_message");
    assert_eq!(
        echo["params"]["update"]["messageId"],
        accepted["result"]["messageId"]
    );
    let running = agent.recv();
    assert_eq!(running["params"]["update"]["state"], "running");
}

fn updates(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter(|m| m["method"] == "session/update")
        .map(|m| m["params"]["update"].clone())
        .collect()
}

fn text(messages: &[Value]) -> String {
    updates(messages)
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .map(|u| u["content"]["text"].as_str().unwrap())
        .collect()
}

fn terminal(messages: &[Value], reason: &str) {
    let updates = updates(messages);
    assert_eq!(updates.iter().filter(|u| u["state"] == "idle").count(), 1);
    assert_eq!(updates.last().unwrap()["stopReason"], reason);
    // Deserialization catches wire/schema drift as well as JSON shape errors.
    for message in messages.iter().filter(|m| m["method"] == "session/update") {
        let _: agent_client_protocol::schema::v2::UpdateSessionNotification =
            serde_json::from_value(message["params"].clone()).unwrap();
    }
}

fn barrier(agent: &mut Agent) {
    // A response is a delivery barrier; preceding unexpected updates fail here
    // without sleeps or a race-prone "nothing arrived yet" assertion.
    agent.send(&json!({"jsonrpc":"2.0","id":99,"method":"session/list","params":{}}));
    assert_eq!(agent.recv()["id"], 99);
}

fn finish(agent: Agent) {
    for root in ["state", "cache", "data"] {
        assert!(
            !agent._isolated.path().join(root).exists(),
            "created {root}"
        );
    }
    let (status, stderr) = agent.finish();
    assert!(status.success(), "{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn completing_scenarios_repeat_with_normal_ordering_and_memory_replay() {
    for name in ["success", "plan", "error", "input"] {
        let (mut agent, session) = start(name, false);
        let mut previous_plan = None;
        for turn in 0..2 {
            prompt(&mut agent, &session, 10 + turn);
            let messages = agent.until_idle();
            terminal(
                &messages,
                if name == "error" { "error" } else { "end_turn" },
            );
            assert!(messages.iter().all(|m| m["params"]["sessionId"] == session));
            match name {
                "success" => {
                    assert_eq!(text(&messages), "Hello from Bee. Scenario complete.");
                    let chunks: Vec<_> = updates(&messages)
                        .into_iter()
                        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
                        .collect();
                    assert_eq!(chunks.len(), 2);
                    assert_eq!(chunks[0]["messageId"], chunks[1]["messageId"]);
                }
                "plan" => {
                    let plans: Vec<_> = updates(&messages)
                        .into_iter()
                        .filter(|u| u["sessionUpdate"] == "plan_update")
                        .collect();
                    assert_eq!(plans.len(), 2);
                    assert_eq!(plans[0]["plan"]["planId"], plans[1]["plan"]["planId"]);
                    assert_ne!(previous_plan.as_ref(), Some(&plans[0]["plan"]["planId"]));
                    previous_plan = Some(plans[0]["plan"]["planId"].clone());
                    assert_eq!(plans[0]["plan"]["entries"][0]["status"], "in_progress");
                    assert_eq!(plans[0]["plan"]["entries"][1]["status"], "pending");
                    assert!(
                        plans[1]["plan"]["entries"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .all(|e| e["status"] == "completed")
                    );
                    assert_eq!(text(&messages), "Plan complete.");
                }
                "error" => {
                    assert_eq!(
                        text(&messages),
                        "Partial output before the controlled error."
                    );
                    let idle = &messages.last().unwrap()["params"]["update"];
                    assert_eq!(idle["error"]["code"], -32603);
                    assert_eq!(
                        idle["error"]["data"],
                        "Controlled scenario failure; send another prompt to repeat it."
                    );
                }
                "input" => assert_eq!(text(&messages), "Input unavailable, declined, or invalid."),
                _ => unreachable!(),
            }
            barrier(&mut agent);
        }
        let cwd = agent._isolated.path().to_owned();
        let replay = agent.resume(&session, &cwd, true);
        assert!(
            updates(&replay)
                .iter()
                .any(|u| u["sessionUpdate"] == "agent_message"),
            "{replay:?}"
        );
        if name == "plan" {
            assert!(
                updates(&replay)
                    .iter()
                    .any(|u| u["sessionUpdate"] == "plan_update")
            );
        }
        barrier(&mut agent);
        finish(agent);
    }
}

fn request(agent: &Agent, method: &str) -> (Vec<Value>, Value) {
    let mut before: Vec<Value> = Vec::new();
    loop {
        let message = agent.recv();
        if message["method"] == method {
            assert_eq!(
                before.last().unwrap()["params"]["update"]["state"],
                "requires_action"
            );
            return (before, message);
        }
        assert_ne!(message["params"]["update"]["state"], "idle", "{message}");
        before.push(message);
    }
}

#[test]
fn permission_scenarios_correlate_cards_and_fail_closed_without_remembering_always() {
    let (mut agent, session) = start("permission", false);
    for (index, choice) in [
        "allow_once",
        "reject_once",
        "allow_always",
        "reject_always",
        "unknown",
        "cancelled",
        "error",
    ]
    .into_iter()
    .enumerate()
    {
        prompt(&mut agent, &session, 10 + index as u64);
        let (before, request) = request(&agent, "session/request_permission");
        let card = updates(&before)
            .into_iter()
            .find(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "pending")
            .unwrap();
        assert_eq!(request["params"]["subject"]["type"], "tool_call");
        assert_eq!(
            request["params"]["subject"]["toolCall"]["toolCallId"],
            card["toolCallId"]
        );
        assert_eq!(
            request["params"]["subject"]["toolCall"]["status"],
            "pending"
        );
        assert_eq!(request["params"]["options"].as_array().unwrap().len(), 4);
        let response = match choice {
            "error" => {
                json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"controlled client error"}})
            }
            "cancelled" => {
                json!({"jsonrpc":"2.0","id":request["id"],"result":{"outcome":{"outcome":"cancelled"}}})
            }
            _ => {
                json!({"jsonrpc":"2.0","id":request["id"],"result":{"outcome":{"outcome":"selected","optionId":choice}}})
            }
        };
        agent.send(&response);
        let tail = agent.until_idle();
        terminal(&tail, "end_turn");
        assert_eq!(tail[0]["params"]["update"]["state"], "running");
        let allowed = choice.starts_with("allow_");
        assert_eq!(
            text(&tail),
            if allowed {
                "Permission approved."
            } else {
                "Permission denied."
            }
        );
        let statuses: Vec<_> = updates(&tail)
            .into_iter()
            .filter(|u| u["sessionUpdate"] == "tool_call_update")
            .map(|u| u["status"].clone())
            .collect();
        assert_eq!(
            statuses,
            if allowed {
                vec![json!("in_progress"), json!("completed")]
            } else {
                vec![json!("failed")]
            }
        );
        barrier(&mut agent);
    }
    finish(agent);
}

#[test]
fn form_scenarios_validate_responses_and_correlate_tool_scope() {
    let (mut agent, session) = start("input", true);
    for (index, result) in [
        json!({"action":"accept","content":{"answer":"blue"}}),
        json!({"action":"decline"}),
        json!({"action":"cancel"}),
        json!({"action":"accept","content":{"answer":42}}),
        json!({"action":"accept","content":{"answer":""}}),
        json!({"action":"accept","content":{"answer":"x".repeat(4097)}}),
    ]
    .into_iter()
    .enumerate()
    {
        prompt(&mut agent, &session, 10 + index as u64);
        let (before, request) = request(&agent, "elicitation/create");
        let card = updates(&before)
            .into_iter()
            .find(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "pending")
            .unwrap();
        assert_eq!(request["params"]["sessionId"], session);
        assert_eq!(request["params"]["toolCallId"], card["toolCallId"]);
        agent.send(&json!({"jsonrpc":"2.0","id":request["id"],"result":result}));
        let tail = agent.until_idle();
        terminal(&tail, "end_turn");
        assert_eq!(tail[0]["params"]["update"]["state"], "running");
        assert_eq!(
            text(&tail),
            if index == 0 {
                "Input accepted."
            } else {
                "Input unavailable, declined, or invalid."
            }
        );
        barrier(&mut agent);
    }
    finish(agent);
}

#[test]
fn cancellation_and_close_drop_pending_scenario_work_before_idle() {
    for name in ["cancel", "permission", "input"] {
        for close in [false, true] {
            let (mut agent, session) = start(name, true);
            prompt(&mut agent, &session, 10);
            let pending = if name == "cancel" {
                let ready = agent.recv();
                assert_eq!(text(&[ready]), "Ready for cancellation.");
                None
            } else {
                Some(
                    request(
                        &agent,
                        if name == "permission" {
                            "session/request_permission"
                        } else {
                            "elicitation/create"
                        },
                    )
                    .1,
                )
            };
            // A waiting session rejects overlap without blocking another session.
            agent.send(&json!({"jsonrpc":"2.0","id":11,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"overlap"}]}}));
            assert!(agent.recv().get("error").is_some());
            let other = new_session(&mut agent, 12);
            assert_ne!(other, session);
            agent.send(&if close {
                json!({"jsonrpc":"2.0","id":13,"method":"session/close","params":{"sessionId":session}})
            } else {
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}})
            });
            let tail = agent.until_idle();
            terminal(&tail, "cancelled");
            if let Some(pending) = pending {
                assert!(
                    tail.iter().any(|m| m["method"] == "$/cancel_request"
                        && m["params"]["requestId"] == pending["id"]),
                    "{tail:?}"
                );
                assert!(updates(&tail).iter().any(
                    |u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "cancelled"
                ));
                // An SDK-cancelled request's late response cannot revive the turn.
                agent.send(&json!({"jsonrpc":"2.0","id":pending["id"],"result":if name == "permission" {json!({"outcome":{"outcome":"selected","optionId":"allow_once"}})} else {json!({"action":"accept","content":{"answer":"late"}})}}));
            }
            if close {
                assert_eq!(agent.recv()["id"], 13);
            }
            barrier(&mut agent);
            if !close {
                prompt(&mut agent, &session, 14);
                if name == "cancel" {
                    agent.recv();
                } else {
                    request(
                        &agent,
                        if name == "permission" {
                            "session/request_permission"
                        } else {
                            "elicitation/create"
                        },
                    );
                }
                agent.send(&json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}));
                terminal(&agent.until_idle(), "cancelled");
                barrier(&mut agent);
            }
            finish(agent);
        }
    }
}

#[test]
fn scenario_sessions_do_not_survive_process_restart() {
    let (mut first, session) = start("success", false);
    prompt(&mut first, &session, 10);
    terminal(&first.until_idle(), "end_turn");
    // Preserve every launch directory so accidental persistence is visible to
    // the replacement process instead of hidden by a fresh temporary home.
    let isolated = std::sync::Arc::clone(&first._isolated);
    finish(first);
    let mut second = Agent::scenario_in("success", isolated);
    second.initialize(1);
    second.send(&json!({"jsonrpc":"2.0","id":2,"method":"session/list","params":{}}));
    assert_eq!(second.recv()["result"]["sessions"], json!([]));
    second.send(&json!({"jsonrpc":"2.0","id":3,"method":"session/resume","params":{"sessionId":session,"cwd":second._isolated.path()}}));
    assert!(second.recv().get("error").is_some());
    finish(second);
}
