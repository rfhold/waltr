//! Durable, provider-neutral effects requested while a host prepares context.

use crate::{
    AgentTool, HostEffectExecutor, HostEffectFuture, HostEffectResult, LlmError, Result,
    SessionCommitReceipt, SessionId, SessionJournalEvent, SessionRecord, SessionStore, ToolCall,
    ToolDefinition, ToolExecutionContext, ToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, instrument::WithSubscriber};

pub(super) const RECEIPTS: &str = "host-effect-results";

#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    call: ToolCall,
    identity: Option<String>,
    declaration: ToolDefinition,
    result: ToolResult,
}

struct JournalState {
    uncertain: bool,
    invocations: std::collections::BTreeSet<String>,
    commit: SessionCommitReceipt,
    events: Vec<SessionJournalEvent>,
    receipts: BTreeMap<String, Receipt>,
}

pub(super) struct HostEffectJournal<S> {
    inner: Arc<EffectInner<S>>,
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

struct EffectInner<S> {
    store: S,
    session: SessionId,
    incarnation: String,
    cancellation: CancellationToken,
    state: Mutex<JournalState>,
}

impl<S> Drop for HostEffectJournal<S> {
    fn drop(&mut self) {
        self.inner.cancellation.cancel();
        let tasks = std::mem::take(
            self.tasks
                .get_mut()
                .expect("host effect tasks lock poisoned"),
        );
        if !tasks.is_empty()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                for task in tasks {
                    if let Err(error) = task.await {
                        tracing::error!(%error, "host effect cleanup failed");
                    }
                }
            });
        }
    }
}

fn invalid(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "host-effect",
        message: message.into(),
    }
}

impl<S> HostEffectJournal<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    pub(super) fn new(
        store: S,
        record: &SessionRecord,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        let incarnation = record
            .metadata
            .incarnation
            .clone()
            .ok_or_else(|| invalid("host effects require a durable incarnation"))?;
        let snapshot = record.snapshot();
        let receipts = snapshot
            .middleware_state
            .get(RECEIPTS)
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            inner: Arc::new(EffectInner {
                store,
                session: record.id.clone(),
                incarnation,
                cancellation,
                state: Mutex::new(JournalState {
                    commit: record
                        .commit_receipt()
                        .map_err(|error| invalid(error.to_string()))?,
                    events: Vec::new(),
                    receipts,
                    uncertain: super::tool::host_dispatch_unsettled(record),
                    invocations: record
                        .journal
                        .iter()
                        .filter_map(|event| match event {
                            SessionJournalEvent::MiddlewareStateUpdated { namespace, state }
                                if namespace == "host-dispatch" =>
                            {
                                state
                                    .get("invocationId")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                            }
                            _ => None,
                        })
                        .collect(),
                }),
            }),
            tasks: Default::default(),
        })
    }

    pub(super) async fn synchronize(
        &self,
        commit: SessionCommitReceipt,
    ) -> Result<(SessionCommitReceipt, Vec<SessionJournalEvent>)> {
        let tasks =
            std::mem::take(&mut *self.tasks.lock().expect("host effect tasks lock poisoned"));
        for task in tasks {
            task.await
                .map_err(|error| invalid(format!("host effect task failed: {error}")))?;
        }
        let mut state = self.inner.state.lock().await;
        let events = std::mem::take(&mut state.events);
        if events.is_empty() {
            if state.commit.execution_token.session_id != commit.execution_token.session_id
                || state.commit.execution_token.incarnation != commit.execution_token.incarnation
                || state.commit.inference_settings != commit.inference_settings
                || state.commit.agent_definition != commit.agent_definition
            {
                return Err(invalid("host effect incarnation or configuration changed"));
            }
            state.commit = commit;
        }
        Ok((state.commit.clone(), events))
    }
}

impl<S> HostEffectExecutor for HostEffectJournal<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    fn execute(
        &self,
        tool: AgentTool,
        call: ToolCall,
        turn: u64,
        context: BTreeMap<String, Value>,
    ) -> HostEffectFuture<'_> {
        let inner = self.inner.clone();
        let (reply, receiver) = oneshot::channel();
        // Dropping a render future cannot drop a dispatched effect or its settlement.
        let task = tokio::spawn(
            async move {
                drop(reply.send(inner.execute(tool, call, turn, context).await));
            }
            .in_current_span()
            .with_current_subscriber(),
        );
        self.tasks
            .lock()
            .expect("host effect tasks lock poisoned")
            .push(task);
        Box::pin(async move {
            receiver
                .await
                .map_err(|_| invalid("host effect completion unavailable"))?
        })
    }
}

impl<S> EffectInner<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    async fn append(
        &self,
        state: &mut JournalState,
        events: Vec<SessionJournalEvent>,
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        state.commit = self
            .store
            .append_journal(
                &self.session,
                state.commit.execution_token.clone(),
                events.clone(),
            )
            .await
            .map_err(|error| invalid(error.to_string()))?;
        state.events.extend(events);
        Ok(())
    }

    async fn execute(
        &self,
        tool: AgentTool,
        call: ToolCall,
        turn: u64,
        mut context_state: BTreeMap<String, Value>,
    ) -> Result<HostEffectResult> {
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| invalid("concurrent or recursive context effects are unsupported"))?;
        if state.uncertain {
            return Err(invalid(
                "unsettled host effect forbids replay or subsequent effects",
            ));
        }
        if let Some(receipt) = state.receipts.get(&call.id) {
            if receipt.call != call
                || receipt.identity.as_deref() != tool.identity()
                || receipt.declaration != *tool.definition()
            {
                return Err(invalid(
                    "settled host effect identity or declaration changed",
                ));
            }
            return Ok(HostEffectResult {
                result: Ok(receipt.result.clone()),
                context: context_state,
            });
        }
        if call.name != tool.definition().name {
            return Err(invalid("host effect declaration mismatch"));
        }
        if self.cancellation.is_cancelled() {
            return Err(invalid("host effect cancelled before dispatch"));
        }
        if let Some(guard) = &tool.execution_guard {
            guard(self.cancellation.clone()).await?;
        }
        let binding = tool
            .prepare_host_dispatch(&call)?
            .ok_or_else(|| invalid("host effect requires durable dispatch preparation"))?;
        if [&binding.invocation_id, &binding.source_revision]
            .iter()
            .any(|value| {
                value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
            })
        {
            return Err(invalid("invalid host effect association"));
        }
        if state.invocations.contains(&binding.invocation_id) {
            return Err(invalid("host invocation identity was already dispatched"));
        }
        let mut writes = Vec::new();
        for (namespace, value) in &context_state {
            if super::host::reserved_context_namespace(namespace) {
                return Err(invalid("reserved execution namespace"));
            }
            writes.push(SessionJournalEvent::MiddlewareStateUpdated {
                namespace: namespace.clone(),
                state: value.clone(),
            });
        }
        let active = json!({"active":true,"callId":call.id,"turn":turn,"incarnation":self.incarnation,
            "invocationId":binding.invocation_id,"sourceRevision":binding.source_revision});
        writes.push(SessionJournalEvent::MiddlewareStateUpdated {
            namespace: "host-dispatch".into(),
            state: active.clone(),
        });
        self.append(&mut state, writes).await?;
        state.uncertain = true;
        state.invocations.insert(binding.invocation_id.clone());
        let mut context = ToolExecutionContext::with_progress_and_cancellation(
            call.clone(),
            crate::ToolProgressSink::noop(),
            self.cancellation.clone(),
        )
        .with_session_context(self.session.clone(), turn);
        context.host_dispatch = Some(binding);
        context.session_incarnation = Some(self.incarnation.clone());
        let sink = context.result_metadata.clone();
        sink.hydrate_state(context_state.clone());
        let outcome = tool.call(context).await;
        let mut writes = Vec::new();
        for (namespace, value) in sink.take_state() {
            context_state.insert(namespace.clone(), value.clone());
            writes.push(SessionJournalEvent::MiddlewareStateUpdated {
                namespace,
                state: value,
            });
        }
        // Context changes survive both known failures and uncertain effect errors.
        if let Err(error) = self.append(&mut state, writes).await {
            return Ok(HostEffectResult {
                result: Err(error),
                context: context_state,
            });
        }
        let result = match outcome {
            Ok(result) => result,
            Err(error) => {
                return Ok(HostEffectResult {
                    result: Err(error),
                    context: context_state,
                });
            }
        };
        if result.call_id != call.id || result.name != call.name {
            return Ok(HostEffectResult {
                result: Err(invalid("host effect result identity mismatch")),
                context: context_state,
            });
        }
        let receipt = Receipt {
            call: call.clone(),
            identity: tool.identity().map(str::to_owned),
            declaration: tool.definition().clone(),
            result: result.clone(),
        };
        let mut receipts = state.receipts.clone();
        receipts.insert(call.id, receipt);
        let mut settled = active;
        settled["active"] = Value::Bool(false);
        let settlement = self
            .append(
                &mut state,
                vec![
                    SessionJournalEvent::MiddlewareStateUpdated {
                        namespace: RECEIPTS.into(),
                        state: serde_json::to_value(&receipts)?,
                    },
                    SessionJournalEvent::MiddlewareStateUpdated {
                        namespace: "host-dispatch".into(),
                        state: settled,
                    },
                ],
            )
            .await;
        if let Err(error) = settlement {
            return Ok(HostEffectResult {
                result: Err(error),
                context: context_state,
            });
        }
        state.receipts = receipts;
        state.uncertain = false;
        Ok(HostEffectResult {
            result: Ok(result),
            context: context_state,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        HostDispatchBinding, MemorySessionStore, SessionCoordinator, SessionCreateRequest,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn bound(tool: AgentTool) -> AgentTool {
        tool.with_host_dispatch(|call| {
            Ok(HostDispatchBinding {
                invocation_id: format!("invocation-{}", call.id),
                source_revision: "declaration-v1".into(),
            })
        })
    }

    #[tokio::test]
    async fn fence_precedes_effect_and_known_error_is_settled_and_replayed_without_effects() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store.clone());
        let record = sessions
            .create(SessionCreateRequest::new("template-known"))
            .await
            .unwrap()
            .record;
        let journal = HostEffectJournal::new(store, &record, CancellationToken::new()).unwrap();
        let effects = Arc::new(AtomicUsize::new(0));
        let count = effects.clone();
        let id = record.id.clone();
        let tool = bound(AgentTool::new_async(
            ToolDefinition::new("effect", "test", json!({})),
            move |context| {
                let sessions = sessions.clone();
                let id = id.clone();
                let count = count.clone();
                Box::pin(async move {
                    let record = sessions.load(&id).await.unwrap().record;
                    assert!(super::super::tool::host_dispatch_unsettled(&record));
                    count.fetch_add(1, Ordering::SeqCst);
                    context
                        .result_metadata()
                        .set_context("host-test".into(), json!({"done":true}))?;
                    Ok(context.result("known denial").with_error())
                })
            },
        ));
        let call = ToolCall::new("template-occurrence", "effect", json!({}));
        assert!(
            journal
                .execute(tool.clone(), call.clone(), 0, BTreeMap::new())
                .await
                .unwrap()
                .result
                .unwrap()
                .is_error
        );
        let (_, events) = journal
            .synchronize(record.commit_receipt().unwrap())
            .await
            .unwrap();
        let mut projected = record.clone();
        projected.append_journal_events(events);
        assert!(!super::super::tool::host_dispatch_unsettled(&projected));
        assert_eq!(
            projected.snapshot().middleware_state["host-test"],
            json!({"done":true})
        );
        journal
            .execute(tool, call, 0, BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(effects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn uncertain_error_preserves_context_and_fence_and_rejects_replay() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store.clone());
        let record = sessions
            .create(SessionCreateRequest::new("template-uncertain"))
            .await
            .unwrap()
            .record;
        let journal = HostEffectJournal::new(store, &record, CancellationToken::new()).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let effects = count.clone();
        let tool = bound(AgentTool::new(
            ToolDefinition::new("effect", "test", json!({})),
            move |context| {
                effects.fetch_add(1, Ordering::SeqCst);
                context
                    .result_metadata()
                    .set_context("host-test".into(), json!("retained"))?;
                Err(invalid("transport outcome uncertain"))
            },
        ));
        let call = ToolCall::new("template-occurrence", "effect", json!({}));
        assert!(
            journal
                .execute(tool.clone(), call.clone(), 0, BTreeMap::new())
                .await
                .unwrap()
                .result
                .is_err()
        );
        assert!(
            journal
                .execute(tool, call, 0, BTreeMap::new())
                .await
                .is_err()
        );
        let (_, events) = journal
            .synchronize(record.commit_receipt().unwrap())
            .await
            .unwrap();
        let mut projected = record;
        projected.append_journal_events(events);
        assert!(super::super::tool::host_dispatch_unsettled(&projected));
        assert_eq!(
            projected.snapshot().middleware_state["host-test"],
            json!("retained")
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_joins_a_dispatched_effect_even_when_render_waiter_is_dropped() {
        let store = MemorySessionStore::new();
        let record = SessionCoordinator::new(store.clone())
            .create(SessionCreateRequest::new("template-cancel"))
            .await
            .unwrap()
            .record;
        let cancellation = CancellationToken::new();
        let journal = HostEffectJournal::new(store, &record, cancellation.clone()).unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let ready = started.clone();
        let finished = Arc::new(AtomicUsize::new(0));
        let joined = finished.clone();
        let tool = bound(AgentTool::new_async(
            ToolDefinition::new("effect", "test", json!({})),
            move |context| {
                let ready = ready.clone();
                let joined = joined.clone();
                Box::pin(async move {
                    ready.notify_one();
                    context.cancellation_token().cancelled().await;
                    joined.fetch_add(1, Ordering::SeqCst);
                    Err(invalid("cancelled uncertain effect"))
                })
            },
        ));
        let waiter = journal.execute(
            tool,
            ToolCall::new("cancel", "effect", json!({})),
            0,
            BTreeMap::new(),
        );
        started.notified().await;
        drop(waiter);
        cancellation.cancel();
        journal
            .synchronize(record.commit_receipt().unwrap())
            .await
            .unwrap();
        assert_eq!(finished.load(Ordering::SeqCst), 1);
    }
}
