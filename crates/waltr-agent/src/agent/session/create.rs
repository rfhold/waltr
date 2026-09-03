//! Durable session creation inputs and materialization.

use std::{collections::BTreeMap, error::Error, fmt};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    definition::AgentDefinitionBundle,
    timeline::{PendingTimelineEvent, PendingTimelineItemId, TimelineCursor},
};

use super::{
    InferenceSettings, Revision, SessionCopy, SessionId, SessionJournalEvent, SessionRecord,
    types::SessionMetadata,
};

/// Complete input for creating a durable session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCreateRequest {
    /// Caller-selected or preallocated session identity.
    pub id: SessionId,
    /// Durable inference settings captured for the session.
    pub inference_settings: Option<InferenceSettings>,
    /// Self-contained selected agent definition.
    pub agent_definition: Option<AgentDefinitionBundle>,
    /// Opaque host metadata preserved exactly.
    pub metadata_values: BTreeMap<String, Value>,
    /// Initial durable conversation state.
    pub start: SessionStart,
}

impl SessionCreateRequest {
    /// Create a fresh empty request for a caller-selected id.
    #[must_use]
    pub fn new(id: impl Into<SessionId>) -> Self {
        Self {
            id: id.into(),
            inference_settings: None,
            agent_definition: None,
            metadata_values: BTreeMap::new(),
            start: SessionStart::Fresh {
                messages: Vec::new(),
            },
        }
    }

    pub(crate) fn materialize(self) -> SessionRecord {
        let Self {
            id,
            inference_settings,
            agent_definition,
            metadata_values,
            start,
        } = self;
        let mut metadata = SessionMetadata::now();
        metadata.values = metadata_values;
        let (mut journal, mut cursor, turn, messages) = match start {
            SessionStart::Fresh { messages } => (Vec::new(), TimelineCursor::ZERO, 0, messages),
            SessionStart::Copy { copy, messages } => {
                let cursor = copy.boundary();
                let turn = copy.next_turn();
                (copy.materialize(&id), cursor, turn, messages)
            }
        };
        for message in messages {
            let text = message.into_text();
            journal.push(SessionJournalEvent::PromptAppended {
                prompt: text.clone(),
            });
            cursor = cursor.next();
            journal.push(SessionJournalEvent::Timeline(
                PendingTimelineEvent::user_text_snapshot(
                    id.clone(),
                    turn,
                    PendingTimelineItemId::New,
                    text,
                )
                .with_cursor(cursor),
            ));
        }

        SessionRecord {
            id,
            revision: Revision::ZERO,
            metadata,
            inference_settings,
            agent_definition,
            timeline_cursor_high_water: cursor,
            journal,
        }
    }
}

/// Initial state for a newly created session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStart {
    /// A new conversation with zero or more initial user messages.
    Fresh { messages: Vec<SessionStartMessage> },
    /// Context copied through a completed source boundary, followed by new user messages.
    Copy {
        copy: SessionCopy,
        messages: Vec<SessionStartMessage>,
    },
}

/// Validated user message materialized during session creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SessionStartMessage(String);

impl SessionStartMessage {
    /// Validate an exact user message.
    pub fn user(text: impl Into<String>) -> Result<Self, SessionStartMessageError> {
        let text = text.into();
        if text.is_empty() {
            return Err(SessionStartMessageError);
        }
        Ok(Self(text))
    }

    /// Borrow the exact user text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.0
    }

    fn into_text(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for SessionStartMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::user(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Invalid user start message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStartMessageError;

impl fmt::Display for SessionStartMessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("session start message must not be empty")
    }
}

impl Error for SessionStartMessageError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::TimelineContent;

    #[test]
    fn fresh_messages_materialize_at_revision_zero_in_exact_order() {
        let mut request = SessionCreateRequest::new("session-1");
        request.start = SessionStart::Fresh {
            messages: vec![
                SessionStartMessage::user("  first\nmessage  ").unwrap(),
                SessionStartMessage::user("second").unwrap(),
            ],
        };

        let record = request.materialize();
        let snapshot = record.snapshot();
        let timeline = record.timeline_events().collect::<Vec<_>>();

        assert_eq!(record.revision, Revision::ZERO);
        assert_eq!(snapshot.prompts, ["  first\nmessage  ", "second"]);
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[0].cursor, TimelineCursor::new(1));
        assert_eq!(timeline[1].cursor, TimelineCursor::new(2));
        assert!(matches!(
            &timeline[0].content,
            TimelineContent::TextSnapshot { text } if text == "  first\nmessage  "
        ));
        assert!(matches!(
            &timeline[1].content,
            TimelineContent::TextSnapshot { text } if text == "second"
        ));
    }

    #[test]
    fn start_message_rejects_empty_text_in_construction_and_deserialization() {
        assert_eq!(SessionStartMessage::user(""), Err(SessionStartMessageError));
        assert!(serde_json::from_str::<SessionStartMessage>("\"\"").is_err());
    }
}
