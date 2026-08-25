//! Session-list projection and live publication.

#![allow(clippy::redundant_pub_crate, clippy::significant_drop_tightening)]

use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures_util::Stream;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};

use super::{SessionId, SessionSummary};

const SESSION_LIST_CHANNEL_CAPACITY: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionIncarnation(u64);

#[derive(Debug)]
struct SessionListEntry {
    incarnation: SessionIncarnation,
    summary: Option<Box<SessionSummary>>,
}

#[derive(Debug, Default)]
struct SessionListState {
    sequence: u64,
    next_incarnation: u64,
    sessions: BTreeMap<SessionId, SessionListEntry>,
}

#[derive(Debug)]
pub(crate) struct SessionPublicationClaim {
    sessions: SessionListBroadcaster,
    id: SessionId,
    incarnation: SessionIncarnation,
}

impl SessionPublicationClaim {
    pub(crate) const fn incarnation(&self) -> SessionIncarnation {
        self.incarnation
    }
}

impl Drop for SessionPublicationClaim {
    fn drop(&mut self) {
        self.sessions.cancel_claim(&self.id, self.incarnation);
    }
}

/// Kind of session list change emitted by [`SessionCoordinator`](super::SessionCoordinator).
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum SessionListEvent {
    /// A session was created or updated.
    Upsert(SessionSummary),
    /// A session was deleted.
    Deleted(SessionId),
    /// Every session was deleted.
    Cleared,
}

/// A session list update emitted by [`SessionCoordinator`](super::SessionCoordinator).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionListUpdate {
    /// Monotonic sequence for detecting missed updates.
    pub sequence: u64,
    /// Session list change event.
    pub event: SessionListEvent,
}

/// Live stream of session list updates.
#[derive(Debug)]
pub struct SessionListSubscription {
    initial_sequence: u64,
    inner: BroadcastStream<SessionListUpdate>,
}

impl SessionListSubscription {
    /// Sequence observed when this subscription was opened.
    #[must_use]
    pub const fn initial_sequence(&self) -> u64 {
        self.initial_sequence
    }
}

impl Stream for SessionListSubscription {
    type Item = SessionListUpdate;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match Pin::new(&mut self.inner).poll_next(context) {
                Poll::Ready(Some(Ok(update))) => return Poll::Ready(Some(update)),
                Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => {}
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionListBroadcaster {
    state: Arc<Mutex<SessionListState>>,
    sender: broadcast::Sender<SessionListUpdate>,
}

impl SessionListBroadcaster {
    pub(crate) fn new() -> Self {
        let (sender, _) = broadcast::channel(SESSION_LIST_CHANNEL_CAPACITY);
        Self {
            state: Arc::new(Mutex::new(SessionListState::default())),
            sender,
        }
    }

    pub(crate) fn subscribe(&self) -> SessionListSubscription {
        let state = self.state.lock().expect("session list state lock poisoned");
        SessionListSubscription {
            initial_sequence: state.sequence,
            inner: BroadcastStream::new(self.sender.subscribe()),
        }
    }

    #[allow(clippy::option_if_let_else)]
    pub(crate) fn claim(&self, id: &SessionId) -> SessionPublicationClaim {
        let mut state = self.state.lock().expect("session list state lock poisoned");
        let incarnation = if let Some(entry) = state.sessions.get(id) {
            entry.incarnation
        } else {
            let incarnation = state.next_incarnation();
            state.sessions.insert(
                id.clone(),
                SessionListEntry {
                    incarnation,
                    summary: None,
                },
            );
            incarnation
        };
        drop(state);
        SessionPublicationClaim {
            sessions: self.clone(),
            id: id.clone(),
            incarnation,
        }
    }

    pub(crate) fn claim_incarnation(
        &self,
        id: &SessionId,
        expected: SessionIncarnation,
    ) -> Option<SessionPublicationClaim> {
        let state = self.state.lock().expect("session list state lock poisoned");
        let matches = state
            .sessions
            .get(id)
            .is_some_and(|entry| entry.incarnation == expected);
        drop(state);
        matches.then(|| SessionPublicationClaim {
            sessions: self.clone(),
            id: id.clone(),
            incarnation: expected,
        })
    }

    pub(crate) fn register_loaded_summary(
        &self,
        incarnation: SessionIncarnation,
        summary: SessionSummary,
    ) {
        let mut state = self.state.lock().expect("session list state lock poisoned");
        if let Some(entry) = state.sessions.get_mut(&summary.id)
            && entry.incarnation == incarnation
            && entry.summary.is_none()
        {
            entry.summary = Some(Box::new(summary));
        }
    }

    pub(crate) fn publish_created(&self, incarnation: SessionIncarnation, summary: SessionSummary) {
        let mut state = self.state.lock().expect("session list state lock poisoned");
        let Some(entry) = state.sessions.get_mut(&summary.id) else {
            return;
        };
        if entry.incarnation != incarnation || entry.summary.is_some() {
            return;
        }
        entry.summary = Some(Box::new(summary.clone()));
        self.publish_update(&mut state, SessionListEvent::Upsert(summary));
    }

    pub(crate) fn publish_host_summary(
        &self,
        incarnation: SessionIncarnation,
        summary: SessionSummary,
    ) {
        let mut state = self.state.lock().expect("session list state lock poisoned");
        let Some(entry) = state.sessions.get_mut(&summary.id) else {
            return;
        };
        if entry.incarnation != incarnation {
            return;
        }
        let summary = match entry.summary.as_ref() {
            None => summary,
            Some(current) => {
                let mut merged = if summary.revision >= current.revision {
                    summary.clone()
                } else {
                    (**current).clone()
                };
                merged.metadata_values.clone_from(&summary.metadata_values);
                merged
                    .inference_settings
                    .clone_from(&summary.inference_settings);
                merged.agent_name.clone_from(&summary.agent_name);
                merged.updated_at = merged.updated_at.max(summary.updated_at);
                merged.last_activity_at = merged.last_activity_at.max(summary.last_activity_at);
                merged
            }
        };
        entry.summary = Some(Box::new(summary.clone()));
        self.publish_update(&mut state, SessionListEvent::Upsert(summary));
    }

    pub(crate) fn publish_run_summary(
        &self,
        incarnation: SessionIncarnation,
        mut summary: SessionSummary,
    ) {
        let mut state = self.state.lock().expect("session list state lock poisoned");
        let Some(entry) = state.sessions.get_mut(&summary.id) else {
            return;
        };
        if entry.incarnation != incarnation {
            return;
        }
        let Some(retained) = entry.summary.as_mut() else {
            return;
        };
        if summary.revision < retained.revision {
            return;
        }
        if summary.revision == retained.revision {
            summary.status = retained.status.clone();
            summary.current_prompt.clone_from(&retained.current_prompt);
            summary.metrics.clone_from(&retained.metrics);
        }
        summary.updated_at = summary.updated_at.max(retained.updated_at);
        summary.last_activity_at = summary.last_activity_at.max(retained.last_activity_at);
        summary
            .metadata_values
            .clone_from(&retained.metadata_values);
        summary
            .inference_settings
            .clone_from(&retained.inference_settings);
        summary.agent_name.clone_from(&retained.agent_name);
        retained.as_mut().clone_from(&summary);
        self.publish_update(&mut state, SessionListEvent::Upsert(summary));
    }

    pub(crate) fn publish_deleted(&self, id: SessionId) {
        let mut state = self.state.lock().expect("session list state lock poisoned");
        state.sessions.remove(&id);
        self.publish_update(&mut state, SessionListEvent::Deleted(id));
    }

    pub(crate) fn publish_cleared(&self) {
        let mut state = self.state.lock().expect("session list state lock poisoned");
        state.sessions.clear();
        self.publish_update(&mut state, SessionListEvent::Cleared);
    }

    fn publish_update(&self, state: &mut SessionListState, event: SessionListEvent) {
        state.sequence = state.sequence.saturating_add(1);
        drop(self.sender.send(SessionListUpdate {
            sequence: state.sequence,
            event,
        }));
    }

    fn cancel_claim(&self, id: &SessionId, incarnation: SessionIncarnation) {
        let mut state = self.state.lock().expect("session list state lock poisoned");
        if state
            .sessions
            .get(id)
            .is_some_and(|entry| entry.incarnation == incarnation && entry.summary.is_none())
        {
            state.sessions.remove(id);
        }
    }

    #[cfg(test)]
    pub(crate) fn lifecycle_count(&self) -> usize {
        self.state
            .lock()
            .expect("session list state lock poisoned")
            .sessions
            .len()
    }
}

impl SessionListState {
    const fn next_incarnation(&mut self) -> SessionIncarnation {
        self.next_incarnation = self.next_incarnation.saturating_add(1);
        SessionIncarnation(self.next_incarnation)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        time::{Duration, SystemTime},
    };

    use futures_util::StreamExt as _;
    use serde_json::json;

    use super::{SessionListBroadcaster, SessionListEvent};
    use crate::{Revision, SessionId, SessionRecord};

    fn summary(id: &str, value: &str) -> crate::SessionSummary {
        SessionRecord::with_metadata_values(
            SessionId::new(id),
            BTreeMap::from([("host.key".to_owned(), json!(value))]),
        )
        .summary()
    }

    fn publish_created(
        sessions: &SessionListBroadcaster,
        summary: crate::SessionSummary,
    ) -> super::SessionIncarnation {
        let claim = sessions.claim(&summary.id);
        let incarnation = claim.incarnation();
        sessions.publish_created(incarnation, summary);
        incarnation
    }

    async fn next_event(watch: &mut crate::SessionListSubscription) -> crate::SessionListUpdate {
        tokio::time::timeout(Duration::from_secs(1), watch.next())
            .await
            .expect("session update timeout")
            .expect("session update")
    }

    #[tokio::test]
    async fn stale_incarnation_is_suppressed_after_delete_clear_and_recreate() {
        let sessions = SessionListBroadcaster::new();
        let mut watch = sessions.subscribe();
        let old = summary("recreated", "old");
        let old_incarnation = publish_created(&sessions, old.clone());
        sessions.publish_deleted(old.id.clone());
        sessions.publish_run_summary(old_incarnation, old.clone());

        let new = SessionRecord::new(old.id.clone()).summary();
        let new_incarnation = publish_created(&sessions, new.clone());
        assert_ne!(old_incarnation, new_incarnation);
        sessions.publish_run_summary(old_incarnation, old);
        sessions.publish_run_summary(new_incarnation, new);
        sessions.publish_cleared();
        sessions.publish_run_summary(new_incarnation, SessionRecord::new("recreated").summary());

        let updates = [
            next_event(&mut watch).await,
            next_event(&mut watch).await,
            next_event(&mut watch).await,
            next_event(&mut watch).await,
            next_event(&mut watch).await,
        ];
        assert_eq!(
            updates.each_ref().map(|update| update.sequence),
            [1, 2, 3, 4, 5]
        );
        let SessionListEvent::Upsert(recreated) = &updates[3].event else {
            panic!("expected recreated run summary");
        };
        assert!(recreated.metadata_values.is_empty());
        assert_eq!(updates[4].event, SessionListEvent::Cleared);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), watch.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn host_and_run_fields_merge_by_owner_revision_and_monotonic_time() {
        let sessions = SessionListBroadcaster::new();
        let mut watch = sessions.subscribe();
        let mut retained = summary("merge", "host");
        retained.revision = Revision::new(2);
        retained.current_prompt = Some("retained".to_owned());
        retained.inference_settings = Some(crate::InferenceSettings {
            provider: crate::InferenceProviderSettings {
                key: "provider".to_owned(),
                api: crate::ModelApi::OpenAiResponses,
                url: "https://example.test".to_owned(),
                model_id: "model".to_owned(),
                model_cost: None,
                headers: BTreeMap::new(),
                oauth: None,
                reasoning_effort: None,
            },
        });
        retained.agent_name = Some("selected".to_owned());
        let incarnation = publish_created(&sessions, retained.clone());

        let mut lower = retained.clone();
        lower.revision = Revision::new(1);
        sessions.publish_run_summary(incarnation, lower);

        let mut equal = retained.clone();
        equal.current_prompt = Some("regression".to_owned());
        equal.metadata_values.clear();
        equal.inference_settings = None;
        equal.agent_name = None;
        sessions.publish_run_summary(incarnation, equal);

        let mut newer = retained.clone();
        newer.revision = Revision::new(3);
        newer.status = crate::SessionStatus::Finished;
        newer.current_prompt = Some("newer".to_owned());
        newer.metrics.total_tokens = 42;
        newer.updated_at = SystemTime::UNIX_EPOCH;
        newer.last_activity_at = SystemTime::UNIX_EPOCH;
        newer.metadata_values.clear();
        newer.inference_settings = None;
        newer.agent_name = None;
        sessions.publish_run_summary(incarnation, newer);

        let created = next_event(&mut watch).await;
        let equal = next_event(&mut watch).await;
        let newer = next_event(&mut watch).await;
        assert_eq!(
            (created.sequence, equal.sequence, newer.sequence),
            (1, 2, 3)
        );
        let SessionListEvent::Upsert(equal) = equal.event else {
            panic!("equal upsert")
        };
        assert_eq!(equal.current_prompt.as_deref(), Some("retained"));
        let SessionListEvent::Upsert(newer) = newer.event else {
            panic!("newer upsert")
        };
        assert_eq!(newer.current_prompt.as_deref(), Some("newer"));
        assert_eq!(newer.metrics.total_tokens, 42);
        assert_eq!(newer.updated_at, equal.updated_at);
        assert_eq!(newer.last_activity_at, equal.last_activity_at);
        assert_eq!(newer.metadata_values["host.key"], json!("host"));
        assert!(newer.inference_settings.is_some());
        assert_eq!(newer.agent_name.as_deref(), Some("selected"));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), watch.next())
                .await
                .is_err()
        );
    }

    #[test]
    fn cancelled_claim_only_removes_its_matching_empty_incarnation() {
        let sessions = SessionListBroadcaster::new();
        let id = SessionId::new("claim");
        let old = sessions.claim(&id);
        let old_incarnation = old.incarnation();
        sessions.publish_deleted(id.clone());
        let new = sessions.claim(&id);
        let new_incarnation = new.incarnation();
        assert_ne!(old_incarnation, new_incarnation);

        drop(old);
        assert_eq!(
            sessions.state.lock().unwrap().sessions[&id].incarnation,
            new_incarnation
        );
        drop(new);
        assert!(sessions.state.lock().unwrap().sessions.is_empty());
    }
}
