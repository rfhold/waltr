use super::*;
use std::result::Result;

pub(super) struct UnusedSessions;

#[tonic::async_trait]
impl crate::session::SessionHost for UnusedSessions {
    fn set_completion_admission_handler(
        &self,
        _: Option<Arc<dyn SessionCompletionCoordinationHandler>>,
    ) {
        panic!("unused")
    }
    async fn create(&self, _: Option<String>) -> Result<AgentSession, tonic::Status> {
        panic!("unused")
    }
    async fn create_exact(&self, _: SessionCreateRequest) -> Result<AgentSession, LlmError> {
        panic!("unused")
    }
    async fn capture_latest_completed(
        &self,
        _: &SessionId,
    ) -> Result<CompletedSessionCapture, SessionCaptureError> {
        panic!("unused")
    }
    async fn acquire_pause_reject_busy(
        &self,
        _: &SessionId,
        _: SessionCoordinationReason,
    ) -> Result<SessionPauseLease, SessionPauseError> {
        panic!("unused")
    }
    async fn capture_paused_effective(
        &self,
        _: &SessionPauseLease,
    ) -> Result<CompletedSessionCapture, SessionPauseError> {
        panic!("unused")
    }
    async fn apply_paused_mask_mutation(
        &self,
        _: &SessionPauseLease,
        _: SessionMaskMutation,
    ) -> Result<SessionMaskMutationOutcome, SessionPauseError> {
        panic!("unused")
    }
    async fn arm_paused_continuation(
        &self,
        _: &SessionPauseLease,
        _: TimelineCursor,
        _: AgentRunLimits,
    ) -> Result<AgentSessionRunSubmission, SessionPauseError> {
        panic!("unused")
    }
    async fn release_pause(&self, _: &SessionPauseLease) -> Result<(), SessionPauseError> {
        panic!("unused")
    }
    async fn start_durable_prompt(
        &self,
        _: SessionId,
        _: AgentRunLimits,
    ) -> Result<AgentSessionRunSubmission, AgentSessionRunError> {
        panic!("unused")
    }
    async fn list(&self) -> Result<Vec<SessionSummary>, LlmError> {
        panic!("unused")
    }
    async fn load(&self, _: &SessionId) -> Result<AgentSession, LlmError> {
        panic!("unused")
    }
    async fn wipe(&self) -> Result<(), LlmError> {
        panic!("unused")
    }
    async fn interrupt(&self, _: &SessionId) -> Result<AgentSession, LlmError> {
        panic!("unused")
    }
    async fn active_state(&self, _: &SessionId) -> Option<AgentSessionActiveRunState> {
        panic!("unused")
    }
    async fn submit_prompt(
        &self,
        _: SessionId,
        _: String,
    ) -> Result<AgentSessionRunSubmission, AgentSessionRunError> {
        panic!("unused")
    }
    async fn set_metadata_value_conditionally(
        &self,
        _: &SessionId,
        _: String,
        _: serde_json::Value,
        _: MetadataSetCondition,
    ) -> Result<MetadataSetOutcome, LlmError> {
        panic!("unused")
    }
    fn watch_sessions(
        &self,
    ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = SessionListUpdate> + Send>> {
        panic!("unused")
    }
    async fn shutdown(&self) {
        panic!("unused")
    }
}

pub(super) fn adapters() -> Arc<dyn interaction::Adapters> {
    Arc::new(interaction::SessionAdapters::new(Arc::new(UnusedSessions)))
}
