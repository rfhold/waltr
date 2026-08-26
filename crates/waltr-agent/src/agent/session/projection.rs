//! Agent-owned raw and mask-effective session timeline projection.

use crate::{
    AgentTimelineEvent, CHANNEL_USER_TEXT, TimelineChannel, TimelineContent, TimelineCursor,
    TimelineEventKind, TimelineQuery, TimelineStatus,
};

use super::{Revision, SessionId, SessionMaskId, SessionRecord};

/// Timeline interpretation selected at the agent boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTimelineMode {
    /// Unmodified rollback-effective durable events.
    Raw,
    /// Durable events after applying active replacement masks.
    Effective,
}

/// One projected timeline page with explicit raw pagination state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTimelinePage {
    /// Events visible in the selected projection.
    pub events: Vec<SessionTimelineProjectionEvent>,
    /// Monotonic session revision from which this projection was derived.
    pub projection_revision: Revision,
    /// Raw cursor consumed at the continuation edge of this page.
    pub raw_page_boundary: Option<TimelineCursor>,
    /// Whether another raw page exists beyond `raw_page_boundary`.
    pub has_more: bool,
}

/// One event and its typed presentation metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTimelineProjectionEvent {
    /// Durable or synthetic event body.
    pub event: SessionTimelineProjectedEvent,
    /// Presentation and source-action classification.
    pub presentation: SessionTimelinePresentation,
}

impl SessionTimelineProjectionEvent {
    /// Return the ordering cursor of this projected event.
    #[must_use]
    pub const fn cursor(&self) -> TimelineCursor {
        match &self.event {
            SessionTimelineProjectedEvent::Durable(event) => event.cursor,
            SessionTimelineProjectedEvent::Synthetic(event) => event.cursor,
        }
    }
}

/// Projected event body. Synthetic events intentionally have no durable item identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionTimelineProjectedEvent {
    /// Canonical durable timeline event.
    Durable(AgentTimelineEvent),
    /// Agent-owned synthetic display event.
    Synthetic(SessionSyntheticTimelineEvent),
}

/// Synthetic timeline event fields shared with durable display events where meaningful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSyntheticTimelineEvent {
    /// Administrative cursor used for stable ordering.
    pub cursor: TimelineCursor,
    /// Owning session.
    pub session_id: SessionId,
    /// Completed source turn selected by the mask.
    pub turn: u64,
    /// Terminal snapshot event kind.
    pub kind: TimelineEventKind,
    /// User-input display channel.
    pub channel: TimelineChannel,
    /// Durable replacement text.
    pub content: TimelineContent,
    /// Terminal presentation status.
    pub status: TimelineStatus,
}

/// Typed event presentation and opaque projection identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionTimelinePresentation {
    /// Canonical durable event whose body carries its item identity.
    Durable,
    /// Synthetic replacement with no canonical item identity or source actions.
    MaskReplacement {
        /// Opaque durable mask identity.
        mask_id: SessionMaskId,
    },
}

pub(super) fn project_timeline(
    record: &SessionRecord,
    mode: SessionTimelineMode,
    query: TimelineQuery,
) -> SessionTimelinePage {
    match mode {
        SessionTimelineMode::Raw => project_raw(record, query),
        SessionTimelineMode::Effective => project_effective(record, query),
    }
}

fn project_raw(record: &SessionRecord, query: TimelineQuery) -> SessionTimelinePage {
    let raw = super::storage::timeline_query_slice(record, &query);
    let raw_page_boundary = continuation_boundary(&raw, &query);
    let has_more = raw_page_boundary.is_some_and(|boundary| {
        let continuation = TimelineQuery {
            cursor: Some(boundary),
            limit: 1,
            ..query.clone()
        };
        !super::storage::timeline_query_slice(record, &continuation).is_empty()
    });
    SessionTimelinePage {
        events: raw.into_iter().map(durable_projection).collect(),
        projection_revision: record.revision,
        raw_page_boundary,
        has_more,
    }
}

fn project_effective(record: &SessionRecord, query: TimelineQuery) -> SessionTimelinePage {
    if query.limit == 0 {
        return SessionTimelinePage {
            events: Vec::new(),
            projection_revision: record.revision,
            raw_page_boundary: None,
            has_more: false,
        };
    }
    let active = record
        .active_session_masks()
        .into_iter()
        .max_by_key(|active| active.mask.selector.cursor);
    let selected_through = active.as_ref().map(|active| active.mask.selector.cursor);
    let raw = record
        .timeline_events()
        .filter(|event| match query.direction {
            crate::TimelineDirection::Forward => {
                query.cursor.is_none_or(|cursor| event.cursor > cursor)
            }
            crate::TimelineDirection::Backward => {
                query.cursor.is_none_or(|cursor| event.cursor < cursor)
            }
        })
        .cloned()
        .collect::<Vec<_>>();
    let (events, raw_page_boundary, has_more) = match query.direction {
        crate::TimelineDirection::Forward => project_effective_iter(
            record,
            &query,
            active.as_ref(),
            selected_through,
            raw.into_iter(),
        ),
        crate::TimelineDirection::Backward => project_effective_iter(
            record,
            &query,
            active.as_ref(),
            selected_through,
            raw.into_iter().rev(),
        ),
    };
    SessionTimelinePage {
        events,
        projection_revision: record.revision,
        raw_page_boundary,
        has_more,
    }
}

fn project_effective_iter(
    record: &SessionRecord,
    query: &TimelineQuery,
    active: Option<&super::ActiveSessionMask>,
    selected_through: Option<TimelineCursor>,
    raw: impl Iterator<Item = AgentTimelineEvent>,
) -> (
    Vec<SessionTimelineProjectionEvent>,
    Option<TimelineCursor>,
    bool,
) {
    let mut raw = raw.peekable();
    let mut events = Vec::new();
    let mut raw_page_boundary = None;
    while events.len() < query.limit {
        let Some(event) = raw.next() else {
            break;
        };
        raw_page_boundary = Some(event.cursor);
        if selected_through.is_some_and(|cursor| event.cursor <= cursor) {
            if active.is_some_and(|active| active.mask.selector.cursor == event.cursor)
                && channel_selected(query, CHANNEL_USER_TEXT)
            {
                events.push(mask_replacement_projection(
                    record,
                    active.expect("active mask was checked"),
                ));
            }
        } else if event_channel_selected(query, &event) {
            events.push(durable_projection(event));
        }
    }
    events.sort_by_key(SessionTimelineProjectionEvent::cursor);
    (events, raw_page_boundary, raw.peek().is_some())
}

fn mask_replacement_projection(
    record: &SessionRecord,
    active: &super::ActiveSessionMask,
) -> SessionTimelineProjectionEvent {
    let turn = super::materialize::completed_turn_at(record, active.mask.selector.cursor)
        .expect("active masks are validated at durable mutation time");
    SessionTimelineProjectionEvent {
        event: SessionTimelineProjectedEvent::Synthetic(SessionSyntheticTimelineEvent {
            cursor: active.mutation_cursor,
            session_id: record.id.clone(),
            turn,
            kind: TimelineEventKind::ItemFinished,
            channel: TimelineChannel::from(CHANNEL_USER_TEXT),
            content: TimelineContent::TextSnapshot {
                text: active.mask.replacement.text.clone(),
            },
            status: TimelineStatus::Finished,
        }),
        presentation: SessionTimelinePresentation::MaskReplacement {
            mask_id: active.mask.id.clone(),
        },
    }
}

fn durable_projection(event: AgentTimelineEvent) -> SessionTimelineProjectionEvent {
    SessionTimelineProjectionEvent {
        event: SessionTimelineProjectedEvent::Durable(event),
        presentation: SessionTimelinePresentation::Durable,
    }
}

fn continuation_boundary(
    raw: &[AgentTimelineEvent],
    query: &TimelineQuery,
) -> Option<TimelineCursor> {
    match query.direction {
        crate::TimelineDirection::Forward => raw.last(),
        crate::TimelineDirection::Backward => raw.first(),
    }
    .map(|event| event.cursor)
}

fn channel_selected(query: &TimelineQuery, channel: &str) -> bool {
    query.channels.as_ref().is_none_or(|channels| {
        channels
            .iter()
            .any(|candidate| candidate.as_str() == channel)
    })
}

fn event_channel_selected(query: &TimelineQuery, event: &AgentTimelineEvent) -> bool {
    query.channels.as_ref().is_none_or(|channels| {
        event
            .channel
            .as_ref()
            .is_some_and(|channel| channels.contains(channel))
    })
}

#[cfg(test)]
mod tests {
    use crate::{PendingTimelineEvent, PendingTimelineItemId, TimelineDirection, TimelineItemId};

    use super::*;
    use crate::agent::session::{
        ReplacementUserContent, SessionJournalEvent, SessionMaskAddition, SessionMaskMutation,
        SessionRollbackTarget, ThroughCursor,
    };

    fn assigned(event: PendingTimelineEvent, cursor: u64) -> AgentTimelineEvent {
        event.with_cursor(TimelineCursor::new(cursor))
    }

    fn record_with_later_turn() -> (SessionRecord, usize, SessionMaskId) {
        let mut record = SessionRecord::new("projected");
        record.append_journal_event(SessionJournalEvent::PromptAppended {
            prompt: "first question".to_owned(),
        });
        record.append_journal_event(SessionJournalEvent::Timeline(assigned(
            PendingTimelineEvent::user_text_snapshot(
                record.id.clone(),
                0,
                PendingTimelineItemId::New,
                "first question",
            ),
            1,
        )));
        record.append_journal_event(SessionJournalEvent::Timeline(assigned(
            PendingTimelineEvent::assistant_text_snapshot(
                record.id.clone(),
                0,
                PendingTimelineItemId::New,
                "first answer",
            ),
            2,
        )));
        record.append_journal_event(SessionJournalEvent::TurnFinished { turn: 0 });
        let rollback_len = record.journal.len();
        let mut mutation = SessionMaskMutation::new(Some(TimelineCursor::new(2)));
        mutation.add.push(SessionMaskAddition::new(
            "summary",
            ThroughCursor::new(TimelineCursor::new(2)),
            ReplacementUserContent::new("compact summary"),
        ));
        record.revision = Revision::new(1);
        let mask_id =
            super::super::mask::mutate_record_masks(&mut record, Revision::new(1), &mutation)
                .unwrap()
                .added_masks
                .remove(0)
                .id;
        record.append_journal_event(SessionJournalEvent::PromptAppended {
            prompt: "second question".to_owned(),
        });
        record.append_journal_event(SessionJournalEvent::Timeline(assigned(
            PendingTimelineEvent::user_text_snapshot(
                record.id.clone(),
                1,
                PendingTimelineItemId::New,
                "second question",
            ),
            4,
        )));
        record.append_journal_event(SessionJournalEvent::Timeline(assigned(
            PendingTimelineEvent::assistant_text_snapshot(
                record.id.clone(),
                1,
                PendingTimelineItemId::New,
                "second answer",
            ),
            5,
        )));
        record.append_journal_event(SessionJournalEvent::TurnFinished { turn: 1 });
        (record, rollback_len, mask_id)
    }

    #[test]
    fn raw_projection_has_exact_raw_events_and_no_synthetic_identity() {
        let (record, _, _) = record_with_later_turn();
        let query = TimelineQuery::forward(None, 16);
        let expected = super::super::storage::timeline_query_slice(&record, &query);

        let page = project_timeline(&record, SessionTimelineMode::Raw, query);

        let actual = page
            .events
            .into_iter()
            .map(|projected| match projected.event {
                SessionTimelineProjectedEvent::Durable(event) => event,
                SessionTimelineProjectedEvent::Synthetic(_) => panic!("synthetic raw event"),
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert_eq!(page.raw_page_boundary, Some(TimelineCursor::new(5)));
        assert!(!page.has_more);
    }

    #[test]
    fn effective_projection_replaces_prefix_and_orders_before_later_submission() {
        let (record, _, mask_id) = record_with_later_turn();

        let page = project_timeline(
            &record,
            SessionTimelineMode::Effective,
            TimelineQuery::forward(None, 16),
        );

        assert_eq!(
            page.events
                .iter()
                .map(SessionTimelineProjectionEvent::cursor)
                .collect::<Vec<_>>(),
            [
                TimelineCursor::new(3),
                TimelineCursor::new(4),
                TimelineCursor::new(5)
            ]
        );
        assert!(matches!(
            &page.events[0],
            SessionTimelineProjectionEvent {
                event: SessionTimelineProjectedEvent::Synthetic(SessionSyntheticTimelineEvent {
                    turn: 0,
                    content: TimelineContent::TextSnapshot { text },
                    ..
                }),
                presentation: SessionTimelinePresentation::MaskReplacement { mask_id: actual },
            } if text == "compact summary" && actual == &mask_id
        ));
        assert!(matches!(
            page.events[1].presentation,
            SessionTimelinePresentation::Durable
        ));
    }

    #[test]
    fn effective_channel_filter_includes_replacement_only_for_user_text() {
        let (record, _, _) = record_with_later_turn();
        let assistant = project_timeline(
            &record,
            SessionTimelineMode::Effective,
            TimelineQuery::forward(None, 16)
                .with_channels([TimelineChannel::from(crate::CHANNEL_ASSISTANT_TEXT)]),
        );
        let user = project_timeline(
            &record,
            SessionTimelineMode::Effective,
            TimelineQuery::forward(None, 16)
                .with_channels([TimelineChannel::from(CHANNEL_USER_TEXT)]),
        );

        assert_eq!(assistant.events.len(), 1);
        assert!(matches!(
            assistant.events[0].event,
            SessionTimelineProjectedEvent::Durable(_)
        ));
        assert_eq!(
            user.events
                .iter()
                .map(SessionTimelineProjectionEvent::cursor)
                .collect::<Vec<_>>(),
            [TimelineCursor::new(3), TimelineCursor::new(4)]
        );
    }

    #[test]
    fn fully_masked_raw_events_advance_to_the_synthetic_entry() {
        let (mut record, rollback_len, _) = record_with_later_turn();
        record.append_journal_event(SessionJournalEvent::Rollback {
            target: SessionRollbackTarget {
                journal_len: rollback_len as u64,
                after_cursor: Some(TimelineCursor::new(2)),
                next_turn: 1,
                remove_prompt_index: None,
            },
        });
        let mut mutation = SessionMaskMutation::new(Some(TimelineCursor::new(2)));
        mutation.add.push(SessionMaskAddition::new(
            "replacement",
            ThroughCursor::new(TimelineCursor::new(2)),
            ReplacementUserContent::new("replacement"),
        ));
        super::super::mask::mutate_record_masks(&mut record, Revision::new(2), &mutation).unwrap();

        let page = project_timeline(
            &record,
            SessionTimelineMode::Effective,
            TimelineQuery::forward(None, 1),
        );

        assert_eq!(page.raw_page_boundary, Some(TimelineCursor::new(2)));
        assert!(!page.has_more);
        assert!(matches!(
            page.events.as_slice(),
            [SessionTimelineProjectionEvent {
                event: SessionTimelineProjectedEvent::Synthetic(_),
                ..
            }]
        ));
    }

    #[test]
    fn forward_limit_one_budgets_synthetic_before_later_filtered_durable_event() {
        let (record, _, _) = record_with_later_turn();
        let query = |cursor| {
            TimelineQuery::forward(cursor, 1)
                .with_channels([TimelineChannel::from(CHANNEL_USER_TEXT)])
        };

        let replacement = project_timeline(&record, SessionTimelineMode::Effective, query(None));
        let later = project_timeline(
            &record,
            SessionTimelineMode::Effective,
            query(replacement.raw_page_boundary),
        );
        let terminal = project_timeline(
            &record,
            SessionTimelineMode::Effective,
            query(later.raw_page_boundary),
        );

        assert_eq!(replacement.events.len(), 1);
        assert!(matches!(
            replacement.events[0].event,
            SessionTimelineProjectedEvent::Synthetic(_)
        ));
        assert_eq!(replacement.raw_page_boundary, Some(TimelineCursor::new(2)));
        assert!(replacement.has_more);
        assert_eq!(later.events.len(), 1);
        assert!(matches!(
            &later.events[0].event,
            SessionTimelineProjectedEvent::Durable(event)
                if event.cursor == TimelineCursor::new(4)
        ));
        assert_eq!(later.raw_page_boundary, Some(TimelineCursor::new(4)));
        assert!(later.has_more);
        assert!(terminal.events.is_empty());
        assert_eq!(terminal.raw_page_boundary, Some(TimelineCursor::new(5)));
        assert!(!terminal.has_more);
    }

    #[test]
    fn backward_limit_one_budgets_later_filtered_durable_before_synthetic() {
        let (record, _, _) = record_with_later_turn();
        let query = |cursor| TimelineQuery {
            direction: TimelineDirection::Backward,
            cursor,
            limit: 1,
            channels: Some(vec![TimelineChannel::from(CHANNEL_USER_TEXT)]),
        };

        let later = project_timeline(&record, SessionTimelineMode::Effective, query(None));
        let replacement = project_timeline(
            &record,
            SessionTimelineMode::Effective,
            query(later.raw_page_boundary),
        );
        let terminal = project_timeline(
            &record,
            SessionTimelineMode::Effective,
            query(replacement.raw_page_boundary),
        );

        assert_eq!(later.events.len(), 1);
        assert!(matches!(
            &later.events[0].event,
            SessionTimelineProjectedEvent::Durable(event)
                if event.cursor == TimelineCursor::new(4)
        ));
        assert_eq!(later.raw_page_boundary, Some(TimelineCursor::new(4)));
        assert!(later.has_more);
        assert_eq!(replacement.events.len(), 1);
        assert!(matches!(
            replacement.events[0].event,
            SessionTimelineProjectedEvent::Synthetic(_)
        ));
        assert_eq!(replacement.raw_page_boundary, Some(TimelineCursor::new(2)));
        assert!(replacement.has_more);
        assert!(terminal.events.is_empty());
        assert_eq!(terminal.raw_page_boundary, Some(TimelineCursor::new(1)));
        assert!(!terminal.has_more);
    }

    #[test]
    fn subtraction_and_rollback_restore_raw_prefix_with_new_projection_revisions() {
        let (mut record, rollback_len, mask_id) = record_with_later_turn();
        let masked_revision = project_timeline(
            &record,
            SessionTimelineMode::Effective,
            TimelineQuery::forward(None, 16),
        )
        .projection_revision;
        let mut subtraction = SessionMaskMutation::new(Some(TimelineCursor::new(5)));
        subtraction.subtract.ids.push(mask_id);
        record.revision = Revision::new(2);
        super::super::mask::mutate_record_masks(&mut record, Revision::new(2), &subtraction)
            .unwrap();
        let subtracted = project_timeline(
            &record,
            SessionTimelineMode::Effective,
            TimelineQuery::forward(None, 16),
        );

        assert!(subtracted.projection_revision > masked_revision);
        assert_eq!(subtracted.events.len(), 4);
        assert!(
            subtracted
                .events
                .iter()
                .all(|event| matches!(event.event, SessionTimelineProjectedEvent::Durable(_)))
        );

        record.append_journal_event(SessionJournalEvent::Rollback {
            target: SessionRollbackTarget {
                journal_len: rollback_len as u64,
                after_cursor: Some(TimelineCursor::new(2)),
                next_turn: 1,
                remove_prompt_index: None,
            },
        });
        record.revision = Revision::new(3);
        let rolled_back = project_timeline(
            &record,
            SessionTimelineMode::Effective,
            TimelineQuery::forward(None, 16),
        );
        assert!(rolled_back.projection_revision > subtracted.projection_revision);
        assert_eq!(rolled_back.events.len(), 2);
    }

    #[test]
    fn backward_page_uses_earliest_raw_cursor_as_continuation() {
        let (record, _, _) = record_with_later_turn();
        let page = project_timeline(
            &record,
            SessionTimelineMode::Effective,
            TimelineQuery {
                direction: TimelineDirection::Backward,
                cursor: None,
                limit: 2,
                channels: None,
            },
        );
        assert_eq!(page.raw_page_boundary, Some(TimelineCursor::new(4)));
        assert!(page.has_more);
        assert_eq!(
            page.events
                .iter()
                .map(SessionTimelineProjectionEvent::cursor)
                .collect::<Vec<_>>(),
            [TimelineCursor::new(4), TimelineCursor::new(5)]
        );
    }

    #[test]
    fn durable_presentation_retains_canonical_event_identity() {
        let (record, _, _) = record_with_later_turn();
        let page = project_timeline(
            &record,
            SessionTimelineMode::Raw,
            TimelineQuery::forward(None, 1),
        );
        let event = &page.events[0];
        assert!(matches!(
            event,
            SessionTimelineProjectionEvent {
                event: SessionTimelineProjectedEvent::Durable(durable),
                presentation: SessionTimelinePresentation::Durable,
            } if durable.item_id == TimelineItemId::from_first_cursor(TimelineCursor::new(1))
        ));
    }
}
