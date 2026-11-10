use super::*;
use crate::{
    AdmittedTransientInput, AuthenticationFailureReason, PromptTurn, TimelineCursor,
    TransientInput, TransientInputApi, TransientInputBudget, TransientInputKind,
    TransientModelCapabilities, TransientPromptTarget,
};
use std::sync::atomic::{AtomicUsize, Ordering};

const PAYLOAD: &str = "sentinel exact UTF-8 é\n\"input\"";
const KEY: &str = "configured-transient-provider";

fn input(budget: &TransientInputBudget) -> TransientInput {
    TransientInput::new(
        budget,
        TransientPromptTarget {
            cursor: TimelineCursor::new(7),
            turn: 3,
            prompt_index: 0,
        },
        TransientModelCapabilities {
            png: false,
            jpeg: false,
            webp: false,
            pdf: false,
            text: true,
            max_body_bytes: 4096,
        },
        vec![
            budget
                .attachment(TransientInputKind::Utf8, PAYLOAD.as_bytes().to_vec())
                .unwrap(),
        ],
    )
    .unwrap()
}

fn request() -> CompletionRequest<'static> {
    CompletionRequest {
        prompt_turns: vec![PromptTurn {
            turn: 3,
            prompt: "task".into(),
        }],
        ..CompletionRequest::new("task")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Preflight,
    Start,
    Item,
}

struct FakeProvider {
    failure: Option<(Stage, u16)>,
    calls: Arc<AtomicUsize>,
}

fn api_error(status: u16) -> LlmError {
    LlmError::Api {
        provider: "fake-transient",
        status: Some(status),
        message: "private response".into(),
        headers: vec![("x-private".into(), "private header".into())],
        code: Some("private code".into()),
        body: Some("private body".into()),
    }
}

impl FakeProvider {
    fn check(&self, stage: Stage) -> crate::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.failure {
            Some((failed, status)) if failed == stage => Err(api_error(status)),
            _ => Ok(()),
        }
    }
    fn assert_input(request: &CompletionRequest<'_>, input: &AdmittedTransientInput) {
        input.input().validate_request(request).unwrap();
        assert_eq!(input.input().target().cursor, TimelineCursor::new(7));
        assert_eq!(input.input().target().turn, 3);
        assert_eq!(input.input().target().prompt_index, 0);
        assert_eq!(
            input.input().content(TransientInputApi::OpenAiResponses),
            vec![serde_json::json!({"type": "input_text", "text": PAYLOAD})]
        );
    }
}

impl LlmStream for FakeProvider {
    fn name(&self) -> &'static str {
        "fake-transient"
    }
    fn transient_api(&self) -> Option<TransientInputApi> {
        Some(TransientInputApi::OpenAiResponses)
    }
    fn project_request(&self, _: &CompletionRequest<'_>) -> Option<serde_json::Value> {
        Some(serde_json::json!({"ordinary": true}))
    }
    fn preflight_transient_request(
        &self,
        request: &CompletionRequest<'_>,
        input: &AdmittedTransientInput,
    ) -> crate::Result<()> {
        Self::assert_input(request, input);
        self.check(Stage::Preflight)
    }
    fn stream_transient_request<'a>(
        &'a self,
        request: CompletionRequest<'a>,
        input: &'a AdmittedTransientInput,
    ) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            Self::assert_input(&request, input);
            self.check(Stage::Start)?;
            Ok(Box::pin(futures_util::stream::iter([self
                .check(Stage::Item)
                .map(|()| crate::CompletionEvent::Finished)]))
                as BoxCompletionStream)
        })
    }
    fn stream_request<'a>(&'a self, _: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            self.check(Stage::Start)?;
            Ok(Box::pin(futures_util::stream::iter([self
                .check(Stage::Item)
                .map(|()| crate::CompletionEvent::Finished)]))
                as BoxCompletionStream)
        })
    }
}

fn wrapped(failure: Option<(Stage, u16)>, calls: Arc<AtomicUsize>) -> Arc<dyn LlmStream> {
    Arc::new(TargetedProvider::new(FakeProvider { failure, calls }, KEY))
}

fn assert_rejected(error: LlmError) {
    assert!(!error.to_string().contains("private"));
    assert!(matches!(error, LlmError::AuthenticationFailure {
        failure: AuthenticationFailure { target, reason: AuthenticationFailureReason::CredentialRejected, host_context: None }, ..
    } if target == AuthTarget::provider(KEY)));
}

#[tokio::test]
async fn targeted_transient_bind_preflight_stream_and_credit_lifetime() {
    let budget = TransientInputBudget::isolated_for_test();
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = wrapped(None, calls.clone());
    assert_eq!(provider.name(), "fake-transient");
    assert_eq!(
        provider.project_request(&request()),
        Some(serde_json::json!({"ordinary": true}))
    );
    let bound = crate::transient_input::bind_provider(provider, input(&budget)).unwrap();
    let projection = bound.project_request(&request()).unwrap();
    assert_eq!(projection["projection"], "metadataOnly");
    assert_eq!(projection["transientInput"]["replayable"], false);
    assert!(!projection.to_string().contains("sentinel"));
    let mut stream = bound.stream_request(request()).await.unwrap();
    assert!(matches!(
        stream.next().await.unwrap().unwrap(),
        crate::CompletionEvent::Finished
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(budget.used_for_test().1, 1);
    drop(stream);
    assert_eq!(budget.used_for_test().1, 0);
    drop(bound);
    assert_eq!(budget.used_for_test(), (0, 0));
}

#[tokio::test]
async fn targeted_transient_401_maps_at_preflight_start_and_stream_item() {
    for stage in [Stage::Preflight, Stage::Start, Stage::Item] {
        let budget = TransientInputBudget::isolated_for_test();
        let calls = Arc::new(AtomicUsize::new(0));
        let bound = crate::transient_input::bind_provider(
            wrapped(Some((stage, 401)), calls.clone()),
            input(&budget),
        )
        .unwrap();
        let error = match bound.stream_request(request()).await {
            Err(error) => error,
            Ok(mut stream) => stream.next().await.unwrap().unwrap_err(),
        };
        assert_rejected(error);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            match stage {
                Stage::Preflight => 1,
                Stage::Start => 2,
                Stage::Item => 3,
            }
        );
        assert_eq!(budget.used_for_test().1, 0);
    }
}

#[tokio::test]
async fn targeted_transient_non_401_retains_category_provider_and_status() {
    for stage in [Stage::Preflight, Stage::Start, Stage::Item] {
        let budget = TransientInputBudget::isolated_for_test();
        let bound = crate::transient_input::bind_provider(
            wrapped(Some((stage, 429)), Arc::new(AtomicUsize::new(0))),
            input(&budget),
        )
        .unwrap();
        let error = match bound.stream_request(request()).await {
            Err(error) => error,
            Ok(mut stream) => stream.next().await.unwrap().unwrap_err(),
        };
        assert!(matches!(
            error,
            LlmError::Api {
                provider: "fake-transient",
                status: Some(429),
                ..
            }
        ));
    }
}

struct Unsupported;
impl LlmStream for Unsupported {
    fn name(&self) -> &'static str {
        "unsupported"
    }
    fn stream_request<'a>(&'a self, _: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        panic!("unsupported provider must not dispatch")
    }
}

#[test]
fn targeted_transient_unsupported_and_already_bound_fail_closed() {
    let budget = TransientInputBudget::isolated_for_test();
    let unsupported = Arc::new(TargetedProvider::new(Unsupported, KEY));
    assert!(crate::transient_input::bind_provider(unsupported, input(&budget)).is_err());
    let bound = crate::transient_input::bind_provider(
        wrapped(None, Arc::new(AtomicUsize::new(0))),
        input(&budget),
    )
    .unwrap();
    let targeted = Arc::new(TargetedProvider::new(bound, KEY));
    assert_eq!(
        targeted.transient_input().unwrap().target().cursor,
        TimelineCursor::new(7)
    );
    assert!(crate::transient_input::bind_provider(targeted, input(&budget)).is_err());
}

#[tokio::test]
async fn targeted_transient_ordinary_auth_mapping_and_non_401_unchanged() {
    for stage in [Stage::Start, Stage::Item] {
        for status in [401, 429] {
            let provider = wrapped(Some((stage, status)), Arc::new(AtomicUsize::new(0)));
            let error = match provider.stream_request(request()).await {
                Err(error) => error,
                Ok(mut stream) => stream.next().await.unwrap().unwrap_err(),
            };
            if status == 401 {
                assert_rejected(error);
            } else {
                assert!(
                    matches!(error, LlmError::Api { status: Some(429), message, headers, code: Some(code), body: Some(body), .. }
                    if message == "private response" && headers == vec![("x-private".into(), "private header".into())] && code == "private code" && body == "private body")
                );
            }
        }
    }
}

#[tokio::test]
async fn targeted_transient_normal_factory_loopback_sends_exact_utf8() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Arc::new(std::sync::Mutex::new(Some(send)));
    let router = axum::Router::new().route(
        "/",
        axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let send = send.clone();
            async move {
                send.lock().unwrap().take().unwrap().send(body).unwrap();
                ([("content-type", "text/event-stream")], "data: [DONE]\n\n")
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let factory = AgentSessionRuntimeFactory::new(crate::FileCredentialStore::new(
        directory.path().join("unused-auth.json"),
    ));
    let provider = factory
        .provider_for_settings(&InferenceSettings {
            provider: InferenceProviderSettings {
                key: KEY.into(),
                api: ModelApi::OpenAiResponses,
                url: format!("http://{address}/"),
                model_id: "fake-model".into(),
                model_cost: None,
                headers: BTreeMap::from([("authorization".into(), "Bearer fake-sentinel".into())]),
                oauth: None,
                reasoning_effort: None,
            },
        })
        .await
        .unwrap();
    let budget = TransientInputBudget::isolated_for_test();
    let bound = crate::transient_input::bind_provider(provider, input(&budget)).unwrap();
    let stream = bound.stream_request(request()).await.unwrap();
    let body = receive.await.unwrap();
    assert_eq!(
        body["input"][0]["content"][0],
        serde_json::json!({"type": "input_text", "text": PAYLOAD})
    );
    assert_eq!(body["input"][0]["content"][1]["text"], "task");
    assert_eq!(body["store"], false);
    drop(stream);
    server.abort();
    drop(server.await);
}
