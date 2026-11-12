//! Private bounded mailboxes. A tool success acknowledges admission, never delivery.
use crate::routine::{
    definition::ToolStream,
    ports::{HelperSelection, PortError},
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, OnceLock},
};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct State {
    claimed: bool,
    closed: bool,
    failed: bool,
    bytes: usize,
    queue: VecDeque<(Value, usize)>,
}
struct Queue {
    declaration: ToolStream,
    state: Mutex<State>,
    consumer: AsyncMutex<()>,
    changed: Notify,
}
/// Shared only by the owning invocation's host and helper runner.
pub struct ToolStreams {
    queues: BTreeMap<String, Arc<Queue>>,
    closed_drained: OnceLock<bool>,
}
impl std::fmt::Debug for ToolStreams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolStreams").finish_non_exhaustive()
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Next {
    pub stream: String,
}
impl ToolStreams {
    pub(crate) fn declarations(&self) -> impl Iterator<Item = &ToolStream> {
        self.queues.values().map(|q| &q.declaration)
    }
    #[cfg(test)]
    pub(crate) fn consumer_pending(&self, name: &str) -> bool {
        self.queues
            .get(name)
            .is_some_and(|q| q.consumer.try_lock().is_err())
    }
    /// Construct fresh, empty, non-replayable invocation state.
    pub fn new(declarations: BTreeMap<String, ToolStream>) -> Arc<Self> {
        Arc::new(Self {
            queues: declarations
                .into_iter()
                .map(|(name, declaration)| {
                    (
                        name,
                        Arc::new(Queue {
                            declaration,
                            state: Mutex::new(State::default()),
                            consumer: AsyncMutex::new(()),
                            changed: Notify::new(),
                        }),
                    )
                })
                .collect(),
            closed_drained: OnceLock::new(),
        })
    }
    pub(crate) fn check(&self, name: &str) -> Result<(), PortError> {
        if self.queues.contains_key(name) {
            Ok(())
        } else {
            Err(PortError::Denied)
        }
    }
    pub(crate) fn claim(
        self: &Arc<Self>,
        selection: &HelperSelection,
    ) -> Result<Producer, PortError> {
        // Serialized by the runner's owner admission lock, before session creation.
        let queues: Vec<_> = self
            .queues
            .values()
            .filter(|q| &q.declaration.selection == selection)
            .cloned()
            .collect();
        let mut states = queues
            .iter()
            .map(|q| q.state.lock().map_err(|_| PortError::Unavailable))
            .collect::<Result<Vec<_>, _>>()?;
        if states.iter().any(|s| s.claimed) {
            return Err(PortError::Conflict);
        }
        if states.iter().any(|s| s.failed || s.closed) {
            return Err(PortError::Unavailable);
        }
        for state in &mut states {
            state.claimed = true;
        }
        drop(states);
        Ok(Producer {
            queues,
            completed: false,
        })
    }
    pub(crate) async fn next(
        &self,
        name: &str,
        cancel: CancellationToken,
    ) -> Result<Option<Value>, PortError> {
        let q = self.queues.get(name).ok_or(PortError::Denied)?;
        let _consumer = q.consumer.try_lock().map_err(|_| PortError::Conflict)?;
        loop {
            let changed = q.changed.notified();
            {
                let mut state = q.state.lock().map_err(|_| PortError::Unavailable)?;
                if state.failed || cancel.is_cancelled() {
                    return Err(PortError::Uncertain);
                }
                if let Some((value, bytes)) = state.queue.pop_front() {
                    state.bytes -= bytes;
                    q.changed.notify_waiters();
                    return Ok(Some(value));
                }
                if state.closed {
                    return Ok(None);
                }
            }
            tokio::select! { biased; () = cancel.cancelled() => return Err(PortError::Unavailable), () = changed => {} }
        }
    }
    pub(crate) fn drained(&self) -> bool {
        self.queues.values().all(|q| {
            q.state
                .lock()
                .is_ok_and(|s| !s.failed && (!s.claimed || s.closed && s.queue.is_empty()))
        })
    }
    pub(crate) fn cancel(&self) {
        self.closed_drained.get_or_init(|| self.drained());
        for q in self.queues.values() {
            if let Ok(mut state) = q.state.lock() {
                state.failed = true;
                state.queue.clear();
                state.bytes = 0;
            }
            q.changed.notify_waiters();
        }
    }
    pub(crate) fn was_drained(&self) -> bool {
        self.closed_drained.get().copied().unwrap_or(false)
    }
}
pub(crate) struct Producer {
    queues: Vec<Arc<Queue>>,
    completed: bool,
}
impl Producer {
    pub(crate) fn tools(&self, cancel: CancellationToken) -> Vec<crate::agent_host::Capability> {
        self.queues.iter().map(|queue| {
            let q = queue.clone();
            let cancel = cancel.clone();
            crate::agent_host::Capability::new_async(waltr_agent::ToolDefinition::new(
                &q.declaration.tool, &q.declaration.description,
                super::helper::capture_schema(&q.declaration.contract.schema),
            ), move |context| {
                let q = q.clone();
                let cancel = cancel.clone();
                Box::pin(async move {
                    let arguments = context.call().arguments.clone();
                    if !q.declaration.contract.accepts_host(&arguments, q.declaration.max_bytes) {
                        return Ok(context.result("arguments do not match stream contract").with_error());
                    }
                    let value = serde_json::json!({"callId":context.call().id,"tool":q.declaration.tool,"arguments":arguments});
                    if waltr_script::StructuredValue::new(
                        serde_json::json!({"status":"completed","value":&value}),
                        &waltr_script::Limits::default(),
                    ).is_err() {
                        return Ok(context.result("call exceeds Routine value bounds").with_error());
                    }
                    let bytes = serde_json::to_vec(&value).map_or(usize::MAX, |v| v.len());
                    if bytes > q.declaration.max_bytes {
                        return Ok(context.result("call exceeds stream byte budget").with_error());
                    }
                    loop {
                        let changed = q.changed.notified();
                        {
                            let mut state = q.state.lock().map_err(|_| waltr_agent::LlmError::Protocol { provider: "routine", message: "stream unavailable".into() })?;
                            if state.failed || state.closed || cancel.is_cancelled() || context.is_cancelled() {
                                return Ok(context.result("stream closed").with_error());
                            }
                            if state.queue.len() < q.declaration.max_calls && state.bytes + bytes <= q.declaration.max_bytes {
                                state.bytes += bytes;
                                state.queue.push_back((value, bytes));
                                q.changed.notify_waiters();
                                return Ok(context.result("queued"));
                            }
                        }
                        tokio::select! { biased;
                            () = cancel.cancelled() => return Ok(context.result("stream cancelled").with_error()),
                            () = context.cancellation_token().cancelled() => return Ok(context.result("stream cancelled").with_error()),
                            () = changed => {}
                        }
                    }
                })
            })
        }).collect()
    }
    pub(crate) fn complete(&mut self) {
        self.completed = true;
    }
}
#[cfg(test)]
#[path = "tool_streams_tests.rs"]
mod tests;
impl Drop for Producer {
    fn drop(&mut self) {
        for q in &self.queues {
            if let Ok(mut state) = q.state.lock() {
                state.closed = true;
                state.failed |= !self.completed;
            }
            q.changed.notify_waiters();
        }
    }
}
