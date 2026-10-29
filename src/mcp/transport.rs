//! Raw ingress preservation around the SDK's typed transport boundary.

use futures::{StreamExt, stream::BoxStream};
use rmcp::{
    RoleClient,
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::{
        Transport,
        streamable_http_client::{
            StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

type Outgoing = (
    ClientJsonRpcMessage,
    tokio::sync::oneshot::Sender<io::Result<()>>,
);

/// The manager retains this join barrier even if SDK initialization is dropped.
pub(super) struct Owner {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Owner {
    pub async fn close(&mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.as_mut() {
            let _ = task.await;
        }
        self.task.take();
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub(super) struct Proxy {
    send: tokio::sync::mpsc::Sender<Outgoing>,
    receive: tokio::sync::mpsc::Receiver<ServerJsonRpcMessage>,
    stop: CancellationToken,
    done: CancellationToken,
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub(super) fn owned<T: Transport<RoleClient> + 'static>(mut transport: T) -> (Proxy, Owner) {
    let (send, mut outgoing) = tokio::sync::mpsc::channel::<Outgoing>(1);
    let (incoming, receive) = tokio::sync::mpsc::channel(1);
    let stop = CancellationToken::new();
    let done = CancellationToken::new();
    let stopping = stop.clone();
    let finished = done.clone();
    let task = tokio::spawn(async move {
        // A panic must also release SDK close waiters; the retained JoinHandle
        // still reports failure to the external cleanup owner.
        let _finished = finished.drop_guard();
        loop {
            tokio::select! {
                biased;
                _ = stopping.cancelled() => break,
                outgoing = outgoing.recv() => {
                    let Some((message, response)) = outgoing else { break; };
                    let result = tokio::select! {
                        result = transport.send(message) => result.map_err(|_| invalid()),
                        _ = stopping.cancelled() => break,
                    };
                    let failed = result.is_err();
                    let _ = response.send(result);
                    if failed { break; }
                }
                message = transport.receive() => {
                    let Some(message) = message else { break; };
                    tokio::select! {
                        result = incoming.send(message) => if result.is_err() { break; },
                        _ = stopping.cancelled() => break,
                    }
                }
            }
        }
        let _ = transport.close().await;
    });
    (
        Proxy {
            send,
            receive,
            stop: stop.clone(),
            done,
        },
        Owner {
            stop,
            task: Some(task),
        },
    )
}
impl Transport<RoleClient> for Proxy {
    type Error = io::Error;
    fn send(
        &mut self,
        message: ClientJsonRpcMessage,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let send = self.send.clone();
        async move {
            let (tx, rx) = tokio::sync::oneshot::channel();
            send.send((message, tx)).await.map_err(|_| invalid())?;
            rx.await.map_err(|_| invalid())?
        }
    }
    async fn receive(&mut self) -> Option<ServerJsonRpcMessage> {
        self.receive.recv().await
    }
    async fn close(&mut self) -> io::Result<()> {
        self.stop.cancel();
        self.done.cancelled().await;
        Ok(())
    }
}

pub(super) fn invalid() -> io::Error {
    io::Error::other("invalid or oversized MCP message; check the server implementation")
}

#[derive(Default)]
struct State {
    id: Option<Value>,
    envelope: Option<Value>,
}

/// Each connection serializes requests, so only one bounded envelope is retained.
#[derive(Clone, Default)]
pub(super) struct Capture(Arc<Mutex<State>>);
impl Capture {
    pub fn outgoing(&self, message: &ClientJsonRpcMessage) {
        let value = serde_json::to_value(message).expect("SDK messages are JSON");
        if matches!(value["method"].as_str(), Some("tools/list" | "tools/call")) {
            *self.0.lock().expect("capture lock") = State {
                id: Some(value["id"].clone()),
                envelope: None,
            };
        }
    }
    pub fn incoming(&self, bytes: &[u8]) -> io::Result<ServerJsonRpcMessage> {
        if bytes.len() > super::MAX_BYTES {
            return Err(invalid());
        }
        let mut value: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        let mut state = self.0.lock().expect("capture lock");
        if state
            .id
            .as_ref()
            .is_some_and(|id| value.get("id") == Some(id))
        {
            if value["jsonrpc"] != "2.0"
                || !value.is_object()
                || value.get("result").is_some() == value.get("error").is_some()
                || state.envelope.is_some()
            {
                return Err(invalid());
            }
            let mut envelope = value.clone();
            envelope.as_object_mut().unwrap().remove("jsonrpc");
            envelope.as_object_mut().unwrap().remove("id");
            state.envelope = Some(envelope);
            // SDK dispatch only needs correlation. Keep the original envelope outside
            // its typed union, which otherwise loses extensions and explicit nulls.
            value = json!({"jsonrpc":"2.0", "id":value["id"], "result":{}});
        }
        serde_json::from_value(value).map_err(|_| invalid())
    }
    pub fn take(&self) -> Option<Value> {
        let mut state = self.0.lock().expect("capture lock");
        let envelope = state.envelope.take();
        state.id = None;
        envelope
    }
}

pub(super) struct Stdio {
    pub reader: BufReader<tokio::process::ChildStdout>,
    pub writer: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    pub capture: Capture,
    pub buffer: Vec<u8>,
}
impl Transport<RoleClient> for Stdio {
    type Error = io::Error;
    fn send(
        &mut self,
        item: ClientJsonRpcMessage,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        self.capture.outgoing(&item);
        let mut bytes = serde_json::to_vec(&item).expect("SDK message is JSON");
        bytes.push(b'\n');
        let writer = self.writer.clone();
        async move {
            let mut writer = writer.lock().await;
            writer.write_all(&bytes).await?;
            writer.flush().await
        }
    }
    async fn receive(&mut self) -> Option<ServerJsonRpcMessage> {
        // fill_buf/consume bounds allocation even when the peer never sends a newline.
        loop {
            let chunk = self.reader.fill_buf().await.ok()?;
            if chunk.is_empty() {
                return None;
            }
            let end = chunk.iter().position(|b| *b == b'\n').map(|p| p + 1);
            let n = end.unwrap_or(chunk.len());
            if self.buffer.len() + n > super::MAX_BYTES {
                return None;
            }
            self.buffer.extend_from_slice(&chunk[..n]);
            self.reader.consume(n);
            if end.is_some() {
                return self
                    .capture
                    .incoming(&std::mem::take(&mut self.buffer))
                    .ok();
            }
        }
    }
    async fn close(&mut self) -> io::Result<()> {
        self.writer.lock().await.shutdown().await
    }
}

#[derive(Clone)]
pub(super) struct Http {
    pub client: reqwest::Client,
    pub capture: Capture,
}
type HttpError = StreamableHttpError<io::Error>;
fn http_error(_: impl std::fmt::Debug) -> HttpError {
    StreamableHttpError::Client(invalid())
}

impl Http {
    pub fn new(capture: Capture) -> io::Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| invalid())?;
        Ok(Self { client, capture })
    }
    fn request(
        &self,
        method: reqwest::Method,
        uri: &str,
        session: Option<&str>,
        auth: Option<String>,
        headers: HashMap<http::HeaderName, http::HeaderValue>,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .request(method, uri)
            .header("accept", "application/json, text/event-stream");
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        if let Some(auth) = auth {
            request = request.bearer_auth(auth);
        }
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request
    }
    fn sse(
        &self,
        response: reqwest::Response,
    ) -> BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>> {
        let capture = self.capture.clone();
        let mut retained = 0;
        let mut line = 0;
        let mut cr = false;
        let bytes = response.bytes_stream().map(move |chunk| {
            let chunk = chunk.map_err(|_| invalid())?;
            for byte in &chunk {
                if cr && *byte == b'\n' {
                    cr = false;
                    continue;
                }
                cr = *byte == b'\r';
                if matches!(byte, b'\r' | b'\n') {
                    if line == 0 {
                        retained = 0;
                    }
                    line = 0;
                } else {
                    line += 1;
                }
                retained += 1;
                if retained > super::MAX_BYTES {
                    return Err(invalid());
                }
            }
            Ok(chunk)
        });
        sse_stream::SseStream::from_bytes_stream(bytes)
            .map(move |event| {
                let mut event = event?;
                if let Some(data) = &event.data
                    && !data.trim().is_empty()
                {
                    let message = capture
                        .incoming(data.as_bytes())
                        .map_err(|e| sse_stream::Error::Body(Box::new(e)))?;
                    event.data =
                        Some(serde_json::to_string(&message).expect("SDK message is JSON"));
                }
                Ok(event)
            })
            .boxed()
    }
}

impl StreamableHttpClient for Http {
    type Error = io::Error;
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<http::HeaderName, http::HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        self.capture.outgoing(&message);
        let response = self
            .request(
                reqwest::Method::POST,
                &uri,
                session.as_deref(),
                auth,
                headers,
            )
            .json(&message)
            .send()
            .await
            .map_err(http_error)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND && session.is_some() {
            return Err(StreamableHttpError::SessionExpired);
        }
        if !response.status().is_success() {
            return Err(http_error(()));
        }
        if matches!(response.status().as_u16(), 202 | 204) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let session = response
            .headers()
            .get("mcp-session-id")
            .map(|v| v.to_str().map(str::to_owned))
            .transpose()
            .map_err(http_error)?;
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_owned();
        match mime.as_str() {
            "text/event-stream" => Ok(StreamableHttpPostResponse::Sse(self.sse(response), session)),
            "application/json" => {
                let mut bytes = Vec::new();
                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(http_error)?;
                    if bytes.len() + chunk.len() > super::MAX_BYTES {
                        return Err(http_error(()));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(StreamableHttpPostResponse::Json(
                    self.capture.incoming(&bytes).map_err(http_error)?,
                    session,
                ))
            }
            _ => Err(http_error(())),
        }
    }
    async fn get_stream(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last: Option<String>,
        auth: Option<String>,
        headers: HashMap<http::HeaderName, http::HeaderValue>,
    ) -> Result<BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>, HttpError> {
        let mut request = self.request(
            reqwest::Method::GET,
            &uri,
            session.as_deref(),
            auth,
            headers,
        );
        if let Some(last) = last {
            request = request.header("last-event-id", last);
        }
        let response = request.send().await.map_err(http_error)?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if !response.status().is_success() {
            return Err(http_error(()));
        }
        Ok(self.sse(response))
    }
    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth: Option<String>,
        headers: HashMap<http::HeaderName, http::HeaderValue>,
    ) -> Result<(), HttpError> {
        let response = self
            .request(reqwest::Method::DELETE, &uri, Some(&session), auth, headers)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(http_error)?;
        if response.status().is_success()
            || response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED
        {
            Ok(())
        } else {
            Err(http_error(()))
        }
    }
}
