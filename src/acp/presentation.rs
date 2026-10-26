//! The sole conversion and ordered publication seam for execution presentation.

use super::{Reply, lock, send_update, v2};
use crate::agent::{Observation, Observer};
use crate::llm::{Call, Error};
use crate::presentation::{Artifact, Event, State, bounded, bounded_value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(super) struct Ledger {
    ids: BTreeMap<String, String>,
    state: Arc<Mutex<State>>,
    admitted: State,
}

impl Ledger {
    pub fn new(state: Arc<Mutex<State>>) -> Self {
        let admitted = lock(&state).clone();
        Self {
            ids: BTreeMap::new(),
            admitted,
            state,
        }
    }
}

pub(super) fn update(value: serde_json::Value) -> v2::SessionUpdate {
    // SDK decoding verifies our SDK-free DTOs against the explicitly pinned wire revision.
    serde_json::from_value(value).expect("presentation reducer produces pinned v2 updates")
}

pub(super) fn finish(ledger: &Mutex<Ledger>, turn: u64, cancelled: bool) -> Vec<v2::SessionUpdate> {
    lock(&lock(ledger).state)
        .finish(turn, cancelled)
        .into_iter()
        .map(update)
        .collect()
}

impl Reply {
    pub(super) fn tool_id(&self, call: &Call) -> String {
        lock(&self.presentation)
            .ids
            .get(&call.id)
            .cloned()
            .expect("announced tool call")
    }

    fn event(
        &self,
        event: Event,
    ) -> Result<Option<tokio::sync::oneshot::Receiver<Result<(), crate::history::Error>>>, Error>
    {
        let mut ledger = lock(&self.presentation);
        ledger
            .admitted
            .apply(self.token, &event)
            .map_err(|_| Error::Limit)?;
        let state = ledger.state.clone();
        let emitted = event.clone();
        let turn = self.token;
        let connection = self.connection.clone();
        let session_id = self.session_id.clone();
        let emit = move || {
            // Failed writes never advance this projection. Cleanup must close the
            // card the client actually saw, not an uncommitted completed result.
            let updates = lock(&state)
                .apply(turn, &emitted)
                .expect("ordered admitted presentation");
            for value in updates {
                let _ = send_update(&connection, &session_id, update(value));
            }
        };
        // The handle was captured before execution: Bash polls under the session gate,
        // so artifact callbacks must never recursively acquire the session registry.
        if let Some(history) = &self.presentation_history {
            Ok(Some(history.presentation(self.token, event, emit)))
        } else {
            emit();
            Ok(None)
        }
    }

    pub(super) fn artifact(
        &self,
        provider_id: &str,
        artifact: Artifact,
    ) -> Result<(), crate::tool::Failure> {
        let id = lock(&self.presentation)
            .ids
            .get(provider_id)
            .cloned()
            .ok_or_else(|| {
                crate::tool::Failure::new(
                    crate::tool::Code::Execution,
                    "Tool presentation is unavailable; retry the turn.",
                )
            })?;
        self.event(Event::Artifact { id, artifact })
            .map(|_| ())
            .map_err(|_| {
                crate::tool::Failure::new(
                    crate::tool::Code::OutputLimit,
                    "Execution presentation exceeded its bound; use a smaller operation.",
                )
            })
    }
}

impl Observer for Reply {
    fn observe<'a>(
        &'a self,
        call: &'a Call,
        observation: Observation<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send + 'a>> {
        Box::pin(async move {
            let pending = {
                let sessions = lock(&self.sessions);
                if !sessions.accepts_output(&self.id, self.token) {
                    return Err(Error::Cancelled);
                }
                let event = match observation {
                    Observation::Accepted(description) => {
                        let mut ledger = lock(&self.presentation);
                        let id = format!("tool-{}-{}", self.token, ledger.ids.len() + 1);
                        ledger.ids.insert(call.id.clone(), id.clone());
                        Event::Accepted {
                            id,
                            name: description.name,
                            kind: description.kind.into(),
                            locations: description.locations,
                            input: bounded_value(&call.arguments),
                        }
                    }
                    Observation::Started => Event::Started {
                        id: self.tool_id(call),
                    },
                    Observation::Finished(result) => {
                        let output = serde_json::from_str(&result.output)
                            .unwrap_or_else(|_| result.output.clone().into());
                        Event::Finished {
                            id: self.tool_id(call),
                            output: bounded_value(&output),
                            text: bounded(&result.output, crate::presentation::CALL_BYTES / 4),
                            failed: result.is_error,
                        }
                    }
                };
                self.event(event)?
            };
            if let Some(pending) = pending {
                pending
                    .await
                    .map_err(|_| Error::Tool)?
                    .map_err(|_| Error::Tool)?;
            }
            Ok(())
        })
    }
}
