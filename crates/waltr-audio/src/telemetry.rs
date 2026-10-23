//! Content-free speech RPC lifetime and traceparent-only propagation.
use crate::error::Error;
use opentelemetry::trace::TraceContextExt as _;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

#[derive(Clone, Copy)]
pub(crate) enum Rpc {
    Recognize,
    StreamingRecognize,
    Synthesize,
    StreamingSynthesize,
}

pub(crate) struct Operation {
    pub(crate) span: tracing::Span,
    finished: AtomicBool,
}
impl Drop for Operation {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Relaxed) {
            self.span.record("outcome", "incomplete");
        }
    }
}
impl Operation {
    pub(crate) fn new(rpc: Rpc) -> Self {
        let span = match rpc {
            Rpc::Recognize => {
                tracing::info_span!(target: "waltr_audio::telemetry", "speech.recognize", outcome = tracing::field::Empty, grpc_status = tracing::field::Empty)
            }
            Rpc::StreamingRecognize => {
                tracing::info_span!(target: "waltr_audio::telemetry", "speech.streaming_recognize", outcome = tracing::field::Empty, grpc_status = tracing::field::Empty)
            }
            Rpc::Synthesize => {
                tracing::info_span!(target: "waltr_audio::telemetry", "speech.synthesize", outcome = tracing::field::Empty, grpc_status = tracing::field::Empty)
            }
            Rpc::StreamingSynthesize => {
                tracing::info_span!(target: "waltr_audio::telemetry", "speech.streaming_synthesize", outcome = tracing::field::Empty, grpc_status = tracing::field::Empty)
            }
        };
        Self {
            span,
            finished: AtomicBool::new(false),
        }
    }

    pub(crate) fn inject<T>(&self, request: &mut tonic::Request<T>) {
        let context = self.span.context();
        let span = context.span();
        let parent = span.span_context();
        if parent.is_valid() {
            let value = format!(
                "00-{}-{}-{:02x}",
                parent.trace_id(),
                parent.span_id(),
                parent.trace_flags().to_u8() & 1
            );
            if let Ok(value) = value.parse() {
                request.metadata_mut().insert("traceparent", value);
            }
        }
    }

    pub(crate) fn result<T>(&self, result: &Result<T, Error>) {
        let (outcome, status) = match result {
            Ok(_) => ("success", Some(0)),
            Err(Error::Status(status)) => ("failure", Some(status.code() as i32)),
            Err(Error::Cancelled) => ("cancelled", None),
            Err(Error::Deadline) => ("deadline", None),
            Err(Error::Invalid(_)) => ("invalid", None),
            Err(Error::Transport(_)) => ("failure", None),
        };
        self.finished.store(true, Ordering::Relaxed);
        self.span.record("outcome", outcome);
        if let Some(status) = status {
            self.span.record("grpc_status", status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_tracing_does_not_inject_an_invalid_parent() {
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            let operation = Operation::new(Rpc::Recognize);
            let mut request = tonic::Request::new(());
            operation.inject(&mut request);
            assert!(request.metadata().is_empty());
        });
    }
}
