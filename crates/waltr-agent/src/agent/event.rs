//! Agent event types.

use futures_util::Stream;

use crate::{Result, ToolCall, ToolResult};

use super::AgentState;

/// Boxed stream of agent loop events.
pub type BoxAgentEventStream<'a> =
    std::pin::Pin<Box<dyn Stream<Item = Result<AgentEvent>> + Send + 'a>>;

/// Ordered agent stream event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// Thinking/reasoning text emitted by the model in a specific turn.
    ThinkingDelta {
        /// Model turn that emitted this thinking text.
        turn: usize,
        /// Incremental thinking/reasoning text emitted by the model.
        delta: String,
    },
    /// Text emitted by the model in a specific turn.
    TextDelta {
        /// Model turn that emitted this text.
        turn: usize,
        /// Incremental text emitted by the model.
        delta: String,
    },
    /// Tool call requested by the model in a specific turn.
    ToolCall {
        /// Model turn that requested the tool call.
        turn: usize,
        /// Tool call requested by the model.
        call: ToolCall,
    },
    /// Tool result returned by the host in a specific turn.
    ToolResult {
        /// Model turn that requested the corresponding tool call.
        turn: usize,
        /// Tool result returned by the host.
        result: ToolResult,
    },
    /// Current model turn finished.
    TurnFinished {
        /// Model turn that finished.
        turn: usize,
    },
    /// Agent loop finished with final state.
    Finished {
        /// Final accumulated agent state.
        state: AgentState,
    },
}
