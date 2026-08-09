//! Runtime observability setup.

use opentelemetry::{global, trace::TracerProvider as _};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::{
    Resource, logs::SdkLoggerProvider, metrics::SdkMeterProvider,
    propagation::TraceContextPropagator, trace::SdkTracerProvider,
};
use std::env;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

const SERVICE_NAME: &str = "waltr-server";
const DEFAULT_FILTER: &str =
    "waltr_server=info,waltr=info,mcp=info,tower_http=info,otel::tracing=info";

/// Holds OpenTelemetry providers until process shutdown.
#[derive(Debug)]
pub struct ObservabilityGuard {
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
    meter: Option<SdkMeterProvider>,
}

impl Drop for ObservabilityGuard {
    fn drop(&mut self) {
        if let Some(tracer) = self.tracer.take()
            && let Err(error) = tracer.shutdown()
        {
            tracing::warn!(%error, "failed to shut down tracer provider");
        }
        if let Some(logger) = self.logger.take()
            && let Err(error) = logger.shutdown()
        {
            tracing::warn!(%error, "failed to shut down logger provider");
        }
        if let Some(meter) = self.meter.take()
            && let Err(error) = meter.shutdown()
        {
            tracing::warn!(%error, "failed to shut down meter provider");
        }
    }
}

/// Initialize JSON logs, tracing, and OpenTelemetry OTLP exporters.
///
/// Exporter configuration is read from the standard OpenTelemetry environment variables.
///
/// # Errors
///
/// Returns an error when an OTLP exporter cannot be built or a tracing subscriber is already set.
pub fn init() -> Result<ObservabilityGuard, Box<dyn std::error::Error + Send + Sync>> {
    if !otel_export_enabled() {
        return init_local();
    }

    global::set_text_map_propagator(TraceContextPropagator::new());

    let resource = Resource::builder().with_service_name(SERVICE_NAME).build();
    let tracer_provider = tracer_provider(resource.clone())?;
    let logger_provider = logger_provider(resource.clone())?;
    let meter_provider = meter_provider(resource)?;

    global::set_tracer_provider(tracer_provider.clone());
    global::set_meter_provider(meter_provider.clone());

    let tracer = tracer_provider.tracer(SERVICE_NAME);
    let telemetry_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let otel_log_layer = OpenTelemetryTracingBridge::new(&logger_provider);
    let fmt_layer = tracing_subscriber::fmt::layer().json();
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    tracing_subscriber::registry()
        .with(filter)
        .with(telemetry_layer)
        .with(otel_log_layer)
        .with(fmt_layer)
        .try_init()?;

    tracing::info!(service.name = SERVICE_NAME, "observability initialized");

    Ok(ObservabilityGuard {
        tracer: Some(tracer_provider),
        logger: Some(logger_provider),
        meter: Some(meter_provider),
    })
}

fn init_local() -> Result<ObservabilityGuard, Box<dyn std::error::Error + Send + Sync>> {
    let fmt_layer = tracing_subscriber::fmt::layer().json();
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .try_init()?;

    tracing::info!(service.name = SERVICE_NAME, "observability initialized");

    Ok(ObservabilityGuard {
        tracer: None,
        logger: None,
        meter: None,
    })
}

fn otel_export_enabled() -> bool {
    env::var("WALTR_OTEL_EXPORT").is_ok_and(|value| value == "1" || value == "true")
        || env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
}

fn tracer_provider(
    resource: Resource,
) -> Result<SdkTracerProvider, opentelemetry_otlp::ExporterBuildError> {
    let exporter = opentelemetry_otlp::SpanExporter::builder().build()?;
    Ok(SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build())
}

fn logger_provider(
    resource: Resource,
) -> Result<SdkLoggerProvider, opentelemetry_otlp::ExporterBuildError> {
    let exporter = opentelemetry_otlp::LogExporter::builder().build()?;
    Ok(SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build())
}

fn meter_provider(
    resource: Resource,
) -> Result<SdkMeterProvider, opentelemetry_otlp::ExporterBuildError> {
    let exporter = opentelemetry_otlp::MetricExporter::builder().build()?;
    Ok(SdkMeterProvider::builder()
        .with_resource(resource)
        .with_periodic_exporter(exporter)
        .build())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing::Subscriber;
    use tracing_subscriber::layer::Context;

    use super::*;

    #[test]
    fn default_filter_enables_generic_mcp_info_spans() {
        let targets = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new(DEFAULT_FILTER))
            .with(TargetCapture(targets.clone()));

        tracing::subscriber::with_default(subscriber, || {
            let _mcp = tracing::info_span!(target: "mcp::client", "mcp.client.request");
            let _dependency = tracing::info_span!(target: "reqwest", "dependency.request");
        });

        assert_eq!(*targets.lock().unwrap(), ["mcp::client"]);
    }

    struct TargetCapture(Arc<Mutex<Vec<&'static str>>>);

    impl<S: Subscriber> tracing_subscriber::Layer<S> for TargetCapture {
        fn on_new_span(
            &self,
            attributes: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _context: Context<'_, S>,
        ) {
            self.0.lock().unwrap().push(attributes.metadata().target());
        }
    }
}
