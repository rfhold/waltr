use std::sync::{Arc, Mutex};

use super::RunOutcomeSender;

/// Process-local admission only. The run owner performs all durable writes.
#[derive(Clone, Debug, Default)]
pub(crate) struct NextTurnInput(Arc<Mutex<State>>);

#[derive(Debug, Default)]
struct State {
    closed: bool,
    count: usize,
    bytes: usize,
    prompts: Vec<String>,
    outcomes: Vec<(u64, RunOutcomeSender)>,
}

impl NextTurnInput {
    pub(crate) fn for_input(input: &super::AgentSessionRunInput) -> Self {
        let result = Self::default();
        if matches!(input, super::AgentSessionRunInput::HostTool(_)) {
            drop(result.drain(true));
        }
        result
    }

    pub(crate) fn admit(&self, prompt: String, id: u64, outcome: RunOutcomeSender) -> bool {
        let mut state = self.0.lock().expect("next-turn input lock poisoned");
        if state.closed
            || state.count >= 64
            || prompt.len() > 16384
            || prompt.trim().is_empty()
            || state.bytes.saturating_add(prompt.len()) > 1048576
        {
            return false;
        }
        state.count += 1;
        state.bytes += prompt.len();
        state.prompts.push(prompt);
        state.outcomes.push((id, outcome));
        true
    }

    /// Closing an empty gate and admitting a prompt are one atomic ordering.
    pub(crate) fn drain(&self, completing: bool) -> Vec<String> {
        let mut state = self.0.lock().expect("next-turn input lock poisoned");
        if completing && state.prompts.is_empty() {
            state.closed = true;
        }
        std::mem::take(&mut state.prompts)
    }

    pub(crate) fn finish(&self) -> Vec<(u64, RunOutcomeSender)> {
        let mut state = self.0.lock().expect("next-turn input lock poisoned");
        state.closed = true;
        state.prompts.clear();
        std::mem::take(&mut state.outcomes)
    }
}
