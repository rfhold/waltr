use super::agent::*;
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

#[derive(Clone, Default)]
pub struct BarrierStore {
    inner: MemorySessionStore,
    armed: Arc<AtomicBool>,
    fail_failure_append: bool,
    pub entered: Arc<Notify>,
    pub release: Arc<Notify>,
}

impl BarrierStore {
    #[allow(dead_code)] // The server imports this fixture but doesn't inject journal faults.
    pub fn with_failed_failure_append(mut self, fail: bool) -> Self {
        self.fail_failure_append = fail;
        self
    }

    pub fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
}

macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $output:ty;)*) => {$(
        async fn $name(&self, $($arg: $ty),*) -> SessionStoreResult<$output> {
            self.inner.$name($($arg),*).await
        }
    )*};
}

impl SessionStore for BarrierStore {
    async fn append_journal(
        &self,
        id: &SessionId,
        expected_revision: ExecutionToken,
        events: Vec<SessionJournalEvent>,
    ) -> SessionStoreResult<SessionCommitReceipt> {
        if self.fail_failure_append
            && events.iter().any(|event| matches!(event, SessionJournalEvent::Failed { .. }))
        {
            return Err(SessionStoreError::Backend {
                message: "injected finalization append failure".into(),
            });
        }
        self.inner.append_journal(id, expected_revision, events).await
    }

    async fn load(&self, id: &SessionId) -> SessionStoreResult<SessionRecord> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.load(id).await
    }
    forward! {
        create(request: SessionCreateRequest) -> SessionRecord;
        delete(id: &SessionId) -> ();
        wipe() -> ();
        list() -> Vec<SessionSummary>;
        append_timeline(id: &SessionId, expected_revision: ExecutionToken, events: Vec<PendingTimelineEvent>) -> TimelineAppendReceipt;
        mutate(id: &SessionId, mutation: SessionMutation) -> SessionMutationOutcome;
        set_metadata_value_conditionally(id: &SessionId, key: String, value: Value, condition: MetadataSetCondition) -> MetadataSetOutcome;
        remove_metadata_value(id: &SessionId, key: String) -> SessionRecord;
        set_inference_settings(id: &SessionId, inference_settings: Option<InferenceSettings>) -> SessionRecord;
        set_agent_definition(id: &SessionId, agent_definition: Option<serde_json::Value>) -> SessionRecord;
        timeline(id: &SessionId, query: TimelineQuery) -> Vec<AgentTimelineEvent>;
        timeline_item_text(id: &SessionId, item_id: &TimelineItemId) -> TimelineItemText;
    }
}
