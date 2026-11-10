use super::*;
use waltr_agent::{AgentTimelineEvent, InferenceSettings, MetadataSetCondition, MetadataSetOutcome,
    SessionStoreResult, SessionSummary, TimelineItemText, TimelineQuery};

#[derive(Clone)]
pub(super) struct GatedStore {
    pub memory: MemorySessionStore,
    pub source: SessionId,
    pub entered: Arc<tokio::sync::Notify>,
    pub resume: Arc<tokio::sync::Notify>,
}

impl SessionStore for GatedStore {
    async fn create(&self, request: SessionCreateRequest) -> SessionStoreResult<SessionRecord> {
        if request.id != self.source {
            self.entered.notify_one();
            self.resume.notified().await;
        }
        self.memory.create(request).await
    }
    async fn load(&self, id: &SessionId) -> SessionStoreResult<SessionRecord> { self.memory.load(id).await }
    async fn delete(&self, id: &SessionId) -> SessionStoreResult<()> { self.memory.delete(id).await }
    async fn wipe(&self) -> SessionStoreResult<()> { self.memory.wipe().await }
    async fn list(&self) -> SessionStoreResult<Vec<SessionSummary>> { self.memory.list().await }
    async fn append_timeline(&self, id: &SessionId, token: waltr_agent::ExecutionToken, events: Vec<PendingTimelineEvent>) -> SessionStoreResult<waltr_agent::TimelineAppendReceipt> {
        self.memory.append_timeline(id, token, events).await
    }
    async fn append_journal(&self, id: &SessionId, token: waltr_agent::ExecutionToken, events: Vec<SessionJournalEvent>) -> SessionStoreResult<waltr_agent::SessionCommitReceipt> {
        self.memory.append_journal(id, token, events).await
    }
    async fn mutate(&self, id: &SessionId, mutation: SessionMutation) -> SessionStoreResult<waltr_agent::SessionMutationOutcome> {
        self.memory.mutate(id, mutation).await
    }
    async fn set_metadata_value_conditionally(&self, id: &SessionId, key: String, value: Value, condition: MetadataSetCondition) -> SessionStoreResult<MetadataSetOutcome> {
        self.memory.set_metadata_value_conditionally(id, key, value, condition).await
    }
    async fn remove_metadata_value(&self, id: &SessionId, key: String) -> SessionStoreResult<SessionRecord> {
        self.memory.remove_metadata_value(id, key).await
    }
    async fn set_inference_settings(&self, id: &SessionId, settings: Option<InferenceSettings>) -> SessionStoreResult<SessionRecord> {
        self.memory.set_inference_settings(id, settings).await
    }
    async fn set_agent_definition(&self, id: &SessionId, definition: Option<Value>) -> SessionStoreResult<SessionRecord> {
        self.memory.set_agent_definition(id, definition).await
    }
    async fn timeline(&self, id: &SessionId, query: TimelineQuery) -> SessionStoreResult<Vec<AgentTimelineEvent>> {
        self.memory.timeline(id, query).await
    }
    async fn timeline_item_text(&self, id: &SessionId, item: &TimelineItemId) -> SessionStoreResult<TimelineItemText> {
        self.memory.timeline_item_text(id, item).await
    }
}
