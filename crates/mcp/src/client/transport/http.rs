use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, Notify, mpsc},
    task::{AbortHandle, JoinHandle},
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::{
    Error, Result,
    oauth::{McpAuthorizationError, McpBearerChallenge},
    protocol::{
        JsonRpcId, JsonRpcInbound, MCP_PROTOCOL_VERSION, json_rpc_result, parse_json_rpc_message,
        parse_json_rpc_str,
    },
    trace::{request_id, request_method},
};

use super::{
    InboundSender, McpTransportError, McpTransportErrorKind, TransportInbound,
    sse::{SseDecoder, SseFrame},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_BODY_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug)]
struct PendingHttpSession {
    session_id: String,
    protocol_version: Option<String>,
}

#[derive(Clone, Debug)]
struct ActiveHttpSession {
    session_id: String,
    protocol_version: String,
    last_event_id: Option<String>,
    generation: u64,
}

#[derive(Debug, Default)]
struct HttpSessionState {
    pending: Option<PendingHttpSession>,
    active: Option<ActiveHttpSession>,
    stateless_protocol_version: Option<String>,
    initialize_message: Option<Value>,
    next_generation: u64,
    #[cfg(test)]
    delivery_pause: Option<Arc<(Notify, Notify)>>,
}

#[derive(Clone, Debug)]
enum ShutdownFailure {
    Authorization(Box<McpAuthorizationError>),
    Protocol(String),
}

impl ShutdownFailure {
    fn error(&self) -> Error {
        match self {
            Self::Authorization(error) => Error::Authorization(error.clone()),
            Self::Protocol(message) => Error::protocol(message.clone()),
        }
    }
}

/// Streamable HTTP client transport.
pub struct StreamableHttpTransport {
    endpoint: String,
    bearer_token: Option<String>,
    client: reqwest::Client,
    state: Arc<Mutex<HttpSessionState>>,
    inbound: InboundSender,
    polling_generation: Arc<AtomicU64>,
    cancellation: CancellationToken,
    reader_tasks: mpsc::UnboundedSender<(u64, JoinHandle<()>)>,
    reader_supervisor: Mutex<Option<JoinHandle<()>>>,
    tracked_readers: Arc<AtomicUsize>,
    reader_aborts: Arc<StdMutex<HashMap<u64, AbortHandle>>>,
    readers_accepting: Arc<AtomicBool>,
    next_reader_id: AtomicU64,
    retry: Arc<Mutex<Duration>>,
    reinitialize: Mutex<()>,
    shutting_down: AtomicBool,
    shutdown_gate: Mutex<()>,
    shutdown_finished: AtomicBool,
    shutdown_failure: Mutex<Option<ShutdownFailure>>,
    active_sends: AtomicUsize,
    sends_finished: Notify,
}

impl std::fmt::Debug for StreamableHttpTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamableHttpTransport")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl StreamableHttpTransport {
    /// Create a Streamable HTTP transport.
    #[must_use]
    pub fn new(endpoint: String, bearer_token: Option<String>, inbound: InboundSender) -> Self {
        let cancellation = CancellationToken::new();
        let tracked_readers = Arc::new(AtomicUsize::new(0));
        let reader_aborts = Arc::new(StdMutex::new(HashMap::new()));
        let readers_accepting = Arc::new(AtomicBool::new(true));
        let (reader_tasks, reader_rx) = mpsc::unbounded_channel();
        let reader_supervisor = tokio::spawn(supervise_readers(
            reader_rx,
            cancellation.clone(),
            Arc::clone(&tracked_readers),
            Arc::clone(&reader_aborts),
            Arc::clone(&readers_accepting),
        ));
        Self {
            endpoint,
            bearer_token,
            client: reqwest::Client::new(),
            state: Arc::new(Mutex::new(HttpSessionState::default())),
            inbound,
            polling_generation: Arc::new(AtomicU64::new(0)),
            cancellation,
            reader_tasks,
            reader_supervisor: Mutex::new(Some(reader_supervisor)),
            tracked_readers,
            reader_aborts,
            readers_accepting,
            next_reader_id: AtomicU64::new(1),
            retry: Arc::new(Mutex::new(Duration::from_secs(1))),
            reinitialize: Mutex::new(()),
            shutting_down: AtomicBool::new(false),
            shutdown_gate: Mutex::new(()),
            shutdown_finished: AtomicBool::new(false),
            shutdown_failure: Mutex::new(None),
            active_sends: AtomicUsize::new(0),
            sends_finished: Notify::new(),
        }
    }

    /// Send one message over Streamable HTTP.
    // Request cancellation, session recovery, response validation, and reader handoff are one ordered operation.
    #[allow(clippy::too_many_lines)]
    pub async fn send(
        &self,
        message: Value,
        initialized: bool,
        request_cancellation: Option<&CancellationToken>,
    ) -> Result<()> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(Error::protocol("MCP HTTP transport is shutting down"));
        }
        self.active_sends.fetch_add(1, Ordering::SeqCst);
        let _send = SendGuard(self);
        let method = request_method(&message).unwrap_or("").to_owned();
        if method == "initialize" {
            self.state.lock().await.initialize_message = Some(message.clone());
        } else if initialized {
            let state = self.state.lock().await;
            let needs_session =
                state.active.is_none() && state.stateless_protocol_version.is_none();
            drop(state);
            if needs_session {
                self.ensure_reinitialized(request_cancellation).await?;
                if request_is_cancelled(request_cancellation) {
                    return Err(Error::protocol("MCP request canceled"));
                }
            }
        }

        let state = self.state.lock().await;
        let session = state.active.clone();
        let stateless_protocol_version = state.stateless_protocol_version.clone();
        drop(state);
        let mut builder = self
            .request(reqwest::Method::POST)
            .await?
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .json(&message);
        if let Some(session) = &session {
            builder = builder
                .header("mcp-session-id", &session.session_id)
                .header("mcp-protocol-version", &session.protocol_version);
        } else if let Some(protocol_version) = &stateless_protocol_version {
            builder = builder.header("mcp-protocol-version", protocol_version);
        }
        let id = request_id(&message);
        let span = tracing::info_span!(
            "http.client.request",
            http_request_method = "POST",
            server_address = %self.endpoint,
            client = "mcp",
            mcp_session = session.is_some(),
            http_response_status_code = tracing::field::Empty,
            rpc.system = "jsonrpc",
            rpc.jsonrpc.version = "2.0",
            rpc.method = %method,
            mcp.method = %method,
            mcp.request_id = id.as_deref().unwrap_or(""),
            mcp.transport = "streamable_http",
        );
        let send = builder.send().instrument(span.clone());
        let response = tokio::select! {
            () = request_cancelled(request_cancellation) => {
                return Err(Error::protocol("MCP request canceled"));
            }
            () = self.cancellation.cancelled() => {
                return Err(Error::protocol("MCP HTTP transport is shutting down"));
            }
            response = async {
                if matches!(method.as_str(), "initialize" | "notifications/initialized") {
                    tokio::time::timeout(HANDSHAKE_TIMEOUT, send)
                        .await
                        .map_err(|_| Error::protocol("MCP HTTP handshake headers timed out"))?
                        .map_err(Error::from)
                } else {
                    send.await.map_err(Error::from)
                }
            } => response?,
        };
        span.record("http_response_status_code", response.status().as_u16());
        if response.status() == reqwest::StatusCode::NOT_FOUND
            && let Some(session) = &session
        {
            self.expire_active_session(session).await;
            return Err(Error::protocol("MCP HTTP session expired"));
        }
        if method == "initialize" {
            self.capture_response_session(&method, response.headers())
                .await?;
        }
        check_authorization(&response)?;
        if !response.status().is_success() {
            return Err(Error::protocol(format!(
                "MCP HTTP request failed with status {}",
                response.status()
            )));
        }
        if method != "initialize" {
            self.capture_response_session(&method, response.headers())
                .await?;
        }
        if response.status() == reqwest::StatusCode::ACCEPTED
            || response.content_length() == Some(0)
        {
            if initialized && method == "notifications/initialized" {
                tokio::select! {
                    () = request_cancelled(request_cancellation) => {
                        return Err(Error::protocol("MCP request canceled"));
                    }
                    () = self.cancellation.cancelled() => {
                        return Err(Error::protocol("MCP HTTP transport is shutting down"));
                    }
                    value = collect_body_bounded(response, HANDSHAKE_TIMEOUT) => {
                        value?;
                    }
                }
                self.start_polling().await;
            }
            return Ok(());
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if content_type.contains("text/event-stream") {
            self.spawn_reader(response).await;
        } else {
            let value = tokio::select! {
                () = request_cancelled(request_cancellation) => {
                    return Err(Error::protocol("MCP request canceled"));
                }
                () = self.cancellation.cancelled() => {
                    return Err(Error::protocol("MCP HTTP transport is shutting down"));
                }
                value = collect_body_bounded(response, HANDSHAKE_TIMEOUT) => {
                    serde_json::from_slice::<Value>(&value?)?
                },
            };
            self.inbound
                .send(TransportInbound::Message(parse_json_rpc_message(value)?))
                .map_err(|_| Error::protocol("MCP connection closed"))?;
        }
        if initialized && method == "notifications/initialized" {
            self.start_polling().await;
        }
        Ok(())
    }

    /// Promote the server-issued pending session after validating initialize.
    pub async fn mark_initialized(&self, protocol_version: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        let Some(pending) = state.pending.take() else {
            state.stateless_protocol_version = Some(protocol_version.to_owned());
            return Ok(());
        };
        if pending
            .protocol_version
            .as_deref()
            .is_some_and(|version| version != protocol_version)
        {
            state.pending = Some(pending);
            return Err(Error::protocol(
                "MCP initialize response protocol header does not match its result",
            ));
        }
        activate_session(&mut state, pending.session_id, protocol_version.to_owned());
        state.stateless_protocol_version = None;
        drop(state);
        Ok(())
    }

    async fn capture_response_session(
        &self,
        method: &str,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<()> {
        let response_session = optional_response_header(headers, "mcp-session-id")?;
        let mut state = self.state.lock().await;
        if let Some(active) = &state.active {
            if response_session
                .as_deref()
                .is_some_and(|session| session != active.session_id)
            {
                return Err(Error::protocol(
                    "MCP server attempted to replace an active HTTP session",
                ));
            }
        } else if method == "initialize"
            && let Some(session_id) = response_session
        {
            state.pending = Some(PendingHttpSession {
                session_id,
                protocol_version: None,
            });
        }
        drop(state);
        let response_version = optional_response_header(headers, "mcp-protocol-version")?;
        if method == "initialize"
            && let Some(pending) = &mut self.state.lock().await.pending
        {
            pending.protocol_version = response_version;
        }
        Ok(())
    }

    async fn ensure_reinitialized(
        &self,
        request_cancellation: Option<&CancellationToken>,
    ) -> Result<()> {
        let _guard = self.reinitialize.lock().await;
        if self.state.lock().await.active.is_some() {
            return Ok(());
        }
        let abandoned = self.state.lock().await.pending.take();
        if let Some(abandoned) = abandoned {
            drop(
                self.delete_session(&abandoned.session_id, abandoned.protocol_version.as_deref())
                    .await,
            );
        }
        let initialize = self
            .state
            .lock()
            .await
            .initialize_message
            .clone()
            .ok_or_else(|| Error::protocol("MCP HTTP session cannot be reinitialized"))?;
        let result = self
            .reinitialize_handshake(initialize, request_cancellation)
            .await;
        if result.is_err() {
            self.cleanup_pending_session().await;
        }
        result
    }

    // The two-request MCP reinitialization handshake must preserve its ordered cleanup points.
    #[allow(clippy::too_many_lines)]
    async fn reinitialize_handshake(
        &self,
        initialize: Value,
        request_cancellation: Option<&CancellationToken>,
    ) -> Result<()> {
        let expected_id = initialize
            .get("id")
            .cloned()
            .ok_or_else(|| Error::protocol("MCP initialize request omitted id"))
            .and_then(|id| serde_json::from_value::<JsonRpcId>(id).map_err(Error::from))?;
        let initialize_request = self
            .request(reqwest::Method::POST)
            .await?
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .json(&initialize)
            .send();
        let response = tokio::select! {
            () = request_cancelled(request_cancellation) => {
                return Err(Error::protocol("MCP request canceled"));
            }
            () = self.cancellation.cancelled() => {
                return Err(Error::protocol("MCP HTTP transport is shutting down"));
            }
            response = tokio::time::timeout(HANDSHAKE_TIMEOUT, initialize_request) => {
                response
                    .map_err(|_| Error::protocol("MCP reinitialize headers timed out"))??
            },
        };
        let pending = self.capture_issued_session(response.headers()).await?;
        check_authorization(&response)?;
        if !response.status().is_success() {
            return Err(Error::protocol(format!(
                "MCP HTTP reinitialization failed with status {}",
                response.status()
            )));
        }
        let parsed = tokio::select! {
            () = request_cancelled(request_cancellation) => {
                return Err(Error::protocol("MCP request canceled"));
            }
            () = self.cancellation.cancelled() => {
                return Err(Error::protocol("MCP HTTP transport is shutting down"));
            }
            parsed = tokio::time::timeout(
                HANDSHAKE_TIMEOUT,
                parse_initialize_response(response, &expected_id),
            ) => parsed,
        };
        let result = match parsed {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(Error::protocol("MCP reinitialize response timed out")),
        };
        let Some(protocol_version) = result.get("protocolVersion").and_then(Value::as_str) else {
            return Err(Error::protocol(
                "MCP reinitialize result omitted protocolVersion",
            ));
        };
        if protocol_version != MCP_PROTOCOL_VERSION {
            return Err(Error::protocol(format!(
                "unsupported MCP protocol version `{protocol_version}`"
            )));
        }
        if let Some(pending) = &pending
            && pending
                .protocol_version
                .as_deref()
                .is_some_and(|version| version != protocol_version)
        {
            return Err(Error::protocol(
                "MCP reinitialize response protocol header does not match its result",
            ));
        }
        let mut initialized = self
            .request(reqwest::Method::POST)
            .await?
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", protocol_version)
            .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        if let Some(pending) = &pending {
            initialized = initialized.header("mcp-session-id", &pending.session_id);
        }
        let response = tokio::select! {
            () = request_cancelled(request_cancellation) => {
                return Err(Error::protocol("MCP request canceled"));
            }
            () = self.cancellation.cancelled() => {
                return Err(Error::protocol("MCP HTTP transport is shutting down"));
            }
            response = tokio::time::timeout(HANDSHAKE_TIMEOUT, initialized.send()) => {
                response
                    .map_err(|_| Error::protocol("MCP initialized notification headers timed out"))??
            },
        };
        check_authorization(&response)?;
        if !response.status().is_success() {
            return Err(Error::protocol(format!(
                "MCP HTTP reinitialization notification failed with status {}",
                response.status()
            )));
        }
        collect_body_bounded(response, HANDSHAKE_TIMEOUT).await?;
        let mut state = self.state.lock().await;
        state.pending = None;
        if let Some(pending) = pending {
            activate_session(&mut state, pending.session_id, protocol_version.to_owned());
        } else {
            state.active = None;
            state.stateless_protocol_version = Some(protocol_version.to_owned());
        }
        drop(state);
        self.start_polling().await;
        Ok(())
    }

    async fn cleanup_pending_session(&self) {
        let pending = self.state.lock().await.pending.take();
        if let Some(pending) = pending {
            drop(
                self.delete_session(&pending.session_id, pending.protocol_version.as_deref())
                    .await,
            );
        }
    }

    async fn capture_issued_session(
        &self,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<Option<PendingHttpSession>> {
        let session_id = optional_response_header(headers, "mcp-session-id")?;
        if let Some(session_id) = &session_id {
            self.state.lock().await.pending = Some(PendingHttpSession {
                session_id: session_id.clone(),
                protocol_version: None,
            });
        }
        let protocol_version = optional_response_header(headers, "mcp-protocol-version")?;
        let pending = session_id.map(|session_id| PendingHttpSession {
            session_id,
            protocol_version,
        });
        self.state.lock().await.pending = pending.clone();
        Ok(pending)
    }

    async fn spawn_reader(&self, response: reqwest::Response) {
        let state = Arc::clone(&self.state);
        let expected_session = state.lock().await.active.clone();
        let inbound = self.inbound.clone();
        let cancellation = self.cancellation.clone();
        let retry = Arc::clone(&self.retry);
        let task = tokio::spawn(async move {
            if let Err(error) = read_sse(
                response,
                SseReadContext {
                    state,
                    expected_session,
                    inbound: inbound.clone(),
                    cancellation,
                    retry,
                },
            )
            .await
            {
                drop(
                    inbound.send(TransportInbound::TransportFailed(classify_sse_error(
                        &error, false,
                    ))),
                );
            }
        });
        self.track_reader(task).await;
    }

    async fn start_polling(&self) {
        let Some(generation) = self
            .state
            .lock()
            .await
            .active
            .as_ref()
            .map(|active| active.generation)
        else {
            return;
        };
        if self.polling_generation.swap(generation, Ordering::SeqCst) == generation {
            return;
        }
        let task = tokio::spawn(poll_loop(PollContext {
            endpoint: self.endpoint.clone(),
            bearer_token: self.bearer_token.clone(),
            client: self.client.clone(),
            state: Arc::clone(&self.state),
            inbound: self.inbound.clone(),
            cancellation: self.cancellation.clone(),
            retry: Arc::clone(&self.retry),
            polling_generation: Arc::clone(&self.polling_generation),
            generation,
        }));
        self.track_reader(task).await;
    }

    async fn track_reader(&self, task: JoinHandle<()>) {
        if !self.readers_accepting.load(Ordering::SeqCst) {
            task.abort();
            drop(task.await);
            return;
        }
        let id = self.next_reader_id.fetch_add(1, Ordering::SeqCst);
        let abort = task.abort_handle();
        {
            let mut aborts = self.reader_aborts.lock().expect("reader abort registry");
            if !self.readers_accepting.load(Ordering::SeqCst) {
                drop(aborts);
                task.abort();
                drop(task.await);
                return;
            }
            aborts.insert(id, abort);
            self.tracked_readers.fetch_add(1, Ordering::SeqCst);
            drop(aborts);
        }
        if let Err(error) = self.reader_tasks.send((id, task)) {
            let (_, task) = error.0;
            self.reader_aborts
                .lock()
                .expect("reader abort registry")
                .remove(&id);
            task.abort();
            drop(task.await);
            self.tracked_readers.fetch_sub(1, Ordering::SeqCst);
        }
    }

    async fn expire_active_session(&self, expected: &ActiveHttpSession) -> bool {
        clear_active_if_matches(&self.state, expected).await
    }

    async fn request(&self, method: reqwest::Method) -> Result<reqwest::RequestBuilder> {
        let request = self.client.request(method, &self.endpoint);
        Ok(match &self.bearer_token {
            Some(token) => request.bearer_auth(token),
            None => request,
        })
    }

    /// Cancel readers, join them, and terminate the durable session.
    pub async fn shutdown(&self) -> Result<()> {
        let _guard = self.shutdown_gate.lock().await;
        if self.shutdown_finished.load(Ordering::SeqCst) {
            let failure = self.shutdown_failure.lock().await.clone();
            return failure.map_or_else(|| Ok(()), |failure| Err(failure.error()));
        }
        self.shutting_down.store(true, Ordering::SeqCst);
        self.cancellation.cancel();
        let wait_for_sends = async {
            loop {
                let finished = self.sends_finished.notified();
                tokio::pin!(finished);
                finished.as_mut().enable();
                if self.active_sends.load(Ordering::SeqCst) == 0 {
                    break;
                }
                finished.await;
            }
        };
        drop(tokio::time::timeout(Duration::from_secs(5), wait_for_sends).await);
        if let Some(mut supervisor) = self.reader_supervisor.lock().await.take()
            && tokio::time::timeout(Duration::from_secs(5), &mut supervisor)
                .await
                .is_err()
        {
            abort_registered_readers(&self.reader_aborts);
            drop(supervisor.await);
        }
        let session = {
            let mut state = self.state.lock().await;
            state
                .active
                .take()
                .map(|active| (active.session_id, Some(active.protocol_version)))
                .or_else(|| {
                    state
                        .pending
                        .take()
                        .map(|pending| (pending.session_id, pending.protocol_version))
                })
        };
        let failure = if let Some((session_id, protocol_version)) = session {
            self.delete_session(&session_id, protocol_version.as_deref())
                .await
                .err()
        } else {
            None
        };
        *self.shutdown_failure.lock().await = failure.clone();
        self.shutdown_finished.store(true, Ordering::SeqCst);
        failure.map_or_else(|| Ok(()), |failure| Err(failure.error()))
    }

    async fn delete_session(
        &self,
        session_id: &str,
        protocol_version: Option<&str>,
    ) -> std::result::Result<(), ShutdownFailure> {
        let mut request = self
            .request(reqwest::Method::DELETE)
            .await
            .map_err(|error| ShutdownFailure::Protocol(error.to_string()))?
            .header("mcp-session-id", session_id);
        if let Some(protocol_version) = protocol_version {
            request = request.header("mcp-protocol-version", protocol_version);
        }
        let response = tokio::time::timeout(Duration::from_secs(5), request.send())
            .await
            .map_err(|_| ShutdownFailure::Protocol("MCP HTTP DELETE timed out".to_owned()))?
            .map_err(|error| {
                ShutdownFailure::Protocol(format!("MCP HTTP DELETE failed: {error}"))
            })?;
        if matches!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(ShutdownFailure::Authorization(Box::new(
                authorization_error(&response),
            )));
        }
        if response.status().is_success()
            || matches!(
                response.status(),
                reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
            )
        {
            Ok(())
        } else {
            Err(ShutdownFailure::Protocol(format!(
                "MCP HTTP DELETE failed with status {}",
                response.status()
            )))
        }
    }
}

struct SendGuard<'a>(&'a StreamableHttpTransport);

impl Drop for SendGuard<'_> {
    fn drop(&mut self) {
        if self.0.active_sends.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.sends_finished.notify_waiters();
        }
    }
}

async fn supervise_readers(
    mut reader_rx: mpsc::UnboundedReceiver<(u64, JoinHandle<()>)>,
    cancellation: CancellationToken,
    tracked_readers: Arc<AtomicUsize>,
    reader_aborts: Arc<StdMutex<HashMap<u64, AbortHandle>>>,
    readers_accepting: Arc<AtomicBool>,
) {
    let mut readers = FuturesUnordered::new();
    let mut accepting = true;
    loop {
        if !accepting && readers.is_empty() {
            return;
        }
        tokio::select! {
            biased;
            () = cancellation.cancelled(), if accepting => {
                readers_accepting.store(false, Ordering::SeqCst);
                reader_rx.close();
                abort_registered_readers(&reader_aborts);
                while let Ok((id, reader)) = reader_rx.try_recv() {
                    readers.push(join_reader(id, reader));
                }
                accepting = false;
            }
            reader = reader_rx.recv(), if accepting => {
                if let Some((id, reader)) = reader { readers.push(join_reader(id, reader)) } else {
                    readers_accepting.store(false, Ordering::SeqCst);
                    accepting = false;
                    cancellation.cancel();
                    abort_registered_readers(&reader_aborts);
                }
            }
            result = readers.next(), if !readers.is_empty() => {
                if let Some((id, _)) = result {
                    reader_aborts.lock().expect("reader abort registry").remove(&id);
                    tracked_readers.fetch_sub(1, Ordering::SeqCst);
                }
            }
        }
    }
}

async fn join_reader(
    id: u64,
    reader: JoinHandle<()>,
) -> (u64, std::result::Result<(), tokio::task::JoinError>) {
    (id, reader.await)
}

fn abort_registered_readers(reader_aborts: &StdMutex<HashMap<u64, AbortHandle>>) {
    for abort in reader_aborts
        .lock()
        .expect("reader abort registry")
        .values()
    {
        abort.abort();
    }
}

fn activate_session(state: &mut HttpSessionState, session_id: String, protocol_version: String) {
    state.next_generation = state.next_generation.wrapping_add(1).max(1);
    state.active = Some(ActiveHttpSession {
        session_id,
        protocol_version,
        last_event_id: None,
        generation: state.next_generation,
    });
    state.stateless_protocol_version = None;
}

async fn clear_active_if_matches(
    state: &Mutex<HttpSessionState>,
    expected: &ActiveHttpSession,
) -> bool {
    let mut state = state.lock().await;
    let matches = state.active.as_ref().is_some_and(|active| {
        active.generation == expected.generation && active.session_id == expected.session_id
    });
    if matches {
        state.active = None;
        state.stateless_protocol_version = None;
    }
    matches
}

struct PollContext {
    endpoint: String,
    bearer_token: Option<String>,
    client: reqwest::Client,
    state: Arc<Mutex<HttpSessionState>>,
    inbound: InboundSender,
    cancellation: CancellationToken,
    retry: Arc<Mutex<Duration>>,
    polling_generation: Arc<AtomicU64>,
    generation: u64,
}

// Polling combines retry, session replacement, authorization, and SSE reader state.
#[allow(clippy::too_many_lines)]
async fn poll_loop(context: PollContext) {
    let _polling = PollingGuard {
        polling_generation: Arc::clone(&context.polling_generation),
        generation: context.generation,
    };
    let mut backoff = Duration::from_millis(250);
    loop {
        if context.cancellation.is_cancelled() {
            return;
        }
        let Some(session) = context
            .state
            .lock()
            .await
            .active
            .clone()
            .filter(|session| session.generation == context.generation)
        else {
            return;
        };
        let mut request = context
            .client
            .get(&context.endpoint)
            .header("accept", "text/event-stream")
            .header("mcp-session-id", &session.session_id)
            .header("mcp-protocol-version", &session.protocol_version);
        if let Some(token) = &context.bearer_token {
            request = request.bearer_auth(token);
        }
        if let Some(last_event_id) = &session.last_event_id {
            request = request.header("last-event-id", last_event_id);
        }
        let response = tokio::select! {
            () = context.cancellation.cancelled() => return,
            response = request.send() => response,
        };
        let response =
            match response {
                Ok(response) => response,
                Err(error) => {
                    drop(context.inbound.send(TransportInbound::TransportFailed(
                        McpTransportError {
                            kind: McpTransportErrorKind::Network,
                            message: format!("MCP HTTP polling failed: {error}"),
                            retrying: true,
                        },
                    )));
                    if !sleep_or_cancel(&context.cancellation, backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            };
        match response.status() {
            reqwest::StatusCode::NOT_FOUND => {
                if clear_active_if_matches(&context.state, &session).await {
                    drop(context.inbound.send(TransportInbound::Disconnected(
                        "MCP HTTP session expired".to_owned(),
                    )));
                }
                return;
            }
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                drop(context.inbound.send(TransportInbound::AuthorizationFailed(
                    authorization_error(&response),
                )));
                return;
            }
            reqwest::StatusCode::METHOD_NOT_ALLOWED => return,
            status if !status.is_success() => {
                drop(
                    context
                        .inbound
                        .send(TransportInbound::TransportFailed(McpTransportError {
                            kind: McpTransportErrorKind::HttpStatus,
                            message: format!("MCP HTTP polling failed with status {status}"),
                            retrying: true,
                        })),
                );
                if !sleep_or_cancel(&context.cancellation, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
            _ => {}
        }
        if let Err(error) = read_sse(
            response,
            SseReadContext {
                state: Arc::clone(&context.state),
                expected_session: Some(session.clone()),
                inbound: context.inbound.clone(),
                cancellation: context.cancellation.clone(),
                retry: Arc::clone(&context.retry),
            },
        )
        .await
        {
            drop(
                context
                    .inbound
                    .send(TransportInbound::TransportFailed(classify_sse_error(
                        &error, true,
                    ))),
            );
            if !sleep_or_cancel(&context.cancellation, backoff).await {
                return;
            }
            backoff = (backoff * 2).min(Duration::from_secs(30));
            continue;
        }
        backoff = Duration::from_millis(250);
        let retry = *context.retry.lock().await;
        if !sleep_or_cancel(&context.cancellation, retry).await {
            return;
        }
    }
}

struct PollingGuard {
    polling_generation: Arc<AtomicU64>,
    generation: u64,
}

impl Drop for PollingGuard {
    fn drop(&mut self) {
        let _ = self.polling_generation.compare_exchange(
            self.generation,
            0,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}

struct SseReadContext {
    state: Arc<Mutex<HttpSessionState>>,
    expected_session: Option<ActiveHttpSession>,
    inbound: InboundSender,
    cancellation: CancellationToken,
    retry: Arc<Mutex<Duration>>,
}

async fn read_sse(response: reqwest::Response, context: SseReadContext) -> Result<()> {
    let SseReadContext {
        state,
        expected_session,
        inbound,
        cancellation,
        retry,
    } = context;
    let mut body = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    loop {
        let chunk = tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            chunk = body.next() => chunk,
        };
        let Some(chunk) = chunk else {
            deliver_frames(
                decoder.finish()?,
                &state,
                expected_session.as_ref(),
                &inbound,
                &retry,
            )
            .await?;
            return Ok(());
        };
        deliver_frames(
            decoder.push(&chunk?)?,
            &state,
            expected_session.as_ref(),
            &inbound,
            &retry,
        )
        .await?;
    }
}

// The state lock intentionally serializes delivery, event-id persistence, and test pause hooks.
#[allow(clippy::significant_drop_tightening)]
async fn deliver_frames(
    frames: Vec<SseFrame>,
    state: &Mutex<HttpSessionState>,
    expected_session: Option<&ActiveHttpSession>,
    inbound: &InboundSender,
    retry: &Mutex<Duration>,
) -> Result<()> {
    for frame in frames {
        let message = frame.data.as_deref().map(parse_json_rpc_str).transpose()?;
        let mut state = state.lock().await;
        if let Some(expected) = expected_session
            && !state.active.as_ref().is_some_and(|active| {
                active.generation == expected.generation && active.session_id == expected.session_id
            })
        {
            return Ok(());
        }
        #[cfg(test)]
        if let Some(pause) = state.delivery_pause.clone() {
            pause.0.notify_one();
            pause.1.notified().await;
        }
        if let Some(delay) = frame.retry {
            *retry.lock().await = delay.max(Duration::from_millis(10));
        }
        if let Some(message) = message {
            inbound
                .send(TransportInbound::Message(message))
                .map_err(|_| Error::protocol("MCP connection closed"))?;
        }
        if let Some(id) = frame.id {
            if let Some(active) = &mut state.active
                && expected_session.is_none_or(|expected| {
                    active.generation == expected.generation
                        && active.session_id == expected.session_id
                })
            {
                active.last_event_id = Some(id.clone());
            }
            inbound
                .send(TransportInbound::SseEventId(id))
                .map_err(|_| Error::protocol("MCP connection closed"))?;
        }
    }
    Ok(())
}

async fn sleep_or_cancel(cancellation: &CancellationToken, duration: Duration) -> bool {
    tokio::select! {
        () = cancellation.cancelled() => false,
        () = tokio::time::sleep(duration) => true,
    }
}

fn request_is_cancelled(cancellation: Option<&CancellationToken>) -> bool {
    cancellation.is_some_and(CancellationToken::is_cancelled)
}

async fn request_cancelled(cancellation: Option<&CancellationToken>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending().await,
    }
}

fn classify_sse_error(error: &Error, retrying: bool) -> McpTransportError {
    McpTransportError {
        kind: if matches!(error, Error::Http(_)) {
            McpTransportErrorKind::SseBody
        } else {
            McpTransportErrorKind::SseDecode
        },
        message: format!("MCP HTTP SSE reader failed: {error}"),
        retrying,
    }
}

async fn parse_initialize_response(
    response: reqwest::Response,
    expected_id: &JsonRpcId,
) -> Result<Value> {
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let inbound = if content_type.contains("text/event-stream") {
        let mut body = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        loop {
            let (frames, eof) = match body.next().await {
                Some(chunk) => (decoder.push(&chunk?)?, false),
                None => (decoder.finish()?, true),
            };
            for frame in frames {
                let Some(data) = frame.data else {
                    continue;
                };
                let message = parse_json_rpc_str(&data)?;
                if matches!(
                    &message,
                    JsonRpcInbound::Response(response) if response.id == *expected_id
                ) {
                    return match message {
                        JsonRpcInbound::Response(response) => json_rpc_result(response),
                        _ => unreachable!(),
                    };
                }
            }
            if eof {
                return Err(Error::protocol("MCP reinitialize SSE omitted a response"));
            }
        }
    } else {
        parse_json_rpc_message(serde_json::from_slice(
            &collect_body_bounded(response, HANDSHAKE_TIMEOUT).await?,
        )?)?
    };
    let JsonRpcInbound::Response(response) = inbound else {
        return Err(Error::protocol(
            "MCP reinitialize response was not a JSON-RPC response",
        ));
    };
    json_rpc_result(response)
}

async fn collect_body_bounded(response: reqwest::Response, timeout: Duration) -> Result<Vec<u8>> {
    tokio::time::timeout(timeout, async move {
        let mut body = response.bytes_stream();
        let mut collected = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            let next = collected
                .len()
                .checked_add(chunk.len())
                .filter(|size| *size <= HTTP_BODY_LIMIT)
                .ok_or_else(|| Error::protocol("MCP HTTP response body exceeds limit"))?;
            collected.reserve(next - collected.len());
            collected.extend_from_slice(&chunk);
        }
        Ok(collected)
    })
    .await
    .map_err(|_| Error::protocol("MCP HTTP response body timed out"))?
}

fn optional_response_header(
    headers: &reqwest::header::HeaderMap,
    name: &'static str,
) -> Result<Option<String>> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(Error::protocol(format!("duplicate {name} response header")));
    }
    Ok(Some(
        value
            .to_str()
            .map_err(|_| Error::protocol(format!("invalid {name} response header")))?
            .to_owned(),
    ))
}

fn check_authorization(response: &reqwest::Response) -> Result<()> {
    if matches!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return Err(Error::Authorization(Box::new(authorization_error(
            response,
        ))));
    }
    Ok(())
}

fn authorization_error(response: &reqwest::Response) -> McpAuthorizationError {
    let challenge = McpBearerChallenge::parse_any(
        response
            .headers()
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok()),
    );
    McpAuthorizationError {
        status: response.status(),
        challenge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::{Body, Bytes},
        response::{IntoResponse as _, Response},
        routing::post,
    };
    use futures_util::stream;
    use serde_json::json;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn stale_delivery_cannot_cross_session_replacement() {
        let (inbound, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(HttpSessionState::default()));
        {
            let mut state = state.lock().await;
            activate_session(
                &mut state,
                "old".to_owned(),
                MCP_PROTOCOL_VERSION.to_owned(),
            );
            state.delivery_pause = Some(Arc::new((Notify::new(), Notify::new())));
        }
        let old = state.lock().await.active.clone().unwrap();
        let pause = state.lock().await.delivery_pause.clone().unwrap();
        let delivering_old = old.clone();
        let delivering = tokio::spawn({
            let state = Arc::clone(&state);
            let inbound = inbound.clone();
            async move {
                deliver_frames(
                    vec![SseFrame {
                        id: Some("old:1".to_owned()),
                        data: Some(
                            json!({"jsonrpc":"2.0","method":"notifications/message"}).to_string(),
                        ),
                        retry: None,
                    }],
                    &state,
                    Some(&delivering_old),
                    &inbound,
                    &Mutex::new(Duration::from_secs(1)),
                )
                .await
                .unwrap();
            }
        });
        pause.0.notified().await;
        let replacing = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                let mut state = state.lock().await;
                activate_session(
                    &mut state,
                    "new".to_owned(),
                    MCP_PROTOCOL_VERSION.to_owned(),
                );
                state.delivery_pause = None;
            }
        });
        tokio::task::yield_now().await;
        assert!(!replacing.is_finished());
        assert!(receiver.try_recv().is_err());
        pause.1.notify_one();
        delivering.await.unwrap();
        replacing.await.unwrap();
        assert!(matches!(
            receiver.recv().await,
            Some(TransportInbound::Message(_))
        ));
        assert!(
            matches!(receiver.recv().await, Some(TransportInbound::SseEventId(id)) if id == "old:1")
        );
        let replacement = state.lock().await.active.clone().unwrap();
        assert_eq!(replacement.session_id, "new");
        assert!(replacement.last_event_id.is_none());

        deliver_frames(
            vec![SseFrame {
                id: Some("old:2".to_owned()),
                data: Some(json!({"jsonrpc":"2.0","method":"notifications/message"}).to_string()),
                retry: None,
            }],
            &state,
            Some(&old),
            &inbound,
            &Mutex::new(Duration::from_secs(1)),
        )
        .await
        .unwrap();
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn completed_post_sse_readers_are_reaped_during_lifetime() {
        async fn finite_sse() -> axum::response::Response {
            (
                [("content-type", "text/event-stream")],
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n",
            )
                .into_response()
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/mcp", post(finite_sse)))
                .await
                .unwrap();
        });
        let (inbound, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let transport = StreamableHttpTransport::new(endpoint, None, inbound);
        for sequence in 0..100 {
            transport
                .send(
                    json!({
                        "jsonrpc": "2.0",
                        "id": sequence,
                        "method": "ping",
                    }),
                    false,
                    None,
                )
                .await
                .unwrap();
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while transport.tracked_readers.load(Ordering::SeqCst) != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("reader supervisor self-reaps finite readers while idle");
        assert_eq!(transport.tracked_readers.load(Ordering::SeqCst), 0);
        let (first, second) = tokio::join!(transport.shutdown(), transport.shutdown());
        first.unwrap();
        second.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn request_cancellation_interrupts_json_body_collection() {
        let response_started = Arc::new(Notify::new());
        let handler_started = Arc::clone(&response_started);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let app = Router::new().route(
                "/mcp",
                post(move || {
                    let handler_started = Arc::clone(&handler_started);
                    async move {
                        handler_started.notify_one();
                        Response::builder()
                            .header("content-type", "application/json")
                            .body(Body::from_stream(stream::pending::<
                                std::result::Result<Bytes, std::convert::Infallible>,
                            >()))
                            .unwrap()
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let (inbound, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let transport = StreamableHttpTransport::new(endpoint, None, inbound);
        let cancellation = CancellationToken::new();
        let send = transport.send(
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}),
            false,
            Some(&cancellation),
        );
        tokio::pin!(send);

        tokio::select! {
            () = response_started.notified() => {}
            result = &mut send => panic!("request ended before response body: {result:?}"),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(1), &mut send)
            .await
            .expect("request cancellation is prompt")
            .unwrap_err();
        assert!(error.to_string().contains("request canceled"));
        transport.shutdown().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn cancellation_ignoring_reader_is_aborted_joined_and_counted_exactly() {
        struct DeathSignal(Arc<AtomicBool>);

        impl Drop for DeathSignal {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let (inbound, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let transport =
            StreamableHttpTransport::new("http://127.0.0.1:1/mcp".to_owned(), None, inbound);
        let died = Arc::new(AtomicBool::new(false));
        let started = Arc::new(Notify::new());
        let reader = tokio::spawn({
            let died = Arc::clone(&died);
            let started = Arc::clone(&started);
            async move {
                let _death = DeathSignal(died);
                started.notify_one();
                std::future::pending::<()>().await;
            }
        });
        started.notified().await;
        transport.track_reader(reader).await;
        assert_eq!(transport.tracked_readers.load(Ordering::SeqCst), 1);

        let (first, second) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(transport.shutdown(), transport.shutdown())
        })
        .await
        .expect("concurrent shutdown is bounded");
        assert!(first.is_ok());
        assert!(second.is_ok());
        assert_eq!(first.is_ok(), second.is_ok());
        assert!(died.load(Ordering::SeqCst));
        assert_eq!(transport.tracked_readers.load(Ordering::SeqCst), 0);
        assert!(transport.reader_supervisor.lock().await.is_none());
        assert!(
            transport
                .reader_aborts
                .lock()
                .expect("reader abort registry")
                .is_empty()
        );

        let late_died = Arc::new(AtomicBool::new(false));
        let late_started = Arc::new(Notify::new());
        let late_reader = tokio::spawn({
            let late_died = Arc::clone(&late_died);
            let late_started = Arc::clone(&late_started);
            async move {
                let _death = DeathSignal(late_died);
                late_started.notify_one();
                std::future::pending::<()>().await;
            }
        });
        late_started.notified().await;
        transport.track_reader(late_reader).await;
        assert!(late_died.load(Ordering::SeqCst));
        assert_eq!(transport.tracked_readers.load(Ordering::SeqCst), 0);
    }
}
