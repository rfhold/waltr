//! Shared raw and effective provider-neutral session context materialization.

#![allow(missing_docs)]

use crate::{CompletionTurn, PromptTurn};

use super::{SessionJournalEvent, SessionRecord, SessionSnapshot};

/// Context projection mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionContextMode {
    /// Unmodified rollback-effective durable conversation.
    Raw,
    /// Provider-visible conversation after active replacement masks.
    Effective,
}

/// Provider-neutral prompt and completion context for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMaterializedContext {
    pub current_prompt: Option<String>,
    pub prompt_turns: Vec<PromptTurn>,
    pub turns: Vec<CompletionTurn>,
    pub next_turn: u64,
}

impl SessionRecord {
    /// Materialize raw or mask-effective provider-neutral context.
    #[must_use]
    pub fn materialize_context(&self, mode: SessionContextMode) -> SessionMaterializedContext {
        materialize_snapshot(self, self.snapshot(), mode)
    }
}

fn materialize_snapshot(
    record: &SessionRecord,
    mut snapshot: SessionSnapshot,
    mode: SessionContextMode,
) -> SessionMaterializedContext {
    if mode == SessionContextMode::Effective {
        for active in record.active_session_masks() {
            let mask = active.mask;
            let selected_turn = completed_turn_at(record, mask.selector.cursor)
                .expect("active masks are validated at durable mutation time");
            let earliest_turn = snapshot
                .prompt_turns
                .iter()
                .filter(|prompt| prompt.turn <= selected_turn)
                .map(|prompt| prompt.turn)
                .chain(
                    snapshot
                        .turns
                        .iter()
                        .filter(|turn| turn.turn <= selected_turn)
                        .map(|turn| turn.turn),
                )
                .min()
                .unwrap_or(0);
            snapshot
                .prompt_turns
                .retain(|prompt| prompt.turn > selected_turn);
            snapshot.turns.retain(|turn| turn.turn > selected_turn);
            snapshot.prompt_turns.insert(
                0,
                PromptTurn {
                    turn: earliest_turn,
                    prompt: mask.replacement.text,
                },
            );
        }
    }
    let current_prompt = snapshot
        .prompt_turns
        .last()
        .map(|prompt| prompt.prompt.clone());
    SessionMaterializedContext {
        current_prompt,
        prompt_turns: snapshot.prompt_turns,
        turns: snapshot.turns,
        next_turn: snapshot.turn,
    }
}

pub(super) fn completed_turn_at(
    record: &SessionRecord,
    cursor: crate::TimelineCursor,
) -> Option<u64> {
    let mut latest = None;
    for event in record.effective_journal_events() {
        match event {
            SessionJournalEvent::Timeline(event) => latest = Some(event.cursor),
            SessionJournalEvent::TurnFinished { turn } if latest == Some(cursor) => {
                return Some(*turn);
            }
            _ => {}
        }
    }
    None
}
