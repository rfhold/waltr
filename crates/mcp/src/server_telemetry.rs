use std::{sync::Arc, time::Instant};

use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Histogram, Meter, UpDownCounter},
};

use crate::protocol::MCP_PROTOCOL_VERSION;

#[derive(Clone)]
pub struct ServerTelemetry {
    inner: Arc<Instruments>,
}

struct Instruments {
    count: Counter<u64>,
    duration: Histogram<f64>,
    in_flight: UpDownCounter<i64>,
}

impl ServerTelemetry {
    pub(crate) fn new() -> Self {
        Self::from_meter(&global::meter("mcp"))
    }

    fn from_meter(meter: &Meter) -> Self {
        Self {
            inner: Arc::new(Instruments {
                count: meter
                    .u64_counter("mcp.server.request.count")
                    .with_description("Accepted MCP server requests and notifications")
                    .with_unit("{request}")
                    .build(),
                duration: meter
                    .f64_histogram("mcp.server.request.duration")
                    .with_description("MCP server request and notification duration")
                    .with_unit("s")
                    .build(),
                in_flight: meter
                    .i64_up_down_counter("mcp.server.request.in_flight")
                    .with_description("Accepted MCP server requests and notifications in flight")
                    .with_unit("{request}")
                    .build(),
            }),
        }
    }

    pub(crate) fn start(
        &self,
        method: &str,
        transport: &'static str,
        protocol_version: &str,
        span: tracing::Span,
    ) -> ServerRequestTelemetry {
        let attributes = vec![
            KeyValue::new("rpc.method", metric_method(method)),
            KeyValue::new("mcp.transport", transport),
            KeyValue::new(
                "mcp.protocol_version",
                metric_protocol_version(protocol_version),
            ),
        ];
        self.inner.in_flight.add(1, &attributes);
        ServerRequestTelemetry {
            instruments: Arc::clone(&self.inner),
            attributes,
            span,
            started: Instant::now(),
            completed: false,
        }
    }
}

pub struct ServerRequestTelemetry {
    instruments: Arc<Instruments>,
    attributes: Vec<KeyValue>,
    span: tracing::Span,
    started: Instant,
    completed: bool,
}

impl ServerRequestTelemetry {
    pub(crate) fn success(&mut self) {
        self.complete("success", None);
    }

    pub(crate) fn response_error_code(&mut self, error_code: Option<i64>) {
        if let Some(error_code) = error_code {
            self.complete("error", Some(error_code));
        } else {
            self.success();
        }
    }

    fn complete(&mut self, outcome: &'static str, error_code: Option<i64>) {
        if self.completed {
            return;
        }
        self.completed = true;

        self.span.record("mcp.outcome", outcome);
        if outcome != "success" {
            self.span.record("otel.status_code", "ERROR");
        }
        if let Some(error_code) = error_code {
            self.span.record("rpc.jsonrpc.error_code", error_code);
        }

        let mut completed_attributes = self.attributes.clone();
        completed_attributes.push(KeyValue::new("mcp.outcome", outcome));
        if let Some(error_code) = error_code {
            completed_attributes.push(KeyValue::new("rpc.jsonrpc.error_code", error_code));
        }
        self.instruments.count.add(1, &completed_attributes);
        self.instruments
            .duration
            .record(self.started.elapsed().as_secs_f64(), &completed_attributes);
        self.instruments.in_flight.add(-1, &self.attributes);
    }
}

impl Drop for ServerRequestTelemetry {
    fn drop(&mut self) {
        self.complete("cancelled", None);
    }
}

fn metric_method(method: &str) -> &'static str {
    match method {
        "server/discover" => "server/discover",
        "subscriptions/listen" => "subscriptions/listen",
        "tools/list" => "tools/list",
        "tools/call" => "tools/call",
        "prompts/list" => "prompts/list",
        "prompts/get" => "prompts/get",
        "resources/list" => "resources/list",
        "resources/templates/list" => "resources/templates/list",
        "resources/read" => "resources/read",
        "completion/complete" => "completion/complete",
        "notifications/cancelled" => "notifications/cancelled",
        "notifications/initialized" => "notifications/initialized",
        "notifications/message" => "notifications/message",
        "notifications/progress" => "notifications/progress",
        "notifications/prompts/list_changed" => "notifications/prompts/list_changed",
        "notifications/resources/list_changed" => "notifications/resources/list_changed",
        "notifications/subscriptions/acknowledged" => "notifications/subscriptions/acknowledged",
        "notifications/tools/list_changed" => "notifications/tools/list_changed",
        _ => "other",
    }
}

fn metric_protocol_version(protocol_version: &str) -> &'static str {
    if protocol_version == MCP_PROTOCOL_VERSION {
        MCP_PROTOCOL_VERSION
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::{
        InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
        data::{AggregatedMetrics, MetricData},
    };

    use super::*;

    #[test]
    fn metric_dimensions_are_bounded() {
        assert_eq!(metric_method("tools/call"), "tools/call");
        assert_eq!(metric_method("extension/private-value"), "other");
        assert_eq!(
            metric_protocol_version(MCP_PROTOCOL_VERSION),
            MCP_PROTOCOL_VERSION
        );
        assert_eq!(metric_protocol_version("private-version"), "other");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn lifecycle_emits_exact_instruments_and_balances_in_flight() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let telemetry = ServerTelemetry::from_meter(&provider.meter("mcp-test"));

        let mut success = telemetry.start(
            "tools/call",
            "stdio",
            MCP_PROTOCOL_VERSION,
            tracing::Span::none(),
        );
        success.success();
        success.success();
        drop(success);

        let mut error = telemetry.start(
            "extension/private-id",
            "streamable_http",
            "private-version",
            tracing::Span::none(),
        );
        error.response_error_code(Some(-32601));
        drop(error);

        let cancelled = telemetry.start(
            "prompts/get",
            "streamable_http",
            MCP_PROTOCOL_VERSION,
            tracing::Span::none(),
        );
        drop(cancelled);

        provider.force_flush().expect("flush metrics");
        let batches = exporter.get_finished_metrics().expect("export metrics");
        let metrics = batches
            .iter()
            .flat_map(opentelemetry_sdk::metrics::data::ResourceMetrics::scope_metrics)
            .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
            .collect::<Vec<_>>();

        let count = metrics
            .iter()
            .find(|metric| metric.name() == "mcp.server.request.count")
            .expect("request count");
        assert_eq!(count.unit(), "{request}");
        let AggregatedMetrics::U64(MetricData::Sum(count)) = count.data() else {
            panic!("request count must be a u64 sum")
        };
        assert_eq!(
            count
                .data_points()
                .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                .sum::<u64>(),
            3
        );
        let count_attributes = count
            .data_points()
            .flat_map(opentelemetry_sdk::metrics::data::SumDataPoint::attributes)
            .collect::<Vec<_>>();
        assert!(has_attribute(&count_attributes, "mcp.outcome", "success"));
        assert!(has_attribute(&count_attributes, "mcp.outcome", "error"));
        assert!(has_attribute(&count_attributes, "mcp.outcome", "cancelled"));
        assert!(has_attribute(&count_attributes, "rpc.method", "other"));
        assert!(has_attribute(
            &count_attributes,
            "mcp.protocol_version",
            "other"
        ));
        assert!(count_attributes.iter().any(|attribute| {
            attribute.key.as_str() == "rpc.jsonrpc.error_code"
                && attribute.value == opentelemetry::Value::I64(-32601)
        }));
        assert!(
            count_attributes
                .iter()
                .all(|attribute| attribute.key.as_str() != "mcp.request_id")
        );

        let duration = metrics
            .iter()
            .find(|metric| metric.name() == "mcp.server.request.duration")
            .expect("request duration");
        assert_eq!(duration.unit(), "s");
        let AggregatedMetrics::F64(MetricData::Histogram(duration)) = duration.data() else {
            panic!("request duration must be an f64 histogram")
        };
        assert_eq!(
            duration
                .data_points()
                .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
                .sum::<u64>(),
            3
        );

        let in_flight = metrics
            .iter()
            .find(|metric| metric.name() == "mcp.server.request.in_flight")
            .expect("in-flight requests");
        assert_eq!(in_flight.unit(), "{request}");
        let AggregatedMetrics::I64(MetricData::Sum(in_flight)) = in_flight.data() else {
            panic!("in-flight requests must be an i64 sum")
        };
        assert_eq!(
            in_flight
                .data_points()
                .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                .sum::<i64>(),
            0
        );
    }

    fn has_attribute(attributes: &[&KeyValue], key: &str, value: &str) -> bool {
        attributes.iter().any(|attribute| {
            attribute.key.as_str() == key
                && attribute.value == opentelemetry::Value::String(value.to_owned().into())
        })
    }
}
