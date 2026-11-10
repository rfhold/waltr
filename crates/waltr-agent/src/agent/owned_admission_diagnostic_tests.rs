use super::*;
use crate::*;
use std::{future::Future, io::Write, sync::Mutex as StdMutex, time::Duration};

const SECRET: &str = "sentinel-token-transcript-https://private.invalid/?token=secret";

#[derive(Clone, Default)]
struct LogBuffer(Arc<StdMutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture(action: impl Future<Output = ()>) -> String {
    let buffer = LogBuffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), action)
                    .await
                    .unwrap()
            });
    });
    let bytes = buffer.0.lock().unwrap().clone();
    let logs = String::from_utf8(bytes).unwrap();
    assert!(!logs.contains(SECRET), "{logs}");
    logs
}

fn assert_log(logs: &str, id: &SessionId, stage: &str, category: &str, cause: &str) {
    assert!(logs.contains("WARN"), "{logs}");
    for field in [
        format!("session_id=\"{}\"", id.as_str()),
        format!("stage=\"{stage}\""),
        format!("category=\"{category}\""),
        format!("cause=\"{cause}\""),
    ] {
        assert!(logs.contains(&field), "missing {field}: {logs}");
    }
}

struct Unused;
impl LlmStream for Unused {
    fn name(&self) -> &'static str {
        "unused"
    }
    fn stream_request<'a>(&'a self, _: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        panic!("admission fixture must not request inference")
    }
}

fn runtime() -> AgentRuntime {
    AgentRuntime::builder(
        ModelRef::new(ProviderId::OpenAi, SECRET, ModelApi::OpenAiResponses),
        Arc::new(Unused),
    )
    .build()
}

struct Observer;
impl CompletionObserver for Observer {
    fn observe(&self, _: CompletionProposal, _: CancellationToken) -> CompletionObserverFuture {
        panic!("admission fixture must not validate completion")
    }
}

fn validation() -> CompletionValidation {
    CompletionValidation::new(Arc::new(Observer), 1, Duration::from_secs(2)).unwrap()
}

#[test]
fn transient_binding_failure_emits_safe_category_and_keeps_unavailable() {
    let id = SessionId::generate();
    let logs = capture(async {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let owner = SessionExecutionOwner::new(sessions, id.clone());
        let budget = TransientInputBudget::process();
        let attachment = budget
            .attachment(TransientInputKind::Utf8, SECRET.as_bytes().to_vec())
            .unwrap();
        let input = TransientInput::new(
            &budget,
            TransientPromptTarget {
                cursor: TimelineCursor::default(),
                turn: 0,
                prompt_index: 0,
            },
            TransientModelCapabilities {
                text: true,
                pdf: false,
                png: false,
                jpeg: false,
                webp: false,
                max_body_bytes: 1024,
            },
            vec![attachment],
        )
        .unwrap();
        assert_eq!(
            owner
                .helper_with_transient_input(
                    runtime(),
                    input,
                    validation(),
                    AgentRunLimits::default(),
                    CancellationToken::new()
                )
                .await,
            Err(OwnedExecutionError::Unavailable)
        );
        assert_eq!(owner.close().await, Ok(()));
    });
    assert_log(&logs, &id, "transient_bind", "protocol", "none");
}

#[test]
fn submission_failures_emit_typed_causes_without_raw_messages() {
    let mut cases = vec![
        (
            AgentSessionRunError::Runtime(AgentSessionRuntimeError::FailedPrecondition(
                SECRET.into(),
            )),
            "runtime",
            "runtime_precondition",
        ),
        (
            AgentSessionRunError::Runtime(AgentSessionRuntimeError::Internal(SECRET.into())),
            "runtime",
            "runtime_internal",
        ),
        (
            AgentSessionRunError::Runtime(AgentSessionRuntimeError::Agent(LlmError::api(
                SECRET, SECRET,
            ))),
            "runtime",
            "provider_api",
        ),
        (
            AgentSessionRunError::Agent(LlmError::Protocol {
                provider: SECRET,
                message: SECRET.into(),
            }),
            "agent",
            "protocol",
        ),
    ];
    let unrelated_id = SessionId::generate();
    cases.extend([
        (
            AgentSessionRunError::AlreadyRunning {
                session_id: unrelated_id.clone(),
            },
            "already_running",
            "none",
        ),
        (
            AgentSessionRunError::Paused {
                session_id: unrelated_id.clone(),
            },
            "paused",
            "none",
        ),
        (
            AgentSessionRunError::Unavailable {
                session_id: unrelated_id.clone(),
            },
            "unavailable",
            "none",
        ),
        (
            AgentSessionRunError::Canceled {
                session_id: unrelated_id.clone(),
                run_id: 7,
                reason: SECRET.into(),
            },
            "canceled",
            "none",
        ),
        (
            AgentSessionRunError::OutcomeDropped {
                session_id: unrelated_id,
                run_id: 7,
            },
            "outcome_dropped",
            "none",
        ),
    ]);
    for (error, category) in [
        (LlmError::MissingConfig { name: SECRET }, "missing_config"),
        (LlmError::Io(std::io::Error::other(SECRET)), "io"),
        (
            LlmError::Authorization {
                provider: SECRET,
                message: SECRET.into(),
                status: Some(401),
            },
            "authorization",
        ),
        (
            LlmError::Api {
                provider: SECRET,
                message: SECRET.into(),
                status: Some(500),
                headers: vec![(SECRET.into(), SECRET.into())],
                code: Some(SECRET.into()),
                body: Some(SECRET.into()),
            },
            "provider_api",
        ),
        (
            LlmError::AuthenticationFailure {
                failure: AuthenticationFailure {
                    target: AuthTarget::Host {
                        namespace: SECRET.into(),
                        key: SECRET.into(),
                    },
                    reason: AuthenticationFailureReason::CredentialRejected,
                    host_context: Some(serde_json::json!({"secret": SECRET})),
                },
                message: SECRET.into(),
            },
            "auth_credential_rejected",
        ),
    ] {
        cases.push((AgentSessionRunError::Agent(error), "agent", category));
    }
    for (reason, category) in [
        (
            AuthenticationFailureReason::CredentialRequired,
            "auth_credential_required",
        ),
        (
            AuthenticationFailureReason::CredentialRejected,
            "auth_credential_rejected",
        ),
        (
            AuthenticationFailureReason::TokenExchangeFailed,
            "auth_token_exchange_failed",
        ),
        (
            AuthenticationFailureReason::NetworkFailed,
            "auth_network_failed",
        ),
        (
            AuthenticationFailureReason::ResponseInvalid,
            "auth_response_invalid",
        ),
        (
            AuthenticationFailureReason::ConfigurationRequired,
            "auth_configuration_required",
        ),
    ] {
        cases.push((
            AgentSessionRunError::Runtime(AgentSessionRuntimeError::Authentication {
                target: AuthTarget::Host {
                    namespace: SECRET.into(),
                    key: SECRET.into(),
                },
                message: SECRET.into(),
                reason,
                host_context: Some(serde_json::json!({"secret": SECRET})),
            }),
            "runtime",
            category,
        ));
    }
    for (error, category, cause) in cases {
        let id = SessionId::generate();
        let logs = capture(async {
            let owner = SessionExecutionOwner::new(
                SessionCoordinator::new(MemorySessionStore::new()),
                id.clone(),
            );
            assert_eq!(
                owner
                    .start(
                        Box::pin(async move { Err(error) }),
                        CancellationToken::new()
                    )
                    .await,
                Err(OwnedExecutionError::Unavailable)
            );
            assert_eq!(owner.close().await, Err(OwnedExecutionError::Unavailable));
        });
        assert_log(&logs, &id, "submit", category, cause);
    }
}

#[test]
fn owner_state_failures_and_driver_rejection_do_not_poll_submission() {
    for category in [
        "owner_closed",
        "owner_closing",
        "request_canceled",
        "owner_busy",
        "driver_rejected",
    ] {
        let id = SessionId::generate();
        let logs = capture(async {
            let sessions = SessionCoordinator::new(MemorySessionStore::new());
            let owner = SessionExecutionOwner::new(sessions.clone(), id.clone());
            let cancel = CancellationToken::new();
            match category {
                "owner_closed" => {
                    owner.close().await.unwrap();
                }
                "owner_closing" => owner.closing.cancel(),
                "request_canceled" => cancel.cancel(),
                "owner_busy" => {
                    owner.state.lock().await.wait = Some(std::future::pending().boxed().shared())
                }
                "driver_rejected" => sessions.shutdown().await,
                _ => unreachable!(),
            }
            assert_eq!(
                owner
                    .start(
                        Box::pin(async { panic!("rejected submission polled") }),
                        cancel
                    )
                    .await,
                Err(OwnedExecutionError::Unavailable)
            );
        });
        assert_log(
            &logs,
            &id,
            if category == "driver_rejected" {
                "drive"
            } else {
                "owner_state"
            },
            category,
            "none",
        );
    }
}

#[test]
fn real_pinned_helper_admission_failure_is_logged() {
    let id = SessionId::generate();
    let logs = capture(async {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let owner = SessionExecutionOwner::new(sessions, id.clone());
        assert_eq!(
            owner
                .helper(
                    runtime(),
                    validation(),
                    AgentRunLimits::default(),
                    CancellationToken::new()
                )
                .await,
            Err(OwnedExecutionError::Unavailable)
        );
        assert_eq!(owner.close().await, Err(OwnedExecutionError::Unavailable));
    });
    assert_log(&logs, &id, "submit", "agent", "protocol");
}
