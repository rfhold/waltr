//! MCP server OpenTelemetry integration coverage.

use std::{collections::BTreeSet, sync::Arc};

use mcp::{
    McpPromptResult, McpToolCall, McpToolResult,
    protocol::MCP_PROTOCOL_VERSION,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
        ServerResult,
    },
};
use opentelemetry::{
    global,
    trace::{SpanId, SpanKind, Status, TraceId, TracerProvider as _},
};
use opentelemetry_sdk::{
    metrics::{
        InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
        data::{AggregatedMetrics, MetricData},
    },
    trace::{InMemorySpanExporter, SdkTracerProvider, SpanData},
};
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::Notify};
use tracing_subscriber::prelude::*;

const PARENT_TRACE_ID: TraceId = TraceId::from_bytes([
    0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47, 0x36,
]);
const PARENT_SPAN_ID: SpanId = SpanId::from_bytes([0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7]);

struct TelemetryServer {
    notification_finished: Arc<Notify>,
    pending_started: Arc<Notify>,
    pending_dropped: Arc<Notify>,
}

struct NotifyOnDrop(Arc<Notify>);

impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

impl ServerHandler for TelemetryServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("telemetry-test", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().prompts().tools()
    }

    fn get_prompt(
        &self,
        name: String,
        _arguments: Option<Value>,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpPromptResult>> {
        if name == "error" {
            return Box::pin(async { Err(ServerError::internal("test error")) });
        }
        let started = Arc::clone(&self.pending_started);
        let dropped = Arc::clone(&self.pending_dropped);
        Box::pin(async move {
            let _drop = NotifyOnDrop(dropped);
            context
                .notify("notifications/progress", Some(json!({ "progress": 1 })))
                .await
                .expect("send pending response frame");
            started.notify_one();
            std::future::pending().await
        })
    }

    fn handle_notification(
        &self,
        _notification: mcp::protocol::JsonRpcNotification,
        _context: ServerContext,
    ) -> BoxFuture<()> {
        let finished = Arc::clone(&self.notification_finished);
        Box::pin(async move { finished.notify_one() })
    }

    fn call_tool(
        &self,
        _call: McpToolCall,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolResult>> {
        Box::pin(async {
            Ok(McpToolResult {
                raw: json!({ "content": [], "isError": true }),
            })
        })
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn server_telemetry_covers_http_and_stdio_dispatch_lifecycles() -> mcp::Result<()> {
    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("mcp-telemetry-test")),
    );
    tracing::subscriber::set_global_default(subscriber)
        .expect("isolated telemetry test subscriber");

    let metric_exporter = InMemoryMetricExporter::default();
    let meter_provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(metric_exporter.clone()).build())
        .build();
    global::set_meter_provider(meter_provider.clone());

    let notification_finished = Arc::new(Notify::new());
    let pending_started = Arc::new(Notify::new());
    let pending_dropped = Arc::new(Notify::new());
    let handler: Arc<dyn ServerHandler> = Arc::new(TelemetryServer {
        notification_finished: Arc::clone(&notification_finished),
        pending_started: Arc::clone(&pending_started),
        pending_dropped: Arc::clone(&pending_dropped),
    });

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    let router = mcp::server::streamable_http_router(Arc::clone(&handler));
    let http_task = tokio::spawn(async move { axum::serve(listener, router).await });

    let success = request("server/discover", "sensitive-request-id", json!({}), true);
    let response = post(&endpoint, &success).await?;
    assert!(response.status().is_success());
    drop(response.bytes().await?);

    let error = request("prompts/get", 2, json!({ "name": "error" }), false);
    let response = post(&endpoint, &error).await?;
    assert!(response.status().is_success());
    drop(response.bytes().await?);

    let initialized = notification("notifications/initialized", json!({}));
    assert_eq!(post(&endpoint, &initialized).await?.status(), 202);
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        notification_finished.notified(),
    )
    .await
    .expect("HTTP notification completes");

    let pending = request("prompts/get", 3, json!({ "name": "wait" }), false);
    let response = post(&endpoint, &pending).await?;
    pending_started.notified().await;
    drop(response);
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        pending_dropped.notified(),
    )
    .await
    .expect("HTTP response drop cancels the handler");

    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let stdio_task = tokio::spawn(mcp::server::serve_stream(
        Arc::clone(&handler),
        server_read,
        server_write,
    ));
    let (client_read, mut client_write) = tokio::io::split(client);
    for message in [
        request("server/discover", 4, json!({}), false),
        request("extension/private-method", 5, json!({}), false),
        request(
            "tools/call",
            6,
            json!({
                "name": "domain-error",
                "arguments": {
                    "secretArgument": "secret-argument-value",
                    "url": "https://secret.invalid/path"
                }
            }),
            false,
        ),
        request("prompts/get", 7, json!({ "name": "wait" }), false),
    ] {
        client_write
            .write_all(format!("{}\n", serde_json::to_string(&message)?).as_bytes())
            .await?;
    }
    pending_started.notified().await;
    let cancelled = json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 7, "reason": "test" },
    });
    client_write
        .write_all(format!("{}\n", serde_json::to_string(&cancelled)?).as_bytes())
        .await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        pending_dropped.notified(),
    )
    .await
    .expect("stdio cancellation drops the handler");
    for message in [notification("notifications/initialized", json!({}))] {
        client_write
            .write_all(format!("{}\n", serde_json::to_string(&message)?).as_bytes())
            .await?;
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        notification_finished.notified(),
    )
    .await
    .expect("stdio notification completes");
    drop(client_write);
    drop(client_read);
    tokio::time::timeout(std::time::Duration::from_secs(1), stdio_task)
        .await
        .expect("stdio server stops")
        .expect("stdio task")?;

    tokio::task::yield_now().await;
    tracer_provider.force_flush().expect("flush spans");
    meter_provider.force_flush().expect("flush metrics");
    let spans = span_exporter.get_finished_spans().expect("export spans");

    let parented = find_span(&spans, "server/discover", "streamable_http", "success");
    assert_eq!(parented.name, "mcp.server.request");
    assert_eq!(parented.span_kind, SpanKind::Server);
    assert_eq!(parented.span_context.trace_id(), PARENT_TRACE_ID);
    assert_eq!(parented.parent_span_id, PARENT_SPAN_ID);
    assert_attribute(parented, "rpc.system", "jsonrpc");
    assert_attribute(parented, "rpc.jsonrpc.version", "2.0");
    assert_attribute(parented, "mcp.method", "server/discover");
    assert_attribute(parented, "mcp.protocol_version", MCP_PROTOCOL_VERSION);
    assert_attribute(parented, "mcp.request_id", "sensitive-request-id");

    let error_span = find_span(&spans, "prompts/get", "streamable_http", "error");
    assert_eq!(error_span.status, Status::error(""));
    assert!(error_span.attributes.iter().any(|attribute| {
        attribute.key.as_str() == "rpc.jsonrpc.error_code"
            && attribute.value == opentelemetry::Value::I64(-32603)
    }));
    let cancelled = find_span(&spans, "prompts/get", "streamable_http", "cancelled");
    assert_eq!(cancelled.status, Status::error(""));
    find_span(
        &spans,
        "notifications/initialized",
        "streamable_http",
        "success",
    );
    find_span(&spans, "server/discover", "stdio", "success");
    find_span(&spans, "extension/private-method", "stdio", "error");
    find_span(&spans, "tools/call", "stdio", "success");
    let stdio_cancelled = find_span(&spans, "prompts/get", "stdio", "cancelled");
    assert_eq!(stdio_cancelled.status, Status::error(""));
    find_span(&spans, "notifications/initialized", "stdio", "success");

    let metrics = metric_exporter
        .get_finished_metrics()
        .expect("export metrics");
    assert_server_metrics(&metrics);

    http_task.abort();
    Ok(())
}

fn request(method: &str, id: impl serde::Serialize, params: Value, trace: bool) -> Value {
    let mut request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": with_meta(params),
    });
    if trace {
        request["params"]["_meta"]["traceparent"] =
            Value::String("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_owned());
    }
    request
}

fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": with_meta(params) })
}

fn with_meta(mut params: Value) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": MCP_PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    params
}

async fn post(endpoint: &str, body: &Value) -> mcp::Result<reqwest::Response> {
    let method = body["method"].as_str().expect("method");
    let mut request = reqwest::Client::new()
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .header("mcp-method", method)
        .json(body);
    if matches!(method, "prompts/get" | "tools/call") {
        request = request.header("mcp-name", body["params"]["name"].as_str().expect("name"));
    }
    Ok(request.send().await?)
}

fn find_span<'a>(
    spans: &'a [SpanData],
    method: &str,
    transport: &str,
    outcome: &str,
) -> &'a SpanData {
    spans
        .iter()
        .find(|span| {
            has_attribute(span, "rpc.method", method)
                && has_attribute(span, "mcp.transport", transport)
                && has_attribute(span, "mcp.outcome", outcome)
        })
        .unwrap_or_else(|| panic!("missing {transport} {method} {outcome} span: {spans:#?}"))
}

fn assert_attribute(span: &SpanData, key: &str, value: &str) {
    assert!(
        has_attribute(span, key, value),
        "missing {key}={value}: {span:#?}"
    );
}

fn has_attribute(span: &SpanData, key: &str, value: &str) -> bool {
    span.attributes.iter().any(|attribute| {
        attribute.key.as_str() == key
            && attribute.value == opentelemetry::Value::String(value.to_owned().into())
    })
}

#[allow(clippy::too_many_lines)]
fn assert_server_metrics(metrics: &[opentelemetry_sdk::metrics::data::ResourceMetrics]) {
    let metrics = metrics
        .iter()
        .flat_map(opentelemetry_sdk::metrics::data::ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .collect::<Vec<_>>();
    assert_eq!(
        metrics.len(),
        3,
        "only the three server instruments are emitted"
    );
    assert_eq!(
        metrics
            .iter()
            .map(|metric| metric.name())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "mcp.server.request.count",
            "mcp.server.request.duration",
            "mcp.server.request.in_flight",
        ])
    );

    let count = metrics
        .iter()
        .find(|metric| metric.name() == "mcp.server.request.count")
        .expect("request count");
    assert_eq!(count.unit(), "{request}");
    let AggregatedMetrics::U64(MetricData::Sum(count)) = count.data() else {
        panic!("request count must be a u64 sum")
    };
    let count_series = count
        .data_points()
        .map(|point| {
            assert_eq!(point.value(), 1, "each accepted dispatch completes once");
            completion_series(point.attributes())
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(count_series, expected_completion_series());
    assert_eq!(
        count
            .data_points()
            .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
            .sum::<u64>(),
        9
    );

    let duration = metrics
        .iter()
        .find(|metric| metric.name() == "mcp.server.request.duration")
        .expect("request duration");
    assert_eq!(duration.unit(), "s");
    let AggregatedMetrics::F64(MetricData::Histogram(duration)) = duration.data() else {
        panic!("request duration must be an f64 histogram")
    };
    let duration_series = duration
        .data_points()
        .map(|point| {
            assert_eq!(point.count(), 1, "each accepted dispatch has one duration");
            completion_series(point.attributes())
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(duration_series, expected_completion_series());
    assert_eq!(
        duration
            .data_points()
            .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
            .sum::<u64>(),
        9
    );

    let in_flight = metrics
        .iter()
        .find(|metric| metric.name() == "mcp.server.request.in_flight")
        .expect("in-flight requests");
    assert_eq!(in_flight.unit(), "{request}");
    let AggregatedMetrics::I64(MetricData::Sum(in_flight)) = in_flight.data() else {
        panic!("in-flight requests must be an i64 sum")
    };
    let mut transports = BTreeSet::new();
    for point in in_flight.data_points() {
        assert_eq!(point.value(), 0, "every in-flight series must balance");
        let attributes = point.attributes().collect::<Vec<_>>();
        assert_metric_attributes(&attributes, false);
        transports.insert(string_attribute(&attributes, "mcp.transport").to_owned());
    }
    assert_eq!(
        transports,
        BTreeSet::from(["stdio".to_owned(), "streamable_http".to_owned()])
    );
}

fn expected_completion_series() -> BTreeSet<(String, String, String, Option<i64>)> {
    [
        ("server/discover", "streamable_http", "success", None),
        ("prompts/get", "streamable_http", "error", Some(-32603)),
        (
            "notifications/initialized",
            "streamable_http",
            "success",
            None,
        ),
        ("prompts/get", "streamable_http", "cancelled", None),
        ("server/discover", "stdio", "success", None),
        ("other", "stdio", "error", Some(-32601)),
        ("tools/call", "stdio", "success", None),
        ("prompts/get", "stdio", "cancelled", None),
        ("notifications/initialized", "stdio", "success", None),
    ]
    .into_iter()
    .map(|(method, transport, outcome, error_code)| {
        (
            method.to_owned(),
            transport.to_owned(),
            outcome.to_owned(),
            error_code,
        )
    })
    .collect()
}

fn completion_series<'a>(
    attributes: impl Iterator<Item = &'a opentelemetry::KeyValue>,
) -> (String, String, String, Option<i64>) {
    let attributes = attributes.collect::<Vec<_>>();
    assert_metric_attributes(&attributes, true);
    (
        string_attribute(&attributes, "rpc.method").to_owned(),
        string_attribute(&attributes, "mcp.transport").to_owned(),
        string_attribute(&attributes, "mcp.outcome").to_owned(),
        integer_attribute(&attributes, "rpc.jsonrpc.error_code"),
    )
}

fn assert_metric_attributes(attributes: &[&opentelemetry::KeyValue], completed: bool) {
    let keys = attributes
        .iter()
        .map(|attribute| attribute.key.as_str())
        .collect::<BTreeSet<_>>();
    let mut expected = BTreeSet::from(["rpc.method", "mcp.transport", "mcp.protocol_version"]);
    if completed {
        expected.insert("mcp.outcome");
        if integer_attribute(attributes, "rpc.jsonrpc.error_code").is_some() {
            expected.insert("rpc.jsonrpc.error_code");
        }
    }
    assert_eq!(keys, expected, "metric attributes must remain bounded");
    assert!(matches!(
        string_attribute(attributes, "rpc.method"),
        "server/discover" | "prompts/get" | "tools/call" | "notifications/initialized" | "other"
    ));
    assert!(matches!(
        string_attribute(attributes, "mcp.transport"),
        "stdio" | "streamable_http"
    ));
    assert_eq!(
        string_attribute(attributes, "mcp.protocol_version"),
        MCP_PROTOCOL_VERSION
    );
    if completed {
        assert!(matches!(
            string_attribute(attributes, "mcp.outcome"),
            "success" | "error" | "cancelled"
        ));
    }
    for attribute in attributes {
        if let opentelemetry::Value::String(value) = &attribute.value {
            assert!(
                ![
                    "sensitive-request-id",
                    "domain-error",
                    "secretArgument",
                    "secret-argument-value",
                    "https://secret.invalid/path",
                    "test error",
                    "extension/private-method",
                    "private-version",
                ]
                .contains(&value.as_str())
            );
        }
    }
}

fn string_attribute<'a>(attributes: &'a [&opentelemetry::KeyValue], key: &str) -> &'a str {
    attributes
        .iter()
        .find_map(|attribute| (attribute.key.as_str() == key).then_some(&attribute.value))
        .and_then(|value| match value {
            opentelemetry::Value::String(value) => Some(value.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing string metric attribute {key}: {attributes:#?}"))
}

fn integer_attribute(attributes: &[&opentelemetry::KeyValue], key: &str) -> Option<i64> {
    attributes.iter().find_map(|attribute| {
        if attribute.key.as_str() != key {
            return None;
        }
        match attribute.value {
            opentelemetry::Value::I64(value) => Some(value),
            _ => panic!("metric attribute {key} is not an integer: {attribute:#?}"),
        }
    })
}
