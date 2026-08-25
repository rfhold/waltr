use std::{future::Future, num::NonZeroU64, pin::Pin, sync::Arc};

use waltr_agent::{
    AgentRunLimits, CompletedSessionCapture, InferenceSettings, MetadataSetCondition,
    SessionCreateRequest, SessionId,
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

pub(super) trait RoutineHost: Send + Sync {
    fn capture_latest_completed<'a>(&'a self, id: &'a SessionId) -> CaptureFuture<'a>;
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
