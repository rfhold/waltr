//! Immutable projection of the latest completed durable turn.

use std::{error::Error, fmt};

use crate::{TimelineContent, TimelineCursor};

use super::{
    InferenceSettings, SessionCopy, SessionId, SessionJournalEvent, SessionRecord,
    SessionStoreError,
};
use crate::definition::AgentDefinitionBundle;

/// Explicit resource bounds for completed-session capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCaptureLimits {
    /// Maximum physical source journal events inspected.
    pub max_journal_events: usize,
    /// Maximum projected prompts plus completion turns.
    pub max_turns: usize,
    /// Maximum projected prompt and completion items.
    pub max_items: usize,
    /// Maximum JSON-encoded provider-neutral projection size.
    pub max_encoded_bytes: usize,
}

impl Default for SessionCaptureLimits {
    fn default() -> Self {
        Self {
            max_journal_events: 100_000,
            max_turns: 10_000,
            max_items: 100_000,
            max_encoded_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Self-contained immutable capture through one completed provider turn.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletedSessionCapture {
    /// Durable source identity.
    pub source_id: SessionId,
    /// Last effective timeline cursor at the completed boundary, when present.
    pub through_cursor: TimelineCursor,
    /// Last completed provider turn.
    pub completed_turn: u64,
    /// Exact durable inference settings.
    pub inference_settings: InferenceSettings,
    /// Exact self-contained agent definition.
    pub agent_definition: AgentDefinitionBundle,
    /// Validated completed context ready for atomic destination creation.
    pub copy: SessionCopy,
}

/// Failure to capture a completed durable conversation.
#[derive(Debug)]
pub enum SessionCaptureError {
    /// Required durable state is absent or unsupported.
    FailedPrecondition(String),
    /// An explicit projection bound was exceeded.
    ResourceExhausted(String),
    /// Loading the source record failed.
    Store(SessionStoreError),
}

impl fmt::Display for SessionCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FailedPrecondition(message) | Self::ResourceExhausted(message) => {
                formatter.write_str(message)
            }
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for SessionCaptureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::FailedPrecondition(_) | Self::ResourceExhausted(_) => None,
        }
    }
}

impl From<SessionStoreError> for SessionCaptureError {
    fn from(error: SessionStoreError) -> Self {
        Self::Store(error)
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn capture_record(
    record: &SessionRecord,
    limits: SessionCaptureLimits,
) -> Result<CompletedSessionCapture, SessionCaptureError> {
    capture_record_mode(record, limits, false)
}

pub(super) fn capture_record_effective(
    record: &SessionRecord,
    limits: SessionCaptureLimits,
) -> Result<CompletedSessionCapture, SessionCaptureError> {
    capture_record_mode(record, limits, true)
}

#[allow(clippy::too_many_lines)]
fn capture_record_mode(
    record: &SessionRecord,
    limits: SessionCaptureLimits,
    effective_context: bool,
) -> Result<CompletedSessionCapture, SessionCaptureError> {
    check_bound(
        record.journal.len(),
        limits.max_journal_events,
        "source journal events",
    )?;
    let effective = record.effective_journal_events();
    let boundary = effective
        .iter()
        .rposition(|event| matches!(event, SessionJournalEvent::TurnFinished { .. }))
        .ok_or_else(|| {
            SessionCaptureError::FailedPrecondition(
                "session has no completed provider turn".to_owned(),
            )
        })?;
    let completed_turn = match effective[boundary] {
        SessionJournalEvent::TurnFinished { turn } => *turn,
        _ => unreachable!("completed boundary was selected by variant"),
    };
    let selected = &effective[..=boundary];
    if selected.iter().any(|event| {
        matches!(event, SessionJournalEvent::Timeline(timeline) if matches!(timeline.content, TimelineContent::ContentRef { .. }))
    }) {
        return Err(SessionCaptureError::FailedPrecondition(
            "completed conversation contains an unresolved durable content reference".to_owned(),
        ));
    }
    let through_cursor = selected
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionJournalEvent::Timeline(event) => Some(event.cursor),
            _ => None,
        })
        .ok_or_else(|| {
            SessionCaptureError::FailedPrecondition(
                "completed conversation has no exact timeline cursor boundary".to_owned(),
            )
        })?;
    let copy = if effective_context {
        SessionCopy::from_record_effective(record, through_cursor)
    } else {
        SessionCopy::from_record(record, through_cursor)
    }
    .map_err(|error| {
        SessionCaptureError::FailedPrecondition(format!(
            "completed conversation cannot be copied: {error}"
        ))
    })?;
    let (projected_turns, projected_items) = copy.context_counts();
    check_bound(projected_turns, limits.max_turns, "projected turns")?;
    check_bound(projected_items, limits.max_items, "projected items")?;
    let encoded_len = copy.encoded_len().map_err(|error| {
        SessionCaptureError::FailedPrecondition(format!(
            "completed conversation cannot be encoded: {error}"
        ))
    })?;
    check_bound(
        encoded_len,
        limits.max_encoded_bytes,
        "encoded projected-context bytes",
    )?;
    let inference_settings = record.inference_settings.clone().ok_or_else(|| {
        SessionCaptureError::FailedPrecondition(
            "session has no exact durable inference settings".to_owned(),
        )
    })?;
    let agent_definition = record.agent_definition.clone().ok_or_else(|| {
        SessionCaptureError::FailedPrecondition(
            "session has no exact durable agent definition".to_owned(),
        )
    })?;
    Ok(CompletedSessionCapture {
        source_id: record.id.clone(),
        through_cursor,
        completed_turn,
        inference_settings,
        agent_definition,
        copy,
    })
}

fn check_bound(actual: usize, limit: usize, name: &str) -> Result<(), SessionCaptureError> {
    if actual > limit {
        Err(SessionCaptureError::ResourceExhausted(format!(
            "{name} exceed capture limit ({actual} > {limit})"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::agent::session::SessionRollbackTarget;
    use crate::{
        InferenceProviderSettings, ModelApi, PendingTimelineEvent, PendingTimelineItemId,
        ProviderTurnMetadata, ThinkingBlock, ThinkingBlockKind, TimelineCursor, TimelineItemId,
        ToolCall, ToolResult, TurnItem,
    };
    use serde_json::json;

    use super::*;

    fn record() -> SessionRecord {
        let mut record = SessionRecord::new("source");
        record.inference_settings = Some(InferenceSettings {
            provider: InferenceProviderSettings {
                key: "provider".to_owned(),
                api: ModelApi::OpenAiResponses,
                url: "https://provider.example/v1".to_owned(),
                model_id: "model".to_owned(),
                model_cost: None,
                headers: BTreeMap::new(),
                oauth: None,
                reasoning_effort: None,
            },
        });
        record.agent_definition = Some(AgentDefinitionBundle::new(
            "agent",
            vec![
                "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: agent }\nspec: {}\n"
                    .to_owned(),
            ],
        ));
        record
    }

    fn assistant(id: &SessionId, turn: u64, cursor: u64, text: &str) -> SessionJournalEvent {
        SessionJournalEvent::Timeline(
            PendingTimelineEvent::assistant_text_snapshot(
                id.clone(),
                turn,
                PendingTimelineItemId::New,
                text,
            )
            .with_cursor(TimelineCursor::new(cursor)),
        )
    }

    fn tool_call(id: &SessionId, turn: u64, cursor: u64, call: ToolCall) -> SessionJournalEvent {
        SessionJournalEvent::Timeline(
            PendingTimelineEvent::tool_call(id.clone(), turn, PendingTimelineItemId::New, call)
                .with_cursor(TimelineCursor::new(cursor)),
        )
    }

    fn tool_result(
        id: &SessionId,
        turn: u64,
        cursor: u64,
        call_cursor: u64,
        result: ToolResult,
    ) -> SessionJournalEvent {
        SessionJournalEvent::Timeline(
            PendingTimelineEvent::tool_result(
                id.clone(),
                turn,
                PendingTimelineItemId::New,
                TimelineItemId::from_first_cursor(TimelineCursor::new(call_cursor)),
                result,
            )
            .with_cursor(TimelineCursor::new(cursor)),
        )
    }

    fn materialize(capture: &CompletedSessionCapture) -> SessionRecord {
        let mut request = super::super::SessionCreateRequest::new("destination");
        request.start = super::super::SessionStart::Copy {
            copy: capture.copy.clone(),
            messages: Vec::new(),
        };
        request.materialize()
    }

    #[test]
    fn capture_uses_effective_completed_boundary_and_excludes_partial_work() {
        let mut record = record();
        let call = ToolCall::new("call-1", "lookup", json!({ "query": "waltr" }));
        let result = ToolResult::new("call-1", "lookup", "found");
        let thinking = ThinkingBlock {
            kind: ThinkingBlockKind::OpenAiResponses,
            text: "reasoning".to_owned(),
            payload: json!({ "id": "reasoning-1" }),
        };
        record.journal = vec![
            SessionJournalEvent::PromptAppended {
                prompt: "first".to_owned(),
            },
            SessionJournalEvent::ProviderThinkingBlockRecorded {
                turn: 0,
                block: thinking.clone(),
            },
            SessionJournalEvent::ProviderTurnMetadataRecorded {
                turn: 0,
                metadata: ProviderTurnMetadata {
                    provider: "provider".to_owned(),
                    url: "https://provider.example/v1".to_owned(),
                    model: "model".to_owned(),
                    usage: None,
                    cost: None,
                    resume: json!({ "id": "response-1" }),
                },
            },
            assistant(&record.id, 0, 1, "complete"),
            tool_call(&record.id, 0, 2, call.clone()),
            tool_result(&record.id, 0, 3, 2, result.clone()),
            SessionJournalEvent::TurnFinished { turn: 0 },
            SessionJournalEvent::PromptAppended {
                prompt: "partial prompt".to_owned(),
            },
            assistant(&record.id, 1, 4, "partial"),
        ];

        let capture = capture_record(&record, SessionCaptureLimits::default()).unwrap();

        assert_eq!(capture.completed_turn, 0);
        assert_eq!(capture.through_cursor, TimelineCursor::new(3));
        let copied = materialize(&capture);
        let snapshot = copied.snapshot();
        assert_eq!(snapshot.prompt_turns.len(), 1);
        assert_eq!(
            snapshot.turns[0].items,
            [
                TurnItem::AssistantText("complete".to_owned()),
                TurnItem::ToolCall(call),
                TurnItem::ToolResult(result),
            ]
        );
        assert_eq!(snapshot.turns[0].metadata, None);
        assert!(!copied.journal.iter().any(|event| matches!(
            event,
            SessionJournalEvent::ProviderThinkingBlockRecorded { .. }
                | SessionJournalEvent::ProviderTurnMetadataRecorded { .. }
        )));
    }

    #[test]
    fn rollback_selects_latest_remaining_effective_completion() {
        let mut record = record();
        record.journal = vec![
            SessionJournalEvent::PromptAppended {
                prompt: "first".to_owned(),
            },
            assistant(&record.id, 0, 1, "kept"),
            SessionJournalEvent::TurnFinished { turn: 0 },
            SessionJournalEvent::PromptAppended {
                prompt: "rolled back".to_owned(),
            },
            assistant(&record.id, 1, 2, "removed"),
            SessionJournalEvent::TurnInterrupted { turn: 1 },
            SessionJournalEvent::TurnFinished { turn: 1 },
            SessionJournalEvent::Rollback {
                target: SessionRollbackTarget {
                    journal_len: 3,
                    after_cursor: Some(TimelineCursor::new(1)),
                    next_turn: 1,
                    remove_prompt_index: None,
                },
            },
            SessionJournalEvent::PromptAppended {
                prompt: "valid retry".to_owned(),
            },
            assistant(&record.id, 1, 3, "valid"),
            SessionJournalEvent::TurnFinished { turn: 1 },
            assistant(&record.id, 2, 4, "active retry suffix"),
            SessionJournalEvent::TurnInterrupted { turn: 2 },
        ];

        let capture = capture_record(&record, SessionCaptureLimits::default()).unwrap();

        assert_eq!(capture.completed_turn, 1);
        assert_eq!(capture.through_cursor, TimelineCursor::new(3));
        let snapshot = materialize(&capture).snapshot();
        assert_eq!(
            snapshot
                .prompt_turns
                .iter()
                .map(|prompt| prompt.prompt.as_str())
                .collect::<Vec<_>>(),
            ["first", "valid retry"]
        );
        assert_eq!(
            snapshot.turns[1].assistant_texts().collect::<Vec<_>>(),
            ["valid"]
        );
    }

    #[test]
    fn capture_preconditions_and_bounds_are_typed() {
        let mut missing_turn = record();
        assert!(matches!(
            capture_record(&missing_turn, SessionCaptureLimits::default()),
            Err(SessionCaptureError::FailedPrecondition(_))
        ));
        missing_turn
            .journal
            .push(SessionJournalEvent::TurnFinished { turn: 0 });
        missing_turn.agent_definition = None;
        assert!(matches!(
            capture_record(&missing_turn, SessionCaptureLimits::default()),
            Err(SessionCaptureError::FailedPrecondition(_))
        ));
        assert!(matches!(
            capture_record(
                &missing_turn,
                SessionCaptureLimits {
                    max_journal_events: 0,
                    ..SessionCaptureLimits::default()
                }
            ),
            Err(SessionCaptureError::ResourceExhausted(_))
        ));
    }

    #[test]
    fn capture_is_immutable_after_source_changes() {
        let mut record = record();
        record.journal.push(SessionJournalEvent::PromptAppended {
            prompt: "original".to_owned(),
        });
        record.journal.push(assistant(&record.id, 0, 1, "answer"));
        record
            .journal
            .push(SessionJournalEvent::TurnFinished { turn: 0 });
        let capture = capture_record(&record, SessionCaptureLimits::default()).unwrap();
        record.journal.clear();

        assert_eq!(
            materialize(&capture).snapshot().prompt_turns[0].prompt,
            "original"
        );
    }

    #[test]
    fn effective_capture_limits_measure_masked_projection() {
        let mut record = record();
        record.journal = vec![
            SessionJournalEvent::PromptAppended {
                prompt: "first".to_owned(),
            },
            assistant(&record.id, 0, 1, "first answer"),
            SessionJournalEvent::TurnFinished { turn: 0 },
            SessionJournalEvent::PromptAppended {
                prompt: "second".to_owned(),
            },
            assistant(&record.id, 1, 2, "second answer"),
            SessionJournalEvent::TurnFinished { turn: 1 },
        ];
        let mut mutation = super::super::SessionMaskMutation::new(Some(TimelineCursor::new(2)));
        mutation.add.push(super::super::SessionMaskAddition::new(
            "summary",
            super::super::ThroughCursor::new(TimelineCursor::new(2)),
            super::super::ReplacementUserContent::new("compact summary"),
        ));
        super::super::mask::mutate_record_masks(
            &mut record,
            super::super::Revision::new(1),
            &mutation,
        )
        .unwrap();
        let limits = SessionCaptureLimits {
            max_turns: 1,
            max_items: 1,
            ..SessionCaptureLimits::default()
        };

        assert!(matches!(
            capture_record(&record, limits),
            Err(SessionCaptureError::ResourceExhausted(_))
        ));
        let capture = capture_record_effective(&record, limits).unwrap();
        assert_eq!(
            materialize(&capture).snapshot().prompts,
            ["compact summary"]
        );
        assert!(matches!(
            capture_record_effective(
                &record,
                SessionCaptureLimits {
                    max_encoded_bytes: 0,
                    ..limits
                }
            ),
            Err(SessionCaptureError::ResourceExhausted(_))
        ));
    }
}
