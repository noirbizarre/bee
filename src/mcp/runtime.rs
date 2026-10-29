//! Session-owned direct connections with cleanup independent of inference futures.

use super::{
    config::Transport,
    transport::{Capture, Http, Owner, Stdio, owned},
};
use rmcp::{
    RoleClient,
    service::{RunningService, serve_client_with_ct},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub(crate) const UNAVAILABLE: &str =
    "MCP server is unavailable; check its configuration and permission, then retry";

/// An entry remains registered even when a caller drops initialization or a call.
pub(crate) struct Entry {
    pub definition: Transport,
    pub revision: String,
    state: tokio::sync::Mutex<State>,
    busy: AtomicBool,
    stop: CancellationToken,
}
#[derive(Default)]
struct State {
    service: Option<RunningService<RoleClient, rmcp::model::ClientConfig>>,
    child: Option<Process>,
    capture: Capture,
    transport: Option<Owner>,
}

struct Process(Box<dyn process_wrap::tokio::ChildWrapper>, bool);
impl Process {
    async fn close(&mut self) {
        if self.1 {
            return;
        }
        self.kill();
        let _ = self.0.wait().await;
        // Never signal a recycled process-group ID from a later Drop.
        self.1 = true;
    }
    fn kill(&mut self) {
        if self.1 {
            return;
        }
        // Wrappers signal the entire process group/job, including pipe owners.
        let _ = self.0.start_kill();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

impl Entry {
    pub fn new(definition: Transport) -> Arc<Self> {
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random)
            .expect("OS randomness is required for connection permission identity");
        let revision = random.iter().map(|b| format!("{b:02x}")).collect();
        Arc::new(Self {
            definition,
            revision,
            state: Default::default(),
            busy: AtomicBool::new(false),
            stop: CancellationToken::new(),
        })
    }
    pub async fn ready(&self) -> bool {
        self.state
            .lock()
            .await
            .service
            .as_ref()
            .is_some_and(|s| !s.is_transport_closed())
    }
    pub async fn connect(&self) -> Result<(), &'static str> {
        self.busy.store(true, Ordering::Release);
        let mut state = self.state.lock().await;
        if let Some(child) = state.child.as_mut() {
            child.close().await;
        }
        if let Some(service) = state.service.as_mut() {
            let _ = service.close().await;
        }
        if let Some(owner) = state.transport.as_mut() {
            owner.close().await;
        }
        *state = State::default();
        let cancellation = self.stop.clone();
        let capture = state.capture.clone();
        // Pin the direct subset independently of SDK future draft defaults.
        let info = rmcp::model::ClientConfig::new(
            rmcp::model::ClientCapabilities::default(),
            rmcp::model::Implementation::new("bee", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(rmcp::model::ProtocolVersion::V_2025_11_25);
        let operation = async {
            let service = match &self.definition {
                Transport::Stdio {
                    command,
                    args,
                    cwd,
                    env,
                } => {
                    let mut command_builder = tokio::process::Command::new(command);
                    command_builder
                        .args(args)
                        .current_dir(cwd)
                        .env_clear()
                        .envs(env)
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null())
                        .kill_on_drop(true);
                    let mut command_builder =
                        process_wrap::tokio::CommandWrap::from(command_builder);
                    command_builder.wrap(process_wrap::tokio::KillOnDrop);
                    #[cfg(unix)]
                    command_builder.wrap(process_wrap::tokio::ProcessGroup::leader());
                    #[cfg(windows)]
                    command_builder.wrap(process_wrap::tokio::JobObject);
                    let mut child =
                        Process(command_builder.spawn().map_err(|_| UNAVAILABLE)?, false);
                    let reader =
                        tokio::io::BufReader::new(child.0.stdout().take().ok_or(UNAVAILABLE)?);
                    let writer = Arc::new(tokio::sync::Mutex::new(
                        child.0.stdin().take().ok_or(UNAVAILABLE)?,
                    ));
                    state.child = Some(child);
                    let (transport, owner) = owned(Stdio {
                        reader,
                        writer,
                        capture,
                        buffer: Vec::new(),
                    });
                    state.transport = Some(owner);
                    Box::pin(serve_client_with_ct(info, transport, cancellation))
                        .await
                        .map_err(|_| UNAVAILABLE)?
                }
                Transport::Http { url, headers } => {
                    let client = Http::new(capture).map_err(|_| UNAVAILABLE)?;
                    let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone());
                    config.reinit_on_expired_session = false;
                    config.max_sse_event_size = super::MAX_BYTES;
                    config.custom_headers = headers
                        .iter()
                        .map(|(k, v)| {
                            Ok::<_, &'static str>((
                                http::HeaderName::from_bytes(k.as_bytes())
                                    .map_err(|_| UNAVAILABLE)?,
                                http::HeaderValue::from_str(v).map_err(|_| UNAVAILABLE)?,
                            ))
                        })
                        .collect::<Result<_, _>>()?;
                    let (transport, owner) =
                        owned(StreamableHttpClientTransport::with_client(client, config));
                    state.transport = Some(owner);
                    Box::pin(serve_client_with_ct(info, transport, cancellation))
                        .await
                        .map_err(|_| UNAVAILABLE)?
                }
            };
            let supported = service.peer().peer_info().is_some_and(|info| {
                matches!(
                    info.protocol_version.as_str(),
                    "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25"
                )
            });
            state.service = Some(service);
            if !supported {
                return Err(
                    "unsupported MCP protocol version; use a server supporting MCP through 2025-11-25",
                );
            }
            Ok(())
        };
        let result = tokio::time::timeout(Duration::from_secs(30), operation)
            .await
            .map_err(|_| UNAVAILABLE)?;
        if result.is_ok() {
            self.busy.store(false, Ordering::Release);
        }
        result
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, &'static str> {
        // A timed-out or cancelled operation poisons this connection until the
        // turn's external cleanup barrier retires it. Never reuse uncertain I/O.
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(UNAVAILABLE);
        }
        let state = self.state.lock().await;
        let request = serde_json::from_value(serde_json::json!({"method":method,"params":params}))
            .map_err(|_| UNAVAILABLE)?;
        let operation = state
            .service
            .as_ref()
            .ok_or(UNAVAILABLE)?
            .peer()
            .send_request(request);
        let result = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(120), operation) => result,
            _ = self.stop.cancelled() => return Err(UNAVAILABLE),
        };
        if matches!(result, Ok(Ok(_))) {
            let envelope = state.capture.take().ok_or(UNAVAILABLE)?;
            self.busy.store(false, Ordering::Release);
            Ok(envelope)
        } else {
            Err(UNAVAILABLE)
        }
    }
    pub async fn close(&self) {
        self.stop.cancel();
        let mut state = self.state.lock().await;
        // Kill/reap children before awaiting SDK closure: a peer that ignores EOF
        // must not keep the session's finalization barrier waiting indefinitely.
        if let Some(child) = state.child.as_mut() {
            child.close().await;
        }
        if let Some(service) = state.service.as_mut() {
            let _ = service.close().await;
        }
        if let Some(owner) = state.transport.as_mut() {
            owner.close().await;
        }
        *state = State::default();
        state.capture.take();
    }
    pub fn uncertain(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}

/// Lives on the ACP connection, while each session owns its independent entries.
#[derive(Default)]
pub(crate) struct Manager {
    sessions: std::sync::Mutex<BTreeMap<String, BTreeMap<String, Arc<Entry>>>>,
    closing: std::sync::Mutex<std::collections::BTreeSet<String>>,
    retired: std::sync::Mutex<BTreeMap<String, Vec<Arc<Entry>>>>,
}
impl Manager {
    pub fn changed(&self, session: &str, definitions: &BTreeMap<String, Transport>) -> bool {
        self.sessions
            .lock()
            .expect("MCP manager lock")
            .get(session)
            .is_some_and(|entries| {
                entries
                    .iter()
                    .any(|(id, entry)| definitions.get(id) != Some(&entry.definition))
            })
    }
    pub fn closing(&self, session: &str) {
        self.closing
            .lock()
            .expect("MCP closing lock")
            .insert(session.into());
    }
    pub fn reopen(&self, session: &str) {
        self.closing
            .lock()
            .expect("MCP closing lock")
            .remove(session);
    }
    pub fn is_closing(&self, session: &str) -> bool {
        self.closing
            .lock()
            .expect("MCP closing lock")
            .contains(session)
    }
    pub async fn reconcile(&self, session: &str, definitions: &BTreeMap<String, Transport>) {
        let retired = {
            let mut sessions = self.sessions.lock().expect("MCP manager lock");
            let entries = sessions.entry(session.into()).or_default();
            let old = std::mem::take(entries);
            let mut retired = Vec::new();
            for (id, entry) in old {
                if definitions.get(&id) == Some(&entry.definition) && !entry.uncertain() {
                    entries.insert(id, entry);
                } else {
                    retired.push(entry);
                }
            }
            for (id, definition) in definitions {
                entries
                    .entry(id.clone())
                    .or_insert_with(|| Entry::new(definition.clone()));
            }
            retired
        };
        self.retire(session, retired).await;
    }
    pub fn get(&self, session: &str, id: &str) -> Option<Arc<Entry>> {
        self.sessions
            .lock()
            .expect("MCP manager lock")
            .get(session)?
            .get(id)
            .cloned()
    }
    pub async fn finish(&self, session: &str, all: bool) {
        let retired = {
            let mut sessions = self.sessions.lock().expect("MCP manager lock");
            let entries = sessions.entry(session.into()).or_default();
            let ids: Vec<_> = entries
                .iter()
                .filter(|(_, entry)| all || entry.uncertain())
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| entries.remove(&id))
                .collect::<Vec<_>>()
        };
        self.retire(session, retired).await;
    }
    async fn retire(&self, session: &str, entries: Vec<Arc<Entry>>) {
        // Retired ownership must survive cancellation during reload/replacement,
        // not merely active I/O. Only remove an entry after its close was awaited.
        self.retired
            .lock()
            .expect("MCP retired lock")
            .entry(session.into())
            .or_default()
            .extend(entries);
        let retired = self
            .retired
            .lock()
            .expect("MCP retired lock")
            .get(session)
            .cloned()
            .unwrap_or_default();
        for entry in retired {
            entry.close().await;
            if let Some(entries) = self
                .retired
                .lock()
                .expect("MCP retired lock")
                .get_mut(session)
            {
                entries.retain(|e| !Arc::ptr_eq(e, &entry));
            }
        }
    }
    pub async fn shutdown(&self) {
        let sessions = std::mem::take(&mut *self.sessions.lock().expect("MCP manager lock"));
        for (session, entries) in sessions {
            self.retire(&session, entries.into_values().collect()).await;
        }
        let pending: Vec<_> = self
            .retired
            .lock()
            .expect("MCP retired lock")
            .keys()
            .cloned()
            .collect();
        for session in pending {
            self.retire(&session, Vec::new()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Response, Server};
    use serde_json::json;

    #[rstest::rstest]
    #[case(false)]
    #[case(true)]
    #[tokio::test]
    async fn stateful_http_closes_sessions_and_never_retries_an_expired_tool_post(
        #[case] expired: bool,
    ) {
        let methods = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = methods.clone();
        let server = Server::respond(6, move |request| {
            let method = request.body["method"]
                .as_str()
                .unwrap_or_else(|| request.head.split_whitespace().next().unwrap());
            observed.lock().unwrap().push(method.to_owned());
            let mut response = match method {
                "initialize" => {
                    let mut response = Response::json(
                        json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"protocolVersion":request.body["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}),
                    );
                    response
                        .headers
                        .push(("mcp-session-id".into(), "fixture-session".into()));
                    response
                }
                "notifications/initialized" | "DELETE" => {
                    let mut response = Response::json(json!({}));
                    response.status = 202;
                    response
                }
                "GET" => {
                    let mut response = Response::json(json!({}));
                    response.status = 405;
                    response
                }
                "tools/list" => Response::json(
                    json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"tools":[]}}),
                ),
                "tools/call" => Response::json(
                    json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"content":[]}}),
                ),
                _ => panic!("unexpected MCP method"),
            };
            if method == "tools/call" && expired {
                response.status = 404;
            }
            response
        });
        let entry = Entry::new(Transport::Http {
            url: server.url.clone(),
            headers: BTreeMap::new(),
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            entry.connect().await.unwrap();
            entry.request("tools/list", json!({})).await.unwrap();
            assert_eq!(
                entry
                    .request("tools/call", json!({"name":"one","arguments":{}}))
                    .await
                    .is_err(),
                expired
            );
            entry.close().await;
        })
        .await
        .unwrap();
        let methods = methods.lock().unwrap();
        assert_eq!(methods.iter().filter(|m| *m == "initialize").count(), 1);
        assert_eq!(methods.iter().filter(|m| *m == "tools/call").count(), 1);
        assert!(
            methods.contains(&"DELETE".into()),
            "close must await session deletion"
        );
    }

    #[rstest::rstest]
    #[case(false)]
    #[case(true)]
    #[tokio::test]
    async fn direct_http_preserves_raw_results_and_errors_for_json_and_sse(#[case] sse: bool) {
        let server = Server::respond(5, move |request| {
            let id = &request.body["id"];
            let envelope = match request.body["method"].as_str().unwrap() {
                "initialize" => {
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":request.body["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}})
                }
                "notifications/initialized" => {
                    let mut response = Response::json(json!({}));
                    response.status = 202;
                    return response;
                }
                "tools/list" => json!({"jsonrpc":"2.0","id":id,"result":{"tools":[]}}),
                "tools/call" if request.body["params"]["name"] == "error" => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"failure","data":null,"extension":[1,2]},"_meta":{"carrier":true}})
                }
                "tools/call" => {
                    json!({"jsonrpc":"2.0","id":id,"result":{"content":[],"structuredContent":{"nested":[1,true,null]},"extension":42,"_meta":{"private":true}},"_meta":{"carrier":true}})
                }
                _ => panic!("unexpected MCP method"),
            };
            if sse {
                Response::sse(format!("event: message\ndata: {envelope}\n\n"))
            } else {
                Response::json(envelope)
            }
        });
        let entry = Entry::new(Transport::Http {
            url: server.url.clone(),
            headers: BTreeMap::new(),
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            entry.connect().await.unwrap();
            assert_eq!(
                entry.request("tools/list", json!({})).await.unwrap()["result"]["tools"],
                json!([])
            );
            let result = entry
                .request("tools/call", json!({"name":"ok","arguments":{}}))
                .await
                .unwrap();
            assert_eq!(result["result"]["extension"], 42);
            assert_eq!(result["_meta"]["carrier"], true);
            let result = entry
                .request("tools/call", json!({"name":"error","arguments":{}}))
                .await
                .unwrap();
            assert_eq!(result["error"]["extension"], json!([1, 2]));
            assert!(result["error"].as_object().unwrap().contains_key("data"));
            entry.close().await;
        })
        .await
        .unwrap();
    }
}
