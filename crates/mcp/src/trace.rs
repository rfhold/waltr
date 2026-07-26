use opentelemetry::{
    Context,
    trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState},
};
use serde_json::{Map, Value};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// MCP request-level trace propagation mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum McpTracePropagation {
    /// Do not inject trace context into MCP request params.
    #[default]
    Disabled,
    /// Inject W3C trace context into `params._meta.traceparent` and `params._meta.tracestate`.
    TraceContext,
}

pub fn inject_request_context(
    params: Option<Value>,
    propagation: McpTracePropagation,
) -> Option<Value> {
    if propagation == McpTracePropagation::Disabled {
        return params;
    }

    let Some(context) = current_trace_context() else {
        return params;
    };

    Some(merge_trace_context(params, &context))
}

pub fn request_meta(params: Option<&Value>) -> Option<&Map<String, Value>> {
    params
        .and_then(Value::as_object)
        .and_then(|params| params.get("_meta"))
        .and_then(Value::as_object)
}

pub fn request_method(message: &Value) -> Option<&str> {
    message.get("method").and_then(Value::as_str)
}

pub fn request_id(message: &Value) -> Option<String> {
    message.get("id").and_then(safe_id)
}

pub fn safe_id(id: &Value) -> Option<String> {
    match id {
        Value::String(id) => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

pub const fn trace_propagation_name(propagation: McpTracePropagation) -> &'static str {
    match propagation {
        McpTracePropagation::Disabled => "disabled",
        McpTracePropagation::TraceContext => "tracecontext",
    }
}

pub fn set_span_parent_from_meta(span: &Span, meta: Option<&Map<String, Value>>) {
    if let Some(parent) = extract_parent_context(meta) {
        drop(span.set_parent(parent));
    }
}

fn current_trace_context() -> Option<Map<String, Value>> {
    let context = Span::current().context();
    let span_context = context.span().span_context().clone();
    if !span_context.is_valid() {
        return None;
    }

    let mut meta = Map::new();
    meta.insert(
        "traceparent".to_owned(),
        Value::String(format!(
            "00-{}-{}-{:02x}",
            span_context.trace_id(),
            span_context.span_id(),
            span_context.trace_flags().to_u8()
        )),
    );
    let tracestate = span_context.trace_state().header();
    if !tracestate.is_empty() {
        meta.insert("tracestate".to_owned(), Value::String(tracestate));
    }
    Some(meta)
}

fn extract_parent_context(meta: Option<&Map<String, Value>>) -> Option<Context> {
    let meta = meta?;
    let traceparent = meta.get("traceparent")?.as_str()?;
    let tracestate = meta.get("tracestate").and_then(Value::as_str);
    let span_context = parse_traceparent(traceparent, tracestate)?;
    Some(Context::new().with_remote_span_context(span_context))
}

fn parse_traceparent(traceparent: &str, tracestate: Option<&str>) -> Option<SpanContext> {
    let mut parts = traceparent.split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let span_id = parts.next()?;
    let flags = parts.next()?;
    if parts.next().is_some()
        || version.len() != 2
        || trace_id.len() != 32
        || span_id.len() != 16
        || flags.len() != 2
    {
        return None;
    }
    let trace_id = TraceId::from_hex(trace_id).ok()?;
    let span_id = SpanId::from_hex(span_id).ok()?;
    if trace_id == TraceId::INVALID || span_id == SpanId::INVALID {
        return None;
    }
    let trace_flags = TraceFlags::new(u8::from_str_radix(flags, 16).ok()?);
    let parsed_state = tracestate
        .and_then(|state| state.parse::<TraceState>().ok())
        .unwrap_or_default();
    let span_context = SpanContext::new(trace_id, span_id, trace_flags, true, parsed_state);
    span_context.is_valid().then_some(span_context)
}

fn merge_trace_context(params: Option<Value>, context: &Map<String, Value>) -> Value {
    let mut params = params.unwrap_or_else(|| Value::Object(Map::new()));
    let Some(params_object) = params.as_object_mut() else {
        return params;
    };

    match params_object.get_mut("_meta") {
        Some(Value::Object(meta)) => {
            for (key, value) in context {
                meta.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        Some(_) => {}
        None => {
            params_object.insert("_meta".to_owned(), Value::Object(context.clone()));
        }
    }

    params
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use opentelemetry::trace::TraceContextExt;

    use super::{
        McpTracePropagation, extract_parent_context, inject_request_context, merge_trace_context,
    };

    #[test]
    fn disabled_propagation_does_not_add_traceparent() {
        let params = Some(json!({ "name": "test" }));

        let injected = inject_request_context(params, McpTracePropagation::Disabled);

        assert_eq!(injected, Some(json!({ "name": "test" })));
    }

    #[test]
    fn progress_token_survives_trace_injection() {
        let context = serde_json::Map::from_iter([(
            "traceparent".to_owned(),
            json!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        )]);
        let params = Some(json!({
            "name": "test",
            "arguments": {},
            "_meta": { "progressToken": 7, "other": true }
        }));

        let injected = merge_trace_context(params, &context);

        assert_eq!(injected["_meta"]["progressToken"], 7);
        assert_eq!(injected["_meta"]["other"], true);
        assert_eq!(
            injected["_meta"]["traceparent"],
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
    }

    #[test]
    fn extracts_server_parent_from_request_meta_traceparent() {
        let meta = serde_json::Map::from_iter([
            (
                "traceparent".to_owned(),
                json!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            ),
            ("tracestate".to_owned(), json!("foo=bar")),
        ]);

        let context = extract_parent_context(Some(&meta)).expect("traceparent extracts");
        let span_context = context.span().span_context().clone();

        assert!(span_context.is_remote());
        assert_eq!(
            span_context.trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(span_context.span_id().to_string(), "00f067aa0ba902b7");
        assert_eq!(span_context.trace_state().header(), "foo=bar");
    }
}
