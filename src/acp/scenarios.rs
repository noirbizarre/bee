//! Fixed turn bodies; transport, cancellation and finalization remain shared.

use super::{Reply, TurnFailure};
use crate::agent::{Observation, Observer, ToolResult};
use crate::llm::{Call, Error};
use crate::presentation::Description;
use crate::scenario::Scenario;
use serde_json::json;

pub(super) async fn run(scenario: Scenario, reply: Reply) -> Result<(), TurnFailure> {
    if scenario == Scenario::Error {
        reply.text("Partial output before the controlled error.");
        return Err(TurnFailure::new(
            "Controlled scenario failure; send another prompt to repeat it.",
        ));
    }
    execute(scenario, &reply)
        .await
        .map_err(|_| TurnFailure::new("Scenario operation failed; retry the prompt."))
}

async fn execute(scenario: Scenario, reply: &Reply) -> Result<(), Error> {
    match scenario {
        Scenario::Success => {
            reply.text("Hello from Bee. ");
            reply.text("Scenario complete.");
        }
        Scenario::Plan => {
            for completed in [false, true] {
                let result = reply.update_plan(&Call {
                    id: "scenario-plan".into(),
                    name: "update_plan".into(),
                    arguments: json!({"entries":[
                        {"content":"Inspect fixture","priority":"high","status":if completed {"completed"} else {"in_progress"}},
                        {"content":"Verify fixture","priority":"medium","status":if completed {"completed"} else {"pending"}}
                    ]}),
                }).await?;
                if result.is_error {
                    return Err(Error::Tool);
                }
            }
            reply.text("Plan complete.");
        }
        Scenario::Permission | Scenario::Input => interaction(scenario, reply).await?,
        Scenario::Cancel => {
            // Clients synchronize on this chunk, never on a guessed delay.
            reply.text("Ready for cancellation.");
            std::future::pending::<()>().await;
        }
        Scenario::Error => unreachable!("controlled failure is handled before execution"),
    }
    Ok(())
}

async fn interaction(scenario: Scenario, reply: &Reply) -> Result<(), Error> {
    let input = scenario == Scenario::Input;
    let call = Call {
        id: "scenario-interaction".into(),
        name: if input { "ask_user" } else { "scenario_noop" }.into(),
        arguments: if input {
            json!({"question":"Enter a non-secret scenario answer."})
        } else {
            json!({})
        },
    };
    let description = Description {
        name: if input {
            "Scenario input"
        } else {
            "Scenario no-op"
        }
        .into(),
        kind: "other",
        locations: vec![],
    };
    // Accepted allocates the correlation ID required by both request helpers.
    reply
        .observe(&call, Observation::Accepted(description.clone()))
        .await?;
    let result = if input {
        reply.observe(&call, Observation::Started).await?;
        reply.ask(&call, 64 * 1024).await?
    } else {
        let subject = reply.tool_permission_subject(&call, &description)?;
        let allowed = reply
            .permission(subject, "Approve scenario no-op".into())
            .await?;
        if allowed {
            reply.observe(&call, Observation::Started).await?;
        }
        ToolResult {
            output: if allowed { json!({"data":{"approved":true}}) } else {
                json!({"error":{"code":"permission_denied","message":"Scenario operation was not approved."}})
            }.to_string(),
            is_error: !allowed,
        }
    };
    reply.observe(&call, Observation::Finished(&result)).await?;
    reply.text(match (input, result.is_error) {
        (true, false) => "Input accepted.",
        (true, true) => "Input unavailable, declined, or invalid.",
        (false, false) => "Permission approved.",
        (false, true) => "Permission denied.",
    });
    Ok(())
}
