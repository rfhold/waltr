use axum::{Json, extract::State, response::IntoResponse, routing::post};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;
use waltr_agent::LlmStream;

#[derive(Default)]
pub(super) struct Counts {
    pub cleanup: AtomicUsize,
    pub filter: AtomicUsize,
    pub notes: AtomicUsize,
    pub invalid_filter: AtomicBool,
    pub plain_cleanup: AtomicBool,
}
#[derive(Clone)]
struct ModelState {
    counts: Arc<Counts>,
    speech: Arc<super::super::media_upstream::Metrics>,
}
pub(super) struct Models {
    counts: Arc<Counts>,
    provider: waltr_agent::provider::openai::OpenAiResponses,
    stop: CancellationToken,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl std::ops::Deref for Models {
    type Target = Counts;
    fn deref(&self) -> &Counts {
        &self.counts
    }
}
impl Models {
    pub async fn start(speech: Arc<super::super::media_upstream::Metrics>) -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let counts = Arc::new(Counts::default());
        let stop = CancellationToken::new();
        let end = stop.clone();
        let state = counts.clone();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new()
                    .route("/responses", post(respond))
                    .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
                    .with_state(ModelState {
                        counts: state,
                        speech,
                    }),
            )
            .with_graceful_shutdown(end.cancelled_owned())
            .await
            .unwrap();
        });
        Arc::new(Self {
            counts,
            provider: waltr_agent::provider::openai::OpenAiResponses::new("media-fixture")
                .with_url(format!("http://{address}/responses")),
            stop,
            task: tokio::sync::Mutex::new(Some(task)),
        })
    }
    pub async fn shutdown(&self) {
        self.stop.cancel();
        if let Some(task) = self.task.lock().await.take() {
            task.await.unwrap();
        }
    }
}
async fn respond(State(state): State<ModelState>, Json(body): Json<Value>) -> impl IntoResponse {
    let counts = state.counts;
    assert_eq!(
        state.speech.live.load(Ordering::SeqCst),
        0,
        "helper must follow speech EOF and reader cleanup"
    );
    let system = body["instructions"].as_str().unwrap();
    let input = body["input"].to_string();
    assert!(input.len() < 1024 * 1024);
    let captured = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["type"] == "function_call_output");
    let events = if system.contains("retained_transcript:") && !captured {
        counts.filter.fetch_add(1, Ordering::SeqCst);
        assert!(input.contains("Committed conversation"));
        assert!(input.contains("voice-1: Committed conversation"));
        assert!(!input.contains("first hypothesis"));
        assert!(!input.contains("revised hypothesis"));
        assert!(
            body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "_helper_capture")
        );
        let capture = if counts.invalid_filter.load(Ordering::SeqCst) {
            json!({"retained_transcript":"INVALID_MISSING_DELETION_AUDIT"})
        } else {
            json!({"retained_transcript":"RETAINED_SUBSTANTIVE","deleted_items":"REMOVED_INCIDENTAL: irrelevant noise"})
        };
        vec![
            json!({"type":"response.output_item.added","item":{"id":"filter-item","call_id":"actual-filter-capture","type":"function_call","name":"_helper_capture","arguments":""}}),
            json!({"type":"response.function_call_arguments.done","item_id":"filter-item","arguments":capture.to_string()}),
        ]
    } else if captured {
        vec![json!({"type":"response.output_text.delta","delta":"Accepted captured output."})]
    } else {
        let text = if system.contains("Clean and format") {
            counts.cleanup.fetch_add(1, Ordering::SeqCst);
            assert!(input.contains("Committed conversation"));
            assert_eq!(input.contains("voice-1"), !counts.plain_cleanup.load(Ordering::SeqCst));
            assert!(!input.contains("first hypothesis"));
            assert!(!input.contains("revised hypothesis"));
            "Clean final transcript."
        } else {
            counts.notes.fetch_add(1, Ordering::SeqCst);
            assert!(input.contains("RETAINED_SUBSTANTIVE"));
            assert!(input.contains("REMOVED_INCIDENTAL"));
            assert!(!input.contains("Committed conversation"));
            "Meeting notes: substantive decision."
        };
        let value = json!({"chosen":text});
        assert!(body["tools"].as_array().unwrap().iter().any(|tool| tool["name"] == "_helper_capture"));
        vec![
            json!({"type":"response.output_item.added","item":{"id":"output-item","call_id":"actual-output-capture","type":"function_call","name":"_helper_capture","arguments":""}}),
            json!({"type":"response.function_call_arguments.done","item_id":"output-item","arguments":value.to_string()}),
        ]
    };
    let mut data = String::new();
    for event in events.into_iter().chain([
        json!({"type":"response.completed","response":{"id":"media-response","output":[]}}),
    ]) {
        data.push_str(&format!("data: {event}\n\n"));
    }
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        data,
    )
}
impl LlmStream for Models {
    fn name(&self) -> &'static str {
        "real-openai-responses-fixture"
    }
    fn transient_api(&self) -> Option<waltr_agent::TransientInputApi> {
        self.provider.transient_api()
    }
    fn preflight_transient_request(
        &self,
        request: &waltr_agent::CompletionRequest<'_>,
        input: &waltr_agent::AdmittedTransientInput,
    ) -> waltr_agent::Result<()> {
        self.provider.preflight_transient_request(request, input)
    }
    fn stream_transient_request<'a>(
        &'a self,
        request: waltr_agent::CompletionRequest<'a>,
        input: &'a waltr_agent::AdmittedTransientInput,
    ) -> waltr_agent::BoxStreamFuture<'a> {
        self.provider.stream_transient_request(request, input)
    }
    fn stream_request<'a>(
        &'a self,
        request: waltr_agent::CompletionRequest<'a>,
    ) -> waltr_agent::BoxStreamFuture<'a> {
        self.provider.stream_request(request)
    }
}
