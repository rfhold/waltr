//! Existing-session MCP Apps operations; never enters the provider loop.

use super::super::{
    SessionIncarnation, SessionOperation, SessionRecord, SessionStatus, SessionTimelineMode,
    SessionTimelineProjectedEvent,
};
use super::{SessionCoordinator, SessionId, SessionStore};
use crate::mcp::{
    McpClientRuntime,
    apps::{MCP_APP_ASSOCIATION, McpAppAssociation},
};
use crate::middleware::{
    HookOutcome,
    context::{MiddlewareStateLog, ToolCallContext, ToolResultContext},
};
use crate::{
    AgentRuntime, PendingTimelineEvent, PendingTimelineItemId, PermissionLevel, TimelineChannel,
    TimelineContent, TimelineEventKind, TimelineQuery, ToolCall,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

/// Fail-closed outcome for an existing-session app operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpAppError {
    /// Session is running, suspended, paused, or otherwise unavailable.
    Busy,
    /// Association, current source authority, or session incarnation is stale.
    Stale,
    /// Caller, handle, or coordinator has canceled the operation.
    Canceled,
    /// A policy requires approval; Apps do not create approval continuations.
    ApprovalRequired,
    /// Policy or visibility denies the operation.
    Denied,
    /// The bounded handle registry is full.
    Limit,
    /// Storage, middleware, MCP, or resource validation failure.
    Failure(String),
}

impl std::fmt::Display for McpAppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MCP app: {self:?}")
    }
}
impl std::error::Error for McpAppError {}

fn failure(error: impl std::fmt::Display) -> McpAppError {
    McpAppError::Failure(error.to_string())
}

/// Opaque agent binding. Hosts must bind this capability to their authenticated owner.
#[derive(Clone)]
pub struct McpAppHandle {
    inner: Arc<AppHandleInner>,
}

impl std::fmt::Debug for McpAppHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpAppHandle").finish_non_exhaustive()
    }
}

pub(super) struct AppHandleInner {
    owner: Arc<tokio::sync::Semaphore>,
    session: SessionId,
    incarnation: SessionIncarnation,
    call_id: String,
    association: McpAppAssociation,
    token: CancellationToken,
    expires: tokio::time::Instant,
    _slot: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for AppHandleInner {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

/// Original execution snapshot and the validated linked HTML resource.
#[derive(Debug)]
pub struct SessionMcpApp {
    /// Owner-bound opaque capability for further same-server calls.
    pub handle: McpAppHandle,
    /// Immutable original execution, not a refresh result.
    pub association: McpAppAssociation,
    /// Linked HTML and requested (not granted) CSP/permissions metadata.
    pub resource: mcp::apps::UiResource,
}

impl<S> SessionCoordinator<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    /// Open only the app linked to an effective, complete recorded tool call.
    ///
    /// # Errors
    /// Rejects stale/hidden calls, unavailable sessions, invalid resources, or cancellation.
    pub async fn open_session_mcp_app(
        &self,
        session: &SessionId,
        recorded_call_id: &str,
        cancellation: CancellationToken,
    ) -> Result<SessionMcpApp, McpAppError> {
        if cancellation.is_cancelled() {
            return Err(McpAppError::Canceled);
        }
        let slot = Arc::clone(&self.app_slots)
            .try_acquire_owned()
            .map_err(|_| McpAppError::Limit)?;
        let (record, incarnation) = self.app_record(session, None).await?;
        let association = recorded_association(&record, recorded_call_id)?;
        let (runtime, mcp) = self.app_runtime(session, &record, incarnation).await?;
        validate_source(&runtime, &association)?;
        validate_live_source(&mcp, &association).await?;
        let token = self.shutdown_token.child_token();
        let resource = tokio::select! {
            biased;
            _ = cancellation.cancelled() => { token.cancel(); return Err(McpAppError::Canceled); },
            _ = token.cancelled() => return Err(McpAppError::Canceled),
            result = mcp.read_app_resource(&association.server, &association.resource_uri, mcp::apps::DEFAULT_MAX_HTML_BYTES, token.clone()) => result.map_err(failure)?,
        };
        validate_live_source(&mcp, &association).await?;
        let operation = self.app_operation(session, Some(incarnation)).await?;
        let current = self.store.load(session).await.map_err(failure)?;
        if current.revision != record.revision
            || recorded_association(&current, recorded_call_id)? != association
        {
            return Err(McpAppError::Stale);
        }
        self.app_ready(session, &current).await?;
        let expires = tokio::time::Instant::now() + Duration::from_secs(15 * 60);
        if cancellation.is_cancelled() || token.is_cancelled() {
            return Err(McpAppError::Canceled);
        }
        let handle = McpAppHandle {
            inner: Arc::new(AppHandleInner {
                owner: Arc::clone(&self.app_slots),
                session: session.clone(),
                incarnation,
                call_id: recorded_call_id.to_owned(),
                association: association.clone(),
                token: token.clone(),
                expires,
                _slot: slot,
            }),
        };
        let mut handles = self
            .app_handles
            .lock()
            .expect("app handle registry poisoned");
        handles.retain(|handle| handle.strong_count() > 0);
        handles.push(Arc::downgrade(&handle.inner));
        drop(handles);
        drop(operation);
        tokio::spawn(async move {
            tokio::select! { _ = token.cancelled() => {}, _ = tokio::time::sleep_until(expires) => token.cancel() }
        });
        Ok(SessionMcpApp {
            handle,
            association,
            resource,
        })
    }

    /// Call a current app-visible installed tool on this handle's exact source server.
    /// No provider request, new session, approval queue, or resource forwarding is performed.
    ///
    /// # Errors
    /// Rejects denied/approval-required policy, stale authority, busy sessions, or cancellation.
    pub async fn call_session_mcp_app_tool(
        &self,
        handle: &McpAppHandle,
        tool: &str,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> Result<mcp::McpToolResult, McpAppError> {
        if cancellation.is_cancelled() {
            return Err(McpAppError::Canceled);
        }
        let inner = &handle.inner;
        if !Arc::ptr_eq(&inner.owner, &self.app_slots) {
            return Err(McpAppError::Stale);
        }
        if inner.token.is_cancelled() || tokio::time::Instant::now() >= inner.expires {
            return Err(McpAppError::Canceled);
        }
        let (record, _) = self
            .app_record(&inner.session, Some(inner.incarnation))
            .await?;
        if recorded_association(&record, &inner.call_id)? != inner.association {
            return Err(McpAppError::Stale);
        }
        let (runtime, mcp) = self
            .app_runtime(&inner.session, &record, inner.incarnation)
            .await?;
        validate_source(&runtime, &inner.association)?;
        let installed = runtime
            .tools
            .iter()
            .find(|installed| {
                installed.mcp_app_source.as_ref().is_some_and(|source| {
                    source.server == inner.association.server
                        && source.tool == tool
                        && source.source == inner.association.source
                })
            })
            .ok_or(McpAppError::Denied)?;
        if !installed
            .mcp_app_source
            .as_ref()
            .expect("source exists")
            .ui
            .visible_to(mcp::apps::UiVisibility::App)
        {
            return Err(McpAppError::Denied);
        }
        let name = installed.definition().name.clone();
        let mut call = ToolCall::new(uuid::Uuid::now_v7().to_string(), &name, arguments);
        let mut state = MiddlewareStateLog::from_snapshot(&record.snapshot());
        let turn = record
            .timeline_events()
            .last()
            .map_or(0, |event| event.turn);
        let token = inner.token.child_token();
        let _cancel_on_drop = token.clone().drop_guard();
        let execution = async {
            let mut claims = Vec::new();
            authorize_app_call(
                &runtime.middleware_chain,
                &mut ToolCallContext {
                    approved_tool_name: &name,
                    permission_policy: &runtime.permission_policy,
                    contextual_input_is_approved: |_, _, _| false,
                    contextual_suspension: crate::agent::r#loop::contextual_suspension,
                    cancellation_token: token.clone(),
                    session_id: inner.session.as_str(),
                    turn,
                    call: &mut call,
                    permission_claims: &mut claims,
                    middleware_state: &mut state,
                },
            )
            .await?;
            let operation = self
                .app_operation(&inner.session, Some(inner.incarnation))
                .await?;
            let current = self.store.load(&inner.session).await.map_err(failure)?;
            self.app_ready(&inner.session, &current).await?;
            if current.revision != record.revision {
                return Err(McpAppError::Stale);
            }
            validate_live_source(&mcp, &inner.association).await?;
            let declaration = installed
                .mcp_app_source
                .as_ref()
                .expect("source exists")
                .declaration
                .clone();
            if !mcp
                .current_app_source(&inner.association.server, tool)
                .await
                .is_some_and(|source| source.declaration == declaration)
            {
                return Err(McpAppError::Stale);
            }
            let (mut projected, raw) = mcp
                .call_app_tool(&inner.association.server, tool, &call, token.clone())
                .await
                .map_err(failure)?;
            let original_projected = projected.clone();
            after_app_tool_result(
                &runtime.middleware_chain,
                &mut ToolResultContext {
                    call: Some(&call),
                    result: &mut projected,
                    middleware_state: &mut state,
                    middleware_chain: runtime.middleware_chain.clone(),
                    permission_policy: runtime.permission_policy.clone(),
                    cancellation_token: token.clone(),
                    session_id: inner.session.as_str(),
                    turn,
                },
                &original_projected,
            )
            .await?;
            let current = self.store.load(&inner.session).await.map_err(failure)?;
            self.app_ready(&inner.session, &current).await?;
            if current.revision != record.revision
                || token.is_cancelled()
                || inner.token.is_cancelled()
            {
                return Err(McpAppError::Stale);
            }
            validate_live_source(&mcp, &inner.association).await?;
            if !mcp
                .current_app_source(&inner.association.server, tool)
                .await
                .is_some_and(|source| source.declaration == declaration)
            {
                return Err(McpAppError::Stale);
            }
            // Audit only: never replace the original result or append model tool turns.
            let event = PendingTimelineEvent::new(
                inner.session.clone(),
                turn,
                PendingTimelineItemId::New,
                TimelineEventKind::ItemFinished,
                Some(TimelineChannel::from("mcp_app_audit")),
                TimelineContent::Json {
                    value: json!({"version": 1, "recordedCallId": inner.call_id, "server": inner.association.server, "tool": tool, "callId": call.id, "arguments": call.arguments, "result": raw.raw}),
                },
            );
            let events = self
                .store
                .append_timeline(&inner.session, current.revision, vec![event])
                .await
                .map_err(failure)?;
            self.timeline.publish_all(events);
            let updated = self.store.load(&inner.session).await.map_err(failure)?;
            operation.publish_host_summary(updated.summary());
            Ok(raw)
        };
        tokio::pin!(execution);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => { token.cancel(); drop(execution.await); Err(McpAppError::Canceled) },
            _ = inner.token.cancelled() => { token.cancel(); drop(execution.await); Err(McpAppError::Canceled) },
            result = &mut execution => result,
        }
    }

    /// Revoke an app capability and cancel any in-flight calls using it.
    pub fn close_session_mcp_app(&self, handle: &McpAppHandle) {
        handle.inner.token.cancel();
    }

    pub(super) fn revoke_session_mcp_apps(&self, session: Option<&SessionId>) {
        self.app_handles
            .lock()
            .expect("app handle registry poisoned")
            .retain(|weak| {
                let Some(handle) = weak.upgrade() else {
                    return false;
                };
                if session.is_none_or(|session| session == &handle.session) {
                    handle.token.cancel();
                }
                true
            });
    }

    async fn app_operation(
        &self,
        session: &SessionId,
        incarnation: Option<SessionIncarnation>,
    ) -> Result<SessionOperation, McpAppError> {
        let operation = self
            .control
            .session(session.clone())
            .try_operation()
            .ok_or(McpAppError::Busy)?;
        if incarnation.is_some_and(|expected| operation.incarnation() != expected) {
            return Err(McpAppError::Stale);
        }
        Ok(operation)
    }

    async fn app_ready(
        &self,
        session: &SessionId,
        record: &SessionRecord,
    ) -> Result<(), McpAppError> {
        if self.shutdown_token.is_cancelled() {
            return Err(McpAppError::Canceled);
        }
        if !matches!(
            record.snapshot().status,
            SessionStatus::Idle | SessionStatus::Finished
        ) {
            return Err(McpAppError::Busy);
        }
        let runs = self.active_runs.lock().await;
        if runs.shutdown
            || runs.wiping
            || runs.sessions.get(session).is_some_and(|state| {
                state.active.is_some()
                    || state.pause.is_some()
                    || state.completion_barrier.is_some()
                    || state.blocked
                    || !state.queued.is_empty()
            })
        {
            return Err(McpAppError::Busy);
        }
        Ok(())
    }

    async fn app_record(
        &self,
        session: &SessionId,
        incarnation: Option<SessionIncarnation>,
    ) -> Result<(SessionRecord, SessionIncarnation), McpAppError> {
        let operation = self.app_operation(session, incarnation).await?;
        let record = self.store.load(session).await.map_err(failure)?;
        self.app_ready(session, &record).await?;
        operation.register_loaded_summary(record.summary());
        Ok((record, operation.incarnation()))
    }

    async fn app_runtime(
        &self,
        session: &SessionId,
        record: &SessionRecord,
        incarnation: SessionIncarnation,
    ) -> Result<(AgentRuntime, Arc<McpClientRuntime>), McpAppError> {
        let factory = self
            .runtime_factory
            .as_ref()
            .ok_or_else(|| failure("session runtime factory is not configured"))?;
        let mcp = self
            .ensure_mcp_runtime(session, record, incarnation, factory)
            .await
            .map_err(failure)?
            .ok_or(McpAppError::Stale)?;
        let runtime = factory
            .runtime_for_session(record, self.clone(), Some(Arc::clone(&mcp)))
            .await
            .map_err(failure)?;
        Ok((runtime, mcp))
    }
}

fn permission(level: PermissionLevel) -> Result<(), McpAppError> {
    match level {
        PermissionLevel::Allow => Ok(()),
        PermissionLevel::Ask => Err(McpAppError::ApprovalRequired),
        PermissionLevel::Deny => Err(McpAppError::Denied),
    }
}

async fn authorize_app_call(
    chain: &crate::middleware::chain::MiddlewareChain,
    ctx: &mut ToolCallContext<'_>,
) -> Result<(), McpAppError> {
    permission(ctx.permission_policy.evaluate_tool(ctx.approved_tool_name))?;
    hook(chain.before_tool_call(ctx).await.map_err(failure)?)?;
    if ctx.call.name != ctx.approved_tool_name {
        return Err(McpAppError::Denied);
    }
    if let Some(input) = ctx
        .permission_policy
        .evaluate_input(&ctx.call.name, &ctx.call.arguments)
    {
        permission(input.effect.level)?;
    }
    for claim in ctx.permission_claims.iter() {
        if let Some(effect) = ctx
            .permission_policy
            .evaluate_claim_refinement_effect(&ctx.call.name, claim)
        {
            permission(effect.level)?;
        }
    }
    Ok(())
}
fn hook(outcome: HookOutcome) -> Result<(), McpAppError> {
    match outcome {
        HookOutcome::Continue => Ok(()),
        HookOutcome::Suspend(_) => Err(McpAppError::ApprovalRequired),
        HookOutcome::ReplaceToolResult(_) => Err(McpAppError::Denied),
    }
}

fn validate_source(
    runtime: &AgentRuntime,
    association: &McpAppAssociation,
) -> Result<(), McpAppError> {
    if association.version != 1 {
        return Err(McpAppError::Stale);
    }
    let tool = runtime
        .tools
        .iter()
        .find(|tool| {
            tool.mcp_app_source.as_ref().is_some_and(|source| {
                source.server == association.server
                    && source.tool == association.tool
                    && source.source == association.source
                    && source.declaration == association.declaration
                    && source.ui.resource_uri.as_ref() == Some(&association.resource_uri)
            })
        })
        .ok_or(McpAppError::Stale)?;
    permission(
        runtime
            .permission_policy
            .evaluate_tool(&tool.definition().name),
    )
}

async fn validate_live_source(
    mcp: &Arc<McpClientRuntime>,
    association: &McpAppAssociation,
) -> Result<(), McpAppError> {
    if association.source.is_empty()
        || !mcp.app_authority_current(&association.server).await
        || !mcp
            .current_app_source(&association.server, &association.tool)
            .await
            .is_some_and(|source| {
                source.source == association.source && source.declaration == association.declaration
            })
    {
        return Err(McpAppError::Stale);
    }
    Ok(())
}

async fn after_app_tool_result(
    chain: &crate::middleware::chain::MiddlewareChain,
    context: &mut ToolResultContext<'_>,
    original: &crate::ToolResult,
) -> Result<(), McpAppError> {
    hook(chain.after_tool_call(context).await.map_err(failure)?)?;
    if context.result != original {
        return Err(McpAppError::Denied);
    }
    Ok(())
}

fn recorded_association(
    record: &SessionRecord,
    call_id: &str,
) -> Result<McpAppAssociation, McpAppError> {
    let page = super::super::projection::project_timeline(
        record,
        SessionTimelineMode::Effective,
        TimelineQuery::forward(None, usize::MAX),
    );
    let events = page
        .events
        .iter()
        .filter_map(|event| match &event.event {
            SessionTimelineProjectedEvent::Durable(event) => Some(event),
            _ => None,
        })
        .collect::<Vec<_>>();
    let calls = events.iter().filter(|event| matches!(&event.content, TimelineContent::ToolCall { call } if call.id == call_id)).collect::<Vec<_>>();
    let [call_event] = calls.as_slice() else {
        return Err(McpAppError::Stale);
    };
    let results = events.iter().filter(|event| event.kind == TimelineEventKind::ItemFinished && event.status == Some(crate::TimelineStatus::Finished) && event.parent_id.as_ref() == Some(&call_event.item_id) && matches!(&event.content, TimelineContent::ToolResult { result } if result.call_id == call_id)).collect::<Vec<_>>();
    let [result] = results.as_slice() else {
        return Err(McpAppError::Stale);
    };
    serde_json::from_value(
        result
            .metadata
            .get(MCP_APP_ASSOCIATION)
            .cloned()
            .ok_or(McpAppError::Stale)?,
    )
    .map_err(failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::{AgentMiddleware, chain::MiddlewareChain};
    use crate::{
        PermissionClaim, PermissionEffect, PermissionInputCondition, PermissionInputRule,
        PermissionInputScalar, PermissionInputValueMatcher, PermissionJsonPointer,
        PermissionOperation, PermissionPolicy, PermissionResource, PermissionToolSelector,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RewriteAndClaim(Arc<AtomicUsize>);
    struct RedactResult(bool);
    impl AgentMiddleware for RedactResult {
        async fn after_tool_call(
            &self,
            ctx: &mut ToolResultContext<'_>,
        ) -> crate::Result<HookOutcome> {
            if self.0 {
                ctx.result.is_error = !ctx.result.is_error;
            } else {
                ctx.result.content = crate::ToolResultContent::text("redacted");
            }
            Ok(HookOutcome::Continue)
        }
    }

    #[tokio::test]
    async fn mcp_apps_after_middleware_mutations_deny_raw_publication() {
        for mutation in [None, Some(false), Some(true)] {
            let mut chain = MiddlewareChain::new();
            if let Some(error) = mutation {
                chain.push(RedactResult(error));
            }
            let original = crate::ToolResult::new("id", "tool", "secret");
            let mut result = original.clone();
            let mut state = MiddlewareStateLog::default();
            let outcome = after_app_tool_result(
                &chain,
                &mut ToolResultContext {
                    call: None,
                    result: &mut result,
                    middleware_state: &mut state,
                    middleware_chain: chain.clone(),
                    permission_policy: crate::PermissionPolicy::default(),
                    cancellation_token: CancellationToken::new(),
                    session_id: "session",
                    turn: 0,
                },
                &original,
            )
            .await;
            assert_eq!(
                outcome,
                if mutation.is_some() {
                    Err(McpAppError::Denied)
                } else {
                    Ok(())
                }
            );
        }
    }
    impl AgentMiddleware for RewriteAndClaim {
        async fn before_tool_call(
            &self,
            ctx: &mut ToolCallContext<'_>,
        ) -> crate::Result<HookOutcome> {
            self.0.fetch_add(1, Ordering::Relaxed);
            ctx.call.arguments["value"] = json!("rewritten");
            ctx.permission_claims.push(PermissionClaim::new(
                PermissionOperation::Read,
                PermissionResource::FilesystemPath("/private".into()),
                "private read",
            ));
            Ok(HookOutcome::Continue)
        }
    }

    async fn authorize(
        policy: &PermissionPolicy,
        chain: &MiddlewareChain,
    ) -> Result<(), McpAppError> {
        let mut call = ToolCall::new("app-call", "tool", json!({"value":"original"}));
        let mut claims = Vec::new();
        let mut state = MiddlewareStateLog::default();
        authorize_app_call(
            chain,
            &mut ToolCallContext {
                approved_tool_name: "tool",
                permission_policy: policy,
                contextual_input_is_approved: |_, _, _| false,
                contextual_suspension: crate::agent::r#loop::contextual_suspension,
                cancellation_token: CancellationToken::new(),
                session_id: "session",
                turn: 0,
                call: &mut call,
                permission_claims: &mut claims,
                middleware_state: &mut state,
            },
        )
        .await
    }

    #[tokio::test]
    async fn apps_permission_order_whole_before_middleware_then_final_input_and_claims() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut chain = MiddlewareChain::new();
        chain.push(RewriteAndClaim(Arc::clone(&count)));
        assert_eq!(
            authorize(&PermissionPolicy::new(PermissionLevel::Ask), &chain).await,
            Err(McpAppError::ApprovalRequired)
        );
        assert_eq!(
            authorize(&PermissionPolicy::new(PermissionLevel::Deny), &chain).await,
            Err(McpAppError::Denied)
        );
        assert_eq!(count.load(Ordering::Relaxed), 0);
        for (effect, error) in [
            (PermissionEffect::ask(), McpAppError::ApprovalRequired),
            (PermissionEffect::deny(), McpAppError::Denied),
        ] {
            let input = PermissionPolicy::default().with_input_rule(
                PermissionToolSelector::exact("tool"),
                PermissionInputRule::new(
                    vec![PermissionInputCondition {
                        pointer: PermissionJsonPointer::parse("/value").unwrap(),
                        value: PermissionInputValueMatcher::Exact(PermissionInputScalar::String(
                            "rewritten".into(),
                        )),
                    }],
                    effect.clone(),
                )
                .unwrap(),
            );
            assert_eq!(authorize(&input, &chain).await, Err(error.clone()));
            let claims = PermissionPolicy::default().with_class_rule(
                PermissionToolSelector::exact("tool"),
                PermissionOperation::Read,
                effect,
            );
            assert_eq!(authorize(&claims, &chain).await, Err(error));
        }
        assert_eq!(count.load(Ordering::Relaxed), 4);
        assert_eq!(
            authorize(&PermissionPolicy::default(), &chain).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn apps_lifecycle_gate_and_incarnation_fail_closed() {
        let sessions = SessionCoordinator::new(crate::MemorySessionStore::new());
        let id = SessionId::new("app-lifecycle");
        sessions
            .create(crate::SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        let (_, incarnation) = sessions.app_record(&id, None).await.unwrap();
        let held = sessions.control.session(id.clone()).operation().await;
        assert!(matches!(
            sessions.app_record(&id, None).await,
            Err(McpAppError::Busy)
        ));
        drop(held);
        let record = sessions.store.load(&id).await.unwrap();
        sessions
            .store
            .append_journal(
                &id,
                record.revision,
                vec![crate::SessionJournalEvent::Suspended {
                    suspension: crate::Suspension::AwaitingToolResults { work: Vec::new() },
                }],
            )
            .await
            .unwrap();
        assert!(matches!(
            sessions.app_record(&id, None).await,
            Err(McpAppError::Busy)
        ));
        sessions.delete(&id).await.unwrap();
        sessions
            .create(crate::SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        assert!(matches!(
            sessions.app_record(&id, Some(incarnation)).await,
            Err(McpAppError::Stale)
        ));
    }
}
