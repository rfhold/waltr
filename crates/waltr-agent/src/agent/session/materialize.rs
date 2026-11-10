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
    /// Opaque annotations for retained canonical items, not provider request content.
    pub item_metadata: super::ItemMetadata,
}

impl SessionRecord {
    /// Materialize raw or mask-effective provider-neutral context.
    #[must_use]
    pub fn materialize_context(&self, mode: SessionContextMode) -> SessionMaterializedContext {
        if mode == SessionContextMode::Raw {
            let mut context = materialize_snapshot(self.snapshot());
            context.item_metadata = metadata_for_journal(self, &self.effective_journal_events().into_iter().cloned().collect::<Vec<_>>());
            return context;
        }
        let mut journal = self
            .effective_journal_events()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        for mask in self.active_masks() {
            if let Some(ids) = &mask.item_ids {
                super::copy::apply_item_replacement(
                    &mut journal,
                    ids,
                    Some(&mask.replacement.text),
                );
            } else if let Some(turn) = completed_turn_at(self, mask.selector.cursor) {
                super::copy::apply_prefix_replacement(&mut journal, turn, &mask.replacement.text);
            }
        }
        let mut context = materialize_snapshot(SessionSnapshot::from_journal(self.id.clone(), &journal));
        context.item_metadata = metadata_for_journal(self, &journal);
        context
    }
}

fn materialize_snapshot(snapshot: SessionSnapshot) -> SessionMaterializedContext {
    let current_prompt = snapshot
        .prompt_turns
        .last()
        .map(|prompt| prompt.prompt.clone());
    SessionMaterializedContext {
        current_prompt,
        prompt_turns: snapshot.prompt_turns,
        turns: snapshot.turns,
        next_turn: snapshot.turn,
        item_metadata: Default::default(),
    }
}

fn metadata_for_journal(record: &SessionRecord, journal: &[SessionJournalEvent]) -> super::ItemMetadata {
    let ids = journal.iter().filter_map(|event| match event {
        SessionJournalEvent::Timeline(event) => Some(&event.item_id), _ => None,
    }).collect::<std::collections::HashSet<_>>();
    record.metadata.item_values.iter().filter(|(id, _)| ids.contains(id))
        .map(|(id, values)| (id.clone(), values.clone())).collect()
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
