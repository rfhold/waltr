use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use futures_util::StreamExt;
use serde_json::Value;
use tokio::{sync::Mutex, task::JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::{
    Error, Result,
    http_headers::encode_header_value,
    oauth::{McpAuthorizationError, McpBearerChallenge},
    protocol::{
        JsonRpcId, JsonRpcInbound, MCP_PROTOCOL_VERSION, MCP_PROTOCOL_VERSION_META_KEY,
        parse_json_rpc_message,
    },
    trace::{request_id, request_method},
};

use super::{
    InboundSender, McpBearerProvider, McpTransportError, McpTransportErrorKind, McpTransportLimits,
    TransportInbound, sse::SseDecoder,
};

/// Stateless Streamable HTTP client transport.
pub struct StreamableHttpTransport {
    endpoint: String,
    bearer_token: Option<String>,
    bearer_provider: Option<Arc<dyn McpBearerProvider>>,
    client: reqwest::Client,
    inbound: InboundSender,
    cancellation: CancellationToken,
    readers: Mutex<JoinSet<()>>,
    shutting_down: AtomicBool,
    limits: McpTransportLimits,
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
    /// Create a stateless Streamable HTTP transport.
    #[must_use]
    pub fn new(
        endpoint: String,
        bearer_token: Option<String>,
        bearer_provider: Option<Arc<dyn McpBearerProvider>>,
        client: reqwest::Client,
        inbound: InboundSender,
        limits: McpTransportLimits,
    ) -> Self {
        Self {
            endpoint,
            bearer_token,
            bearer_provider,
            client,
            inbound,
            cancellation: CancellationToken::new(),
            readers: Mutex::new(JoinSet::new()),
            shutting_down: AtomicBool::new(false),
            limits,
        }
    }

    /// Send one JSON-RPC message as one HTTP POST.
    #[allow(clippy::too_many_lines)]
    pub async fn send(
        &self,
        message: Value,
        request_cancellation: Option<&CancellationToken>,
        custom_headers: &[(String, String)],
    ) -> Result<()> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(Error::protocol("MCP HTTP transport is shutting down"));
        }
        let parsed = parse_json_rpc_message(message.clone())?;
        if matches!(parsed, JsonRpcInbound::Response(_)) {
            return Err(Error::protocol(
                "MCP clients must not send JSON-RPC responses over Streamable HTTP",
            ));
        }
        let request_json_rpc_id = match &parsed {
            JsonRpcInbound::Request(request) => Some(request.id.clone()),
            _ => None,
        };
        let method = request_method(&message)
            .filter(|method| !method.is_empty())
            .ok_or_else(|| Error::protocol("MCP HTTP message omitted method"))?;
        let protocol = message
            .get("params")
            .and_then(|params| params.get("_meta"))
            .and_then(|meta| meta.get(MCP_PROTOCOL_VERSION_META_KEY))
            .and_then(Value::as_str)
            .ok_or_else(|| Error::protocol("MCP HTTP request omitted protocol metadata"))?;
        if protocol != MCP_PROTOCOL_VERSION {
            return Err(Error::protocol(format!(
                "unsupported MCP protocol version `{protocol}`"
            )));
        }

        let mut builder = self
            .client
            .post(&self.endpoint)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", protocol)
            .header("mcp-method", method)
            .json(&message);
        if let Some(name) = mirrored_name(method, &message) {
            builder = builder.header("mcp-name", encode_header_value(&name));
        }
        for (name, value) in custom_headers {
            builder = builder.header(name, value);
        }
        if let Some(provider) = &self.bearer_provider {
            match provider.bearer_token().await {
                Ok(token) => builder = builder.bearer_auth(token),
                Err(error) => {
                    drop(
                        self.inbound
                            .send(TransportInbound::BearerProviderFailed(error)),
                    );
                    return Err(Error::BearerProvider(error));
                }
            }
        } else if let Some(token) = &self.bearer_token {
            builder = builder.bearer_auth(token);
        }

        let id = request_id(&message);
        let span = tracing::info_span!(
            "http.client.request",
            http_request_method = "POST",
            server_address = %self.endpoint,
            client = "mcp",
            http_response_status_code = tracing::field::Empty,
            rpc.system = "jsonrpc",
            rpc.jsonrpc.version = "2.0",
            rpc.method = %method,
            mcp.method = %method,
            mcp.request_id = id.as_deref().unwrap_or(""),
            mcp.transport = "streamable_http",
            mcp.protocol_version = MCP_PROTOCOL_VERSION,
        );
        let response = tokio::select! {
            () = request_cancelled(request_cancellation) => return Err(Error::protocol("MCP request canceled")),
            () = self.cancellation.cancelled() => return Err(Error::protocol("MCP HTTP transport is shutting down")),
            response = builder.send().instrument(span.clone()) => response?,
        };
        span.record("http_response_status_code", response.status().as_u16());
        if matches!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            let error = authorization_error(&response);
            drop(
                self.inbound
                    .send(TransportInbound::AuthorizationFailed(error.clone())),
            );
            return Err(Error::Authorization(Box::new(error)));
        }
        let status = response.status();
        if status == reqwest::StatusCode::ACCEPTED || response.content_length() == Some(0) {
            return if status.is_success() {
                Ok(())
            } else {
                Err(Error::protocol(format!(
                    "MCP HTTP request failed with status {status}"
                )))
            };
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if content_type.contains("text/event-stream") {
            let inbound = self.inbound.clone();
            let shutdown = self.cancellation.clone();
            let request_cancel = request_cancellation.cloned();
            let limits = self.limits;
            {
                let mut readers = self.readers.lock().await;
                while readers.try_join_next().is_some() {}
                if self.shutting_down.load(Ordering::SeqCst) {
                    return Err(Error::protocol("MCP HTTP transport is shutting down"));
                }
                readers.spawn(async move {
                    read_sse(
                        response,
                        inbound,
                        shutdown,
                        request_cancel,
                        request_json_rpc_id,
                        limits,
                    )
                    .await;
                });
            }
            return Ok(());
        }
        let body = collect_body_bounded(response, self.limits.max_message_bytes).await?;
        let inbound = parse_json_rpc_message(serde_json::from_slice(&body)?)?;
        self.inbound
            .send(TransportInbound::Message(inbound))
            .map_err(|_| Error::protocol("MCP connection closed"))?;
        if status.is_success() || status == reqwest::StatusCode::BAD_REQUEST {
            Ok(())
        } else {
            Err(Error::protocol(format!(
                "MCP HTTP request failed with status {status}"
            )))
        }
    }

    /// Stop every active response stream. No protocol DELETE is sent.
    pub async fn shutdown(&self) -> Result<()> {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.cancellation.cancel();
        let mut readers = self.readers.lock().await;
        while let Some(result) = readers.join_next().await {
            drop(result);
        }
        Ok(())
    }
}

fn mirrored_name(method: &str, message: &Value) -> Option<String> {
    let params = message.get("params")?;
    match method {
        "tools/call" | "prompts/get" => params.get("name")?.as_str().map(ToOwned::to_owned),
        "resources/read" => params.get("uri")?.as_str().map(ToOwned::to_owned),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn read_sse(
    response: reqwest::Response,
    inbound: InboundSender,
    shutdown: CancellationToken,
    request_cancel: Option<CancellationToken>,
    request_id: Option<JsonRpcId>,
    limits: McpTransportLimits,
) {
    let mut body = response.bytes_stream();
    let mut decoder = SseDecoder::with_limits(limits.max_sse_line_bytes, limits.max_message_bytes);
    let mut terminal = false;
    loop {
        let chunk = tokio::select! {
            () = shutdown.cancelled() => return,
            () = request_cancelled(request_cancel.as_ref()) => return,
            chunk = body.next() => chunk,
        };
        let Some(chunk) = chunk else { break };
        let frames = match chunk
            .map_err(Error::from)
            .and_then(|chunk| decoder.push(&chunk))
        {
            Ok(frames) => frames,
            Err(error) => {
                report_request_stream_error(&inbound, request_id.as_ref(), &error);
                return;
            }
        };
        match dispatch_frames(&inbound, frames) {
            Ok(found_terminal) => terminal |= found_terminal,
            Err(error) => {
                report_request_stream_error(&inbound, request_id.as_ref(), &error);
                return;
            }
        }
    }
    match decoder.finish() {
        Ok(frames) => match dispatch_frames(&inbound, frames) {
            Ok(found_terminal) => terminal |= found_terminal,
            Err(error) => {
                report_request_stream_error(&inbound, request_id.as_ref(), &error);
                return;
            }
        },
        Err(error) => {
            report_request_stream_error(&inbound, request_id.as_ref(), &error);
            return;
        }
    }
    if !terminal && let Some(request_id) = request_id {
        drop(inbound.send(TransportInbound::RequestFailed(
            request_id,
            "MCP SSE response ended before a terminal response".to_owned(),
        )));
    }
}

fn dispatch_frames(inbound: &InboundSender, frames: Vec<super::sse::SseFrame>) -> Result<bool> {
    let mut terminal = false;
    for frame in frames {
        let Some(data) = frame.data else { continue };
        let message = crate::protocol::parse_json_rpc_str(&data)?;
        terminal |= matches!(message, JsonRpcInbound::Response(_));
        inbound
            .send(TransportInbound::Message(message))
            .map_err(|_| Error::protocol("MCP connection closed"))?;
    }
    Ok(terminal)
}

fn report_request_stream_error(
    inbound: &InboundSender,
    request_id: Option<&JsonRpcId>,
    error: &Error,
) {
    if let Some(request_id) = request_id {
        drop(inbound.send(TransportInbound::RequestFailed(
            request_id.clone(),
            error.to_string(),
        )));
    } else {
        report_stream_error(inbound, error);
    }
}

fn report_stream_error(inbound: &InboundSender, error: &Error) {
    drop(
        inbound.send(TransportInbound::TransportFailed(McpTransportError {
            kind: McpTransportErrorKind::SseDecode,
            message: error.to_string(),
            retrying: false,
        })),
    );
}

async fn collect_body_bounded(
    response: reqwest::Response,
    max_message_bytes: usize,
) -> Result<Vec<u8>> {
    let mut body = response.bytes_stream();
    let mut collected = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        let next = collected
            .len()
            .checked_add(chunk.len())
            .filter(|size| *size <= max_message_bytes)
            .ok_or_else(|| Error::protocol("MCP HTTP response body exceeds limit"))?;
        collected.reserve(next - collected.len());
        collected.extend_from_slice(&chunk);
    }
    Ok(collected)
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

async fn request_cancelled(cancellation: Option<&CancellationToken>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending().await,
    }
}
