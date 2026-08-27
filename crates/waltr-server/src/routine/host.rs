use std::{future::Future, num::NonZeroU64, pin::Pin, sync::Arc};

use waltr_agent::{
    AgentRunLimits, CompletedSessionCapture, InferenceSettings, MetadataSetCondition,
    SessionCoordinationReason, SessionCreateRequest, SessionId, SessionMaskMutation,
    SessionMaskMutationOutcome, SessionPauseError, SessionPauseLease, TimelineCursor,
};
use serde_json::Value;

use crate::session::SessionHost;

pub(super) type HostFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;
pub(super) type CaptureFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<CompletedSessionCapture, waltr_agent::SessionCaptureError>>
            + Send
            + 'a,
    >,
>;
pub(super) type ChildRunFuture =
    Pin<Box<dyn Future<Output = Result<Option<String>, String>> + Send>>;
pub(super) type PauseFuture<'a> =
    Pin<Box<dyn Future<Output = Result<RoutinePause, SessionPauseError>> + Send + 'a>>;
pub(super) type PauseCaptureFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompletedSessionCapture, SessionPauseError>> + Send + 'a>>;
pub(super) type PauseMaskFuture<'a> = Pin<
    Box<dyn Future<Output = Result<SessionMaskMutationOutcome, SessionPauseError>> + Send + 'a>,
>;
pub(super) type PauseUnitFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), SessionPauseError>> + Send + 'a>>;

/// Opaque Routine ownership of one coordinator pause lease.
#[derive(Debug)]
pub(super) struct RoutinePause {
    inner: RoutinePauseInner,
}

#[derive(Debug)]
enum RoutinePauseInner {
    Coordinator(SessionPauseLease),
    #[cfg(test)]
    Test {
        session_id: SessionId,
        source_cursor: TimelineCursor,
    },
}

impl RoutinePause {
    pub(super) fn from_lease(lease: SessionPauseLease) -> Self {
        Self {
            inner: RoutinePauseInner::Coordinator(lease),
        }
    }

    pub(super) fn into_lease(self) -> SessionPauseLease {
        match self.inner {
            RoutinePauseInner::Coordinator(lease) => lease,
            #[cfg(test)]
            RoutinePauseInner::Test { .. } => panic!("test pause has no coordinator lease"),
        }
    }

    pub(super) fn session_id(&self) -> &SessionId {
        match &self.inner {
            RoutinePauseInner::Coordinator(lease) => lease.session_id(),
            #[cfg(test)]
            RoutinePauseInner::Test { session_id, .. } => session_id,
        }
    }

    pub(super) fn source_cursor(&self) -> TimelineCursor {
        match &self.inner {
            RoutinePauseInner::Coordinator(lease) => lease.source_cursor(),
            #[cfg(test)]
            RoutinePauseInner::Test { source_cursor, .. } => *source_cursor,
        }
    }

    fn lease(&self) -> &SessionPauseLease {
        match &self.inner {
            RoutinePauseInner::Coordinator(lease) => lease,
            #[cfg(test)]
            RoutinePauseInner::Test { .. } => panic!("test pause has no coordinator lease"),
        }
    }
}

#[cfg(test)]
impl RoutinePause {
    pub(super) fn for_test(session_id: SessionId, source_cursor: TimelineCursor) -> Self {
        Self {
            inner: RoutinePauseInner::Test {
                session_id,
                source_cursor,
            },
        }
    }
}

pub(super) trait RoutineHost: Send + Sync {
    fn capture_latest_completed<'a>(&'a self, id: &'a SessionId) -> CaptureFuture<'a>;
    fn acquire_pause<'a>(
        &'a self,
        id: &'a SessionId,
        reason: SessionCoordinationReason,
    ) -> PauseFuture<'a>;
    fn capture_paused_effective<'a>(&'a self, pause: &'a RoutinePause) -> PauseCaptureFuture<'a>;
    fn apply_paused_mask<'a>(
        &'a self,
        pause: &'a RoutinePause,
        mutation: SessionMaskMutation,
    ) -> PauseMaskFuture<'a>;
    fn arm_paused_continuation<'a>(
        &'a self,
        pause: &'a RoutinePause,
        cursor: TimelineCursor,
    ) -> PauseUnitFuture<'a>;
    fn release_pause<'a>(&'a self, pause: &'a RoutinePause) -> PauseUnitFuture<'a>;
    fn load_inference<'a>(&'a self, id: &'a SessionId) -> HostFuture<'a, InferenceSettings>;
    fn create_child(&self, request: SessionCreateRequest) -> HostFuture<'_, ()>;
    fn submit_child(&self, id: SessionId, max_turns: NonZeroU64) -> HostFuture<'_, ChildRunFuture>;
    fn create_destination(&self, request: SessionCreateRequest) -> HostFuture<'_, ()>;
    fn start_destination(&self, id: SessionId) -> HostFuture<'_, ()>;
    fn set_metadata_if_absent<'a>(
        &'a self,
        id: &'a SessionId,
        key: String,
        value: Value,
    ) -> HostFuture<'a, bool>;
    fn interrupt<'a>(&'a self, id: &'a SessionId) -> HostFuture<'a, ()>;
}

pub(super) struct RoutinePauseGuard {
    pause: Option<Arc<RoutinePause>>,
    host: Arc<dyn RoutineHost>,
}

impl RoutinePauseGuard {
    pub(super) fn new(pause: RoutinePause, host: Arc<dyn RoutineHost>) -> Self {
        Self {
            pause: Some(Arc::new(pause)),
            host,
        }
    }

    pub(super) fn pause(&self) -> &RoutinePause {
        self.pause.as_deref().expect("Routine pause guard is armed")
    }

    pub(super) async fn release(&mut self) -> Result<(), SessionPauseError> {
        let Some(pause) = self.pause.as_ref() else {
            return Ok(());
        };
        self.host.release_pause(pause).await?;
        self.pause.take();
        Ok(())
    }
}

impl Drop for RoutinePauseGuard {
    fn drop(&mut self) {
        let Some(pause) = self.pause.take() else {
            return;
        };
        let host = Arc::clone(&self.host);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn(async move {
                if let Err(error) = host.release_pause(&pause).await {
                    tracing::warn!(%error, "failed to release canceled Routine pause");
                }
            }));
        }
    }
}

pub(super) struct CoordinatorRoutineHost {
    sessions: Arc<dyn SessionHost>,
}

impl CoordinatorRoutineHost {
    pub(super) fn new(sessions: Arc<dyn SessionHost>) -> Self {
        Self { sessions }
    }
}

impl RoutineHost for CoordinatorRoutineHost {
    fn capture_latest_completed<'a>(&'a self, id: &'a SessionId) -> CaptureFuture<'a> {
        Box::pin(async move { self.sessions.capture_latest_completed(id).await })
    }

    fn acquire_pause<'a>(
        &'a self,
        id: &'a SessionId,
        reason: SessionCoordinationReason,
    ) -> PauseFuture<'a> {
        Box::pin(async move {
            self.sessions
                .acquire_pause_reject_busy(id, reason)
                .await
                .map(|lease| RoutinePause {
                    inner: RoutinePauseInner::Coordinator(lease),
                })
        })
    }

    fn capture_paused_effective<'a>(&'a self, pause: &'a RoutinePause) -> PauseCaptureFuture<'a> {
        Box::pin(async move { self.sessions.capture_paused_effective(pause.lease()).await })
    }

    fn apply_paused_mask<'a>(
        &'a self,
        pause: &'a RoutinePause,
        mutation: SessionMaskMutation,
    ) -> PauseMaskFuture<'a> {
        Box::pin(async move {
            self.sessions
                .apply_paused_mask_mutation(pause.lease(), mutation)
                .await
        })
    }

    fn arm_paused_continuation<'a>(
        &'a self,
        pause: &'a RoutinePause,
        cursor: TimelineCursor,
    ) -> PauseUnitFuture<'a> {
        Box::pin(async move {
            self.sessions
                .arm_paused_continuation(pause.lease(), cursor, AgentRunLimits::default())
                .await
                .map(drop)
        })
    }

    fn release_pause<'a>(&'a self, pause: &'a RoutinePause) -> PauseUnitFuture<'a> {
        Box::pin(async move { self.sessions.release_pause(pause.lease()).await })
    }

    fn load_inference<'a>(&'a self, id: &'a SessionId) -> HostFuture<'a, InferenceSettings> {
        Box::pin(async move {
            self.sessions
                .load(id)
                .await
                .map_err(|error| error.to_string())?
                .record
                .inference_settings
                .ok_or_else(|| "source session has no inference settings".to_owned())
        })
    }

    fn create_child(&self, request: SessionCreateRequest) -> HostFuture<'_, ()> {
        Box::pin(async move {
            self.sessions
                .create_exact(request)
                .await
                .map(drop)
                .map_err(|error| error.to_string())
        })
    }

    fn submit_child(&self, id: SessionId, max_turns: NonZeroU64) -> HostFuture<'_, ChildRunFuture> {
        Box::pin(async move {
            let handle = self
                .sessions
                .start_durable_prompt(
                    id,
                    AgentRunLimits::default().with_max_provider_turns(max_turns),
                )
                .await
                .map_err(|error| error.to_string())?
                .handle;
            Ok(Box::pin(async move {
                handle
                    .wait()
                    .await
                    .map(|outcome| outcome.assistant_text)
                    .map_err(|error| error.to_string())
            }) as ChildRunFuture)
        })
    }

    fn create_destination(&self, request: SessionCreateRequest) -> HostFuture<'_, ()> {
        Box::pin(async move {
            self.sessions
                .create_exact(request)
                .await
                .map(drop)
                .map_err(|error| error.to_string())
        })
    }

    fn start_destination(&self, id: SessionId) -> HostFuture<'_, ()> {
        Box::pin(async move {
            self.sessions
                .start_durable_prompt(id, AgentRunLimits::default())
                .await
                .map(drop)
                .map_err(|error| error.to_string())
        })
    }

    fn set_metadata_if_absent<'a>(
        &'a self,
        id: &'a SessionId,
        key: String,
        value: Value,
    ) -> HostFuture<'a, bool> {
        Box::pin(async move {
            self.sessions
                .set_metadata_value_conditionally(id, key, value, MetadataSetCondition::Absent)
                .await
                .map(|outcome| outcome.changed)
                .map_err(|error| error.to_string())
        })
    }

    fn interrupt<'a>(&'a self, id: &'a SessionId) -> HostFuture<'a, ()> {
        Box::pin(async move {
            self.sessions
                .interrupt(id)
                .await
                .map(drop)
                .map_err(|error| error.to_string())
        })
    }
}
