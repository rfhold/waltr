//! Agent state and transcript types.

use crate::{ThinkingBlock, ToolCall, ToolResult};

/// Accumulated agent state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentState {
    /// Ordered transcript events accumulated by the agent loop.
    pub events: Vec<AgentTranscriptEvent>,
    /// Provider-specific thinking/reasoning blocks accumulated for replay.
    pub thinking_blocks: Vec<ThinkingBlock>,
}

impl AgentState {
    /// Return concatenated assistant text from the last turn containing text.
    #[must_use]
    pub fn last_assistant_text(&self) -> String {
        let Some(last_text_turn) = self.events.iter().rev().find_map(|event| match event {
            AgentTranscriptEvent::Text { turn, .. } => Some(*turn),
            AgentTranscriptEvent::Thinking { .. }
            | AgentTranscriptEvent::ToolCall { .. }
            | AgentTranscriptEvent::ToolResult { .. } => None,
        }) else {
            return String::new();
        };

        self.events
            .iter()
            .filter_map(|event| match event {
                AgentTranscriptEvent::Text { turn, text } if *turn == last_text_turn => {
                    Some(text.as_str())
                }
                AgentTranscriptEvent::Text { .. }
                | AgentTranscriptEvent::Thinking { .. }
                | AgentTranscriptEvent::ToolCall { .. }
                | AgentTranscriptEvent::ToolResult { .. } => None,
            })
            .collect()
    }
}

/// Ordered persisted agent transcript event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTranscriptEvent {
    /// Thinking/reasoning emitted by the model.
    Thinking {
        /// Model turn that emitted this thinking text.
        turn: usize,
        /// Thinking/reasoning text delta emitted by the model.
        text: String,
    },
    /// Text emitted by the model.
    Text {
        /// Model turn that emitted this text.
        turn: usize,
        /// Text delta emitted by the model.
        text: String,
    },
    /// Tool call requested by the model.
    ToolCall {
        /// Model turn that requested the tool call.
        turn: usize,
        /// Tool call requested by the model.
        call: ToolCall,
    },
    /// Tool result returned by the host.
    ToolResult {
        /// Model turn that requested the corresponding tool call.
        turn: usize,
        /// Tool result returned by the host.
        result: ToolResult,
    },
}
