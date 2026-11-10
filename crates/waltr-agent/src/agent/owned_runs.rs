//! Invocation-scoped ownership of coordinator runs, including dropped request futures.

use crate::{
    AgentRunLimits, AgentRuntime, AgentSessionRunOutcome, AgentSessionRunSubmission,
    CompletionValidation, SessionCoordinator, SessionId, SessionStore,
};

/// Failure of an invocation-owned run or its joined cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnedExecutionError {
    Unavailable,
    Uncertain,
}
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use std::{pin::Pin, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

type RunWait = Shared<BoxFuture<'static, Result<AgentSessionRunOutcome, OwnedExecutionError>>>;
#[cfg(test)]
use crate as agent;
#[cfg(test)]
#[path = "owned_admission_diagnostic_tests.rs"]
mod admission_diagnostic_tests;
#[cfg(test)]
#[path = "../../tests/support/barrier_store.rs"]
mod barrier_store;

fn auth_category(reason: crate::AuthenticationFailureReason) -> &'static str {
    use crate::AuthenticationFailureReason::*;
    match reason {
        CredentialRequired => "auth_credential_required",
        CredentialRejected => "auth_credential_rejected",
        TokenExchangeFailed => "auth_token_exchange_failed",
        NetworkFailed => "auth_network_failed",
        ResponseInvalid => "auth_response_invalid",
        ConfigurationRequired => "auth_configuration_required",
    }
}

fn llm_category(error: &crate::LlmError) -> &'static str {
    use crate::LlmError::*;
    match error {
        MissingConfig { .. } => "missing_config",
        Http(_) => "http",
        WebSocket(_) => "websocket",
        Request(_) => "request",
        Json(_) => "json",
        Io(_) => "io",
        Api { .. } => "provider_api",
        Protocol { .. } => "protocol",
        Authorization { .. } => "authorization",
        AuthenticationFailure { failure, .. } => auth_category(failure.reason),
    }
}

fn submission_category(error: &crate::AgentSessionRunError) -> (&'static str, &'static str) {
    use crate::{AgentSessionRunError::*, AgentSessionRuntimeError};
    match error {
        AlreadyRunning { .. } => ("already_running", "none"),
        Paused { .. } => ("paused", "none"),
        Unavailable { .. } => ("unavailable", "none"),
        Canceled { .. } => ("canceled", "none"),
        OutcomeDropped { .. } => ("outcome_dropped", "none"),
        Agent(error) => ("agent", llm_category(error)),
        Runtime(error) => (
            "runtime",
            match error {
                AgentSessionRuntimeError::FailedPrecondition(_) => "runtime_precondition",
                AgentSessionRuntimeError::Internal(_) => "runtime_internal",
                AgentSessionRuntimeError::Agent(error) => llm_category(error),
                AgentSessionRuntimeError::Authentication { reason, .. } => auth_category(*reason),
            },
        ),
    }
}

fn warn_admission(
    session_id: &SessionId,
    stage: &'static str,
    category: &'static str,
    cause: &'static str,
) {
    tracing::warn!(
        session_id = session_id.as_str(),
        stage,
        category,
        cause,
        "owned execution admission failed"
    );
}

#[derive(Default)]
struct OwnedState {
    closed: bool,
    wait: Option<RunWait>,
    closing: CancellationToken,
}

impl Drop for OwnedState {
    fn drop(&mut self) {
        self.closing.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Unused;
    impl LlmStream for Unused {
        fn name(&self) -> &'static str {
            "unused"
        }
        fn stream_request<'a>(&'a self, _: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            panic!("no provider")
        }
    }
    struct Guard(Arc<AtomicUsize>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn blocked_store_admission_drains_after_request_and_only_close_waiter_drop() {
        let store = barrier_store::BarrierStore::default();
        let sessions = SessionCoordinator::new(store.clone());
        let mut create = SessionCreateRequest::new(SessionId::generate());
        create.inference_settings = Some(InferenceSettings {
            provider: InferenceProviderSettings {
                key: "test".into(),
                api: ModelApi::OpenAiResponses,
                url: "https://unused.invalid".into(),
                model_id: "test".into(),
                model_cost: None,
                headers: Default::default(),
                oauth: None,
                reasoning_effort: None,
            },
        });
        let id = sessions.create(create).await.unwrap().id().clone();
        let owner = SessionExecutionOwner::new(sessions.clone(), id.clone());
        let dropped = Arc::new(AtomicUsize::new(0));
        let guard = Guard(dropped.clone());
        let runtime = AgentRuntime::builder(
            ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
            Arc::new(Unused),
        )
        .with_tool(AgentTool::new_async(
            ToolDefinition::new("alias", "effect", json!({"type":"object"})),
            move |_| {
                let _guard = &guard;
                panic!("cancelled admission must not dispatch")
            },
        ))
        .build();
        store.arm();
        let request = tokio::spawn({
            let owner = owner.clone();
            async move {
                owner
                    .tool(
                        runtime,
                        ToolCall::new("effect", "alias", json!({})),
                        CancellationToken::new(),
                    )
                    .await
            }
        });
        store.entered.notified().await;
        assert!(sessions.active_state(&id).await.is_some());
        request.abort();
        drop(request.await);
        let close = tokio::spawn({
            let owner = owner.clone();
            async move { owner.close().await }
        });
        owner.closing.cancelled().await;
        assert!(!close.is_finished());
        close.abort();
        drop(close.await);
        drop(owner);
        let shutdown = tokio::spawn({
            let sessions = sessions.clone();
            async move { sessions.shutdown().await }
        });
        tokio::task::yield_now().await;
        assert!(!shutdown.is_finished());
        store.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), shutdown)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn driver_errors_and_panics_never_report_clean_cleanup() {
        for panic in [false, true] {
            let sessions = SessionCoordinator::new(MemorySessionStore::new());
            let owner = SessionExecutionOwner::new(sessions.clone(), SessionId::generate());
            let result = owner
                .start(
                    Box::pin(async move {
                        assert!(!panic, "driver panic fixture");
                        Err(crate::AgentSessionRunError::Unavailable {
                            session_id: SessionId::generate(),
                        })
                    }),
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(
                result,
                Err(if panic {
                    OwnedExecutionError::Uncertain
                } else {
                    OwnedExecutionError::Unavailable
                })
            );
            assert!(owner.close().await.is_err());
            tokio::time::timeout(std::time::Duration::from_secs(2), sessions.shutdown())
                .await
                .unwrap();
            let late = SessionExecutionOwner::new(sessions, SessionId::generate());
            assert_eq!(
                late.start(
                    Box::pin(async { panic!("shutdown must reject before admission") }),
                    CancellationToken::new()
                )
                .await,
                Err(OwnedExecutionError::Unavailable)
            );
        }
    }

    #[tokio::test]
    async fn committed_admission_is_retained_across_request_and_cleanup_drop_and_shutdown_race() {
        for (shutdown, drop_only) in [(false, false), (true, false), (true, true)] {
            let sessions = SessionCoordinator::new(MemorySessionStore::new());
            let mut create = SessionCreateRequest::new(SessionId::generate());
            create.inference_settings = Some(InferenceSettings {
                provider: InferenceProviderSettings {
                    key: "test".into(),
                    api: ModelApi::OpenAiResponses,
                    url: "https://unused.invalid".into(),
                    model_id: "test".into(),
                    model_cost: None,
                    headers: Default::default(),
                    oauth: None,
                    reasoning_effort: None,
                },
            });
            let id = sessions.create(create).await.unwrap().id().clone();
            let started = Arc::new(tokio::sync::Notify::new());
            let dropped = Arc::new(AtomicUsize::new(0));
            let tool = AgentTool::new_async(
                ToolDefinition::new("alias", "effect", json!({"type":"object"})),
                {
                    let started = started.clone();
                    let dropped = dropped.clone();
                    move |_| {
                        let started = started.clone();
                        let dropped = dropped.clone();
                        Box::pin(async move {
                            let _guard = Guard(dropped);
                            started.notify_one();
                            std::future::pending().await
                        })
                    }
                },
            );
            let runtime = AgentRuntime::builder(
                ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
                Arc::new(Unused),
            )
            .with_tool(tool)
            .build();
            let owner = SessionExecutionOwner::new(sessions.clone(), id.clone());
            let admitted = Arc::new(tokio::sync::Notify::new());
            let barrier = Arc::new(tokio::sync::Notify::new());
            let request = tokio::spawn({
                let owner = owner.clone();
                let sessions = sessions.clone();
                let admitted = admitted.clone();
                let barrier = barrier.clone();
                async move {
                    owner
                        .start(
                            Box::pin(async move {
                                let submission = sessions
                                    .start_pinned_host_tool(
                                        id,
                                        runtime,
                                        ToolCall::new("effect", "alias", json!({})),
                                    )
                                    .await?;
                                admitted.notify_one();
                                barrier.notified().await;
                                Ok(submission)
                            }),
                            CancellationToken::new(),
                        )
                        .await
                }
            });
            admitted.notified().await;
            started.notified().await;
            request.abort();
            drop(request.await);
            let closing = tokio::spawn({
                let owner = owner.clone();
                async move { owner.close().await }
            });
            owner.closing.cancelled().await;
            assert!(!closing.is_finished());
            closing.abort();
            drop(closing.await);
            let joined = (!drop_only).then(|| {
                tokio::spawn({
                    let owner = owner.clone();
                    async move { owner.close().await }
                })
            });
            let shutdown = shutdown.then(|| {
                tokio::spawn({
                    let sessions = sessions.clone();
                    async move { sessions.shutdown().await }
                })
            });
            barrier.notify_one();
            if let Some(joined) = joined {
                let result = tokio::time::timeout(std::time::Duration::from_secs(2), joined)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(owner.close().await, result);
            }
            if let Some(shutdown) = shutdown {
                tokio::time::timeout(std::time::Duration::from_secs(2), shutdown)
                    .await
                    .unwrap()
                    .unwrap();
            }
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
        }
    }
}

/// Owns one private consumer session. Call `close` during invocation cleanup, even when the
/// request future was dropped. No new execution may start after cleanup begins.
#[derive(Clone)]
pub struct SessionExecutionOwner<S: SessionStore + Clone + Send + Sync + 'static> {
    sessions: SessionCoordinator<S>,
    session_id: SessionId,
    state: Arc<Mutex<OwnedState>>,
    closing: CancellationToken,
    runtime: tokio::runtime::Handle,
}

impl<S: SessionStore + Clone + Send + Sync + 'static> std::fmt::Debug for SessionExecutionOwner<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionExecutionOwner")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl<S: SessionStore + Clone + Send + Sync + 'static> SessionExecutionOwner<S> {
    /// Bind an exclusively consumer-owned session on the host's long-lived Tokio runtime.
    /// That runtime must remain alive until coordinator shutdown has joined owned work.
    pub fn new(sessions: SessionCoordinator<S>, session_id: SessionId) -> Self {
        Self::on_runtime(sessions, session_id, tokio::runtime::Handle::current())
    }

    /// Bind a host runtime explicitly when sessions are allocated from a guest worker.
    /// The supplied runtime must outlive coordinator shutdown and every owned join.
    pub fn on_runtime(
        sessions: SessionCoordinator<S>,
        session_id: SessionId,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let closing = CancellationToken::new();
        Self {
            sessions,
            session_id,
            state: Arc::new(Mutex::new(OwnedState {
                closing: closing.clone(),
                closed: false,
                wait: None,
            })),
            closing,
            runtime,
        }
    }
    /// Run one host tool call and retain its join handle for cleanup.
    pub async fn tool(
        &self,
        runtime: AgentRuntime,
        call: crate::ToolCall,
        cancellation: CancellationToken,
    ) -> Result<AgentSessionRunOutcome, OwnedExecutionError> {
        let sessions = self.sessions.clone();
        let session_id = self.session_id.clone();
        self.start(
            Box::pin(async move {
                sessions
                    .start_pinned_host_tool(session_id, runtime, call)
                    .await
            }),
            cancellation,
        )
        .await
    }
    /// Run one validated helper with normal coordinator ownership and durable session events.
    pub async fn helper(
        &self,
        runtime: AgentRuntime,
        validation: CompletionValidation,
        limits: AgentRunLimits,
        cancellation: CancellationToken,
    ) -> Result<AgentSessionRunOutcome, OwnedExecutionError> {
        let sessions = self.sessions.clone();
        let session_id = self.session_id.clone();
        self.start(
            Box::pin(async move {
                sessions
                    .start_pinned_helper(session_id, runtime, Some(validation), limits)
                    .await
            }),
            cancellation,
        )
        .await
    }
    /// Run a helper retaining invocation-local payload credits through owned cancellation/join.
    /// Moves an already-constructed runtime; pinned admission does not invoke the runtime factory.
    ///
    /// # Errors
    /// Rejects unsupported input, unavailable ownership, or failed helper execution.
    #[allow(
        clippy::too_many_arguments,
        reason = "extends the existing owned helper contract with invocation-local input"
    )]
    pub async fn helper_with_transient_input(
        &self,
        mut runtime: AgentRuntime,
        input: crate::TransientInput,
        validation: CompletionValidation,
        limits: AgentRunLimits,
        cancellation: CancellationToken,
    ) -> Result<AgentSessionRunOutcome, OwnedExecutionError> {
        runtime.provider =
            crate::transient_input::bind_provider(runtime.provider, input).map_err(|error| {
                warn_admission(
                    &self.session_id,
                    "transient_bind",
                    llm_category(&error),
                    "none",
                );
                OwnedExecutionError::Unavailable
            })?;
        self.helper(runtime, validation, limits, cancellation).await
    }
    async fn start(
        &self,
        submit: Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<AgentSessionRunSubmission, crate::AgentSessionRunError>,
                    > + Send
                    + 'static,
            >,
        >,
        cancellation: CancellationToken,
    ) -> Result<AgentSessionRunOutcome, OwnedExecutionError> {
        let mut state = self.state.lock().await;
        if state.closed
            || self.closing.is_cancelled()
            || cancellation.is_cancelled()
            || state
                .wait
                .as_ref()
                .is_some_and(|wait| wait.peek().is_none())
        {
            let category = if state.closed {
                "owner_closed"
            } else if self.closing.is_cancelled() {
                "owner_closing"
            } else if cancellation.is_cancelled() {
                "request_canceled"
            } else {
                "owner_busy"
            };
            warn_admission(&self.session_id, "owner_state", category, "none");
            return Err(OwnedExecutionError::Unavailable);
        }
        let sessions = self.sessions.clone();
        let session_id = self.session_id.clone();
        let closing = self.closing.clone();
        let driver = self
            .sessions
            .drive_owned_execution(&self.runtime, async move {
                let submission = submit.await.map_err(|error| {
                    let (category, cause) = submission_category(&error);
                    warn_admission(&session_id, "submit", category, cause);
                    OwnedExecutionError::Unavailable
                })?;
                let joined = submission.handle.wait();
                tokio::pin!(joined);
                tokio::select! {
                    biased;
                    () = closing.cancelled() => {
                        let interrupted = sessions.interrupt(&session_id).await;
                        let outcome = joined.await.map_err(|_| OwnedExecutionError::Uncertain)?;
                        interrupted.map_err(|_| OwnedExecutionError::Uncertain)?;
                        Ok(outcome)
                    }
                    outcome = &mut joined => outcome.map_err(|_| OwnedExecutionError::Uncertain),
                }
            })
            .await
            .ok_or_else(|| {
                warn_admission(&self.session_id, "drive", "driver_rejected", "none");
                OwnedExecutionError::Unavailable
            })?;
        let wait = async move { driver.await.map_err(|_| OwnedExecutionError::Uncertain)? }
            .boxed()
            .shared();
        state.wait = Some(wait.clone());
        drop(state);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                self.closing.cancel();
                drop(wait.await);
                Err(OwnedExecutionError::Uncertain)
            }
            outcome = wait.clone() => outcome,
        }
    }
    /// Prevent admission, cancel the actual run, and join cleanup. Idempotent; never replays.
    pub async fn close(&self) -> Result<(), OwnedExecutionError> {
        let wait = {
            let mut state = self.state.lock().await;
            state.closed = true;
            self.closing.cancel();
            state.wait.clone()
        };
        if let Some(wait) = wait {
            wait.await?;
        }
        Ok(())
    }
}
