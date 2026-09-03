use waltr_agent::{SessionEvent, SessionEventSink, SessionEventSinkFuture};
use serde_json::{Value, json};
use tokio::sync::broadcast;

/// Exact semantic event type emitted for durable user messages.
pub const USER_MESSAGE_CREATED: &str = "waltr.session.user-message.created";
const EVENT_BUS_CAPACITY: usize = 256;

/// Generic immutable event admitted by the Routine engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventEnvelope {
    /// Deterministic source event identity.
    pub id: String,
    /// Exact event type.
    pub r#type: String,
    /// Session that originated the event, when the source provides one.
    pub source_session_id: Option<String>,
    /// Event-specific data object.
    pub data: Value,
}

/// Generic event publication boundary.
pub trait EventSink: Send + Sync {
    /// Publish one event without waiting for consumers.
    ///
    /// # Errors
    ///
    /// Returns an error when no event consumer is subscribed.
    fn send(&self, event: EventEnvelope) -> Result<(), String>;
}

/// Generic event subscription boundary.
pub trait EventSource: Send + Sync {
    /// Subscribe to events published after this call.
    fn subscribe(&self) -> broadcast::Receiver<EventEnvelope>;
}

/// Bounded in-memory broadcast bus. Lagged consumers lose events by design.
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<EventEnvelope>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(EVENT_BUS_CAPACITY)
    }
}

impl EventBus {
    /// Create a bus with bounded broadcast capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self { sender }
    }
}

impl EventSink for EventBus {
    fn send(&self, event: EventEnvelope) -> Result<(), String> {
        self.sender
            .send(event)
            .map(drop)
            .map_err(|_| "routine event bus has no subscribers".to_owned())
    }
}

impl EventSource for EventBus {
    fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.sender.subscribe()
    }
}

impl SessionEventSink for EventBus {
    fn send(&self, event: SessionEvent) -> SessionEventSinkFuture {
        let bus = self.clone();
        Box::pin(async move {
            let envelope = session_event_envelope(event);
            EventSink::send(&bus, envelope)
        })
    }
}

pub(super) fn session_event_envelope(event: SessionEvent) -> EventEnvelope {
    match event {
        SessionEvent::UserMessageCreated {
            id,
            session_id,
            user_message_count,
            text,
            metadata,
            ..
        } => EventEnvelope {
            id,
            r#type: USER_MESSAGE_CREATED.to_owned(),
            source_session_id: Some(session_id.as_str().to_owned()),
            data: json!({
                "message": { "text": text },
                "userMessageCount": user_message_count,
                "session": {
                    "id": session_id.as_str(),
                    "metadata": metadata,
                }
            }),
        },
    }
}
