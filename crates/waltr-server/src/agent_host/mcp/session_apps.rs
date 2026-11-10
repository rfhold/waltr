//! Server-owned Apps provenance, exclusive effects, opaque handles, and audit.
#[cfg(test)]
#[path = "session_apps_tests.rs"]
mod tests;

use super::{
    McpClientRuntime,
    apps::{MCP_APP_ASSOCIATION, McpAppAssociation},
    elicitation::McpOperation,
};
use crate::agent_host::{
    dispatcher::CapabilityDispatcher, runtime_factory::ServerAgentRuntimeFactory,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use waltr_agent::{
    AgentTool, PendingTimelineEvent, PendingTimelineItemId, SessionCoordinator, SessionId,
    SessionRecord, SessionStatus, SessionStore, SessionTimelineMode, SessionTimelineProjectedEvent,
    TimelineChannel, TimelineContent, TimelineEventKind, TimelineQuery, ToolCall,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpAppError {
    Busy,
    Stale,
    Canceled,
    Denied,
    Limit,
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
fn known_error(
    context: &waltr_agent::ToolExecutionContext,
    errors: &Mutex<Option<McpAppError>>,
    error: McpAppError,
) -> waltr_agent::ToolResult {
    let result = context.result(error.to_string()).with_error();
    *errors.lock().expect("app error lock poisoned") = Some(error);
    result
}

#[derive(Clone)]
pub struct McpAppHandle {
    inner: Arc<Handle>,
}
impl std::fmt::Debug for McpAppHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpAppHandle")
    }
}
impl McpAppHandle {
    pub(crate) fn revoke(&self) {
        self.inner.token.cancel();
    }
}
struct Handle {
    owner: Arc<tokio::sync::Semaphore>,
    session: SessionId,
    incarnation: String,
    call_id: String,
    association: McpAppAssociation,
    token: CancellationToken,
    expires: tokio::time::Instant,
    _slot: tokio::sync::OwnedSemaphorePermit,
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.token.cancel();
    }
}
#[derive(Debug)]
pub struct SessionMcpApp {
    pub handle: McpAppHandle,
    pub association: McpAppAssociation,
    pub resource: mcp::apps::UiResource,
}

#[derive(Debug, Clone)]
pub struct ServerMcpApps<S: SessionStore + Clone + Send + Sync + 'static> {
    sessions: SessionCoordinator<S>,
    factory: ServerAgentRuntimeFactory,
    slots: Arc<tokio::sync::Semaphore>,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> ServerMcpApps<S> {
    pub fn new(sessions: SessionCoordinator<S>, factory: ServerAgentRuntimeFactory) -> Self {
        Self {
            sessions,
            factory,
            slots: Arc::new(tokio::sync::Semaphore::new(32)),
        }
    }
    async fn record(
        &self,
        id: &SessionId,
        incarnation: Option<&str>,
    ) -> Result<SessionRecord, McpAppError> {
        let record = self.sessions.store().load(id).await.map_err(failure)?;
        if !matches!(
            record.snapshot().status,
            SessionStatus::Idle | SessionStatus::Finished
        ) || self.sessions.coordination_state(id).await.phase
            != waltr_agent::SessionCoordinationPhase::Ready
        {
            return Err(McpAppError::Busy);
        }
        if record
            .metadata
            .incarnation
            .as_deref()
            .is_none_or(str::is_empty)
            || incarnation
                .is_some_and(|value| record.metadata.incarnation.as_deref() != Some(value))
        {
            return Err(McpAppError::Stale);
        }
        Ok(record)
    }
    async fn association(
        &self,
        record: &SessionRecord,
        call_id: &str,
    ) -> Result<McpAppAssociation, McpAppError> {
        let page = self
            .sessions
            .timeline_projection(
                &record.id,
                SessionTimelineMode::Effective,
                TimelineQuery::forward(None, usize::MAX),
            )
            .await
            .map_err(failure)?;
        recorded_association(&page, call_id)
    }
    async fn runtime(
        &self,
        record: &SessionRecord,
    ) -> Result<(Arc<McpClientRuntime>, CapabilityDispatcher), McpAppError> {
        let mcp = self
            .factory
            .mcp_registry()
            .runtime(&self.factory, record, &self.sessions)
            .await
            .map_err(failure)?
            .ok_or(McpAppError::Stale)?;
        let (_, dispatcher) = self
            .factory
            .runtime_with_dispatcher(record, self.sessions.clone(), Some(mcp.clone()))
            .await
            .map_err(failure)?;
        Ok((mcp, dispatcher))
    }
    pub async fn open_session_mcp_app(
        &self,
        session: &SessionId,
        call_id: &str,
        cancellation: CancellationToken,
    ) -> Result<SessionMcpApp, McpAppError> {
        if cancellation.is_cancelled() {
            return Err(McpAppError::Canceled);
        }
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| McpAppError::Limit)?;
        let record = self.record(session, None).await?;
        let association = self.association(&record, call_id).await?;
        let (mcp, dispatcher) = self.runtime(&record).await?;
        let source_ceiling = validate_source(&dispatcher, &association)?;
        let dispatcher = dispatcher.narrowed_permissions(source_ceiling);
        validate_live_source(&mcp, &association, cancellation.clone()).await?;
        let token = mcp.lifetime();
        let output = Arc::new(Mutex::new(None));
        let captured = output.clone();
        let mut capability = dispatcher
            .capabilities()
            .find(|cap| cap.definition().name == "read_mcp_app_resource")
            .cloned()
            .ok_or(McpAppError::Denied)?;
        let definition = capability.definition().clone();
        let identity = capability.identity();
        let runtime = mcp.clone();
        let source = association.clone();
        let policy = dispatcher.operation_policy();
        let errors = Arc::new(Mutex::new(None));
        let captured_errors = errors.clone();
        capability.tool = AgentTool::new_async(definition, move |context| {
            let errors = captured_errors.clone();
            let runtime = runtime.clone();
            let source = source.clone();
            let captured = captured.clone();
            let policy = policy.clone();
            Box::pin(async move {
                if context.call().arguments
                    != json!({"server":source.server,"uri":source.resource_uri})
                {
                    return Ok(known_error(&context, &errors, McpAppError::Denied));
                }
                if let Err(error) =
                    validate_live_source(&runtime, &source, context.cancellation_token().clone())
                        .await
                {
                    return Ok(known_error(&context, &errors, error));
                }
                let route = McpOperation::for_context(policy.as_ref(), &context);
                let resource = runtime
                    .read_app_resource(
                        &source.server,
                        &source.resource_uri,
                        mcp::apps::DEFAULT_MAX_HTML_BYTES,
                        context.cancellation_token().clone(),
                        route,
                    )
                    .await?;
                if let Err(error) =
                    validate_live_source(&runtime, &source, context.cancellation_token().clone())
                        .await
                {
                    return Ok(known_error(&context, &errors, error));
                }
                *captured.lock().expect("app resource lock poisoned") = Some(resource);
                Ok(context.result("linked app resource loaded"))
            })
        })
        .with_identity(identity);
        let effect = dispatcher.scoped_effect(capability).map_err(failure)?;
        let call = ToolCall::new(
            uuid::Uuid::now_v7().to_string(),
            "read_mcp_app_resource",
            json!({"server":association.server,"uri":association.resource_uri}),
        );
        self.execute(
            &record,
            call_id,
            &association.server,
            "resource",
            effect,
            call,
            token.clone(),
            cancellation,
            None,
        )
        .await?;
        if let Some(error) = errors.lock().expect("app error lock poisoned").take() {
            return Err(error);
        }
        let resource = output
            .lock()
            .expect("app resource lock poisoned")
            .take()
            .ok_or(McpAppError::Denied)?;
        let expires = tokio::time::Instant::now() + Duration::from_secs(900);
        let handle = McpAppHandle {
            inner: Arc::new(Handle {
                owner: self.slots.clone(),
                session: session.clone(),
                incarnation: record.metadata.incarnation.expect("validated incarnation"),
                call_id: call_id.into(),
                association: association.clone(),
                token: token.clone(),
                expires,
                _slot: slot,
            }),
        };
        mcp.expire_handle(token, expires);
        Ok(SessionMcpApp {
            handle,
            association,
            resource,
        })
    }
    pub async fn call_session_mcp_app_tool(
        &self,
        handle: &McpAppHandle,
        tool: &str,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> Result<mcp::McpToolResult, McpAppError> {
        let inner = &handle.inner;
        if !Arc::ptr_eq(&inner.owner, &self.slots) {
            return Err(McpAppError::Stale);
        }
        if inner.token.is_cancelled() || tokio::time::Instant::now() >= inner.expires {
            return Err(McpAppError::Canceled);
        }
        let record = self
            .record(&inner.session, Some(&inner.incarnation))
            .await?;
        if self.association(&record, &inner.call_id).await? != inner.association {
            return Err(McpAppError::Stale);
        }
        let (mcp, dispatcher) = self.runtime(&record).await?;
        let source_ceiling = validate_source(&dispatcher, &inner.association)?;
        let dispatcher = dispatcher.narrowed_permissions(source_ceiling);
        validate_live_source(&mcp, &inner.association, cancellation.clone()).await?;
        let mut capability = dispatcher
            .capabilities()
            .find(|cap| {
                cap.mcp_app_source.as_ref().is_some_and(|source| {
                    source.server == inner.association.server
                        && source.tool == tool
                        && source.source == inner.association.source
                        && source.ui.visible_to(mcp::apps::UiVisibility::App)
                })
            })
            .cloned()
            .ok_or(McpAppError::Denied)?;
        let source = capability
            .mcp_app_source
            .clone()
            .ok_or(McpAppError::Stale)?;
        let guard = capability.tool.clone();
        let definition = capability.definition().clone();
        let identity = capability.identity();
        let policy = dispatcher.operation_policy();
        let association = inner.association.clone();
        let output = Arc::new(Mutex::new(None));
        let captured = output.clone();
        let audit_data = Arc::new(Mutex::new(None));
        let captured_audit = audit_data.clone();
        let errors = Arc::new(Mutex::new(None));
        let captured_errors = errors.clone();
        capability.tool = AgentTool::new_async(definition.clone(), move |context| {
            let errors = captured_errors.clone();
            let runtime = mcp.clone();
            let source = source.clone();
            let association = association.clone();
            let policy = policy.clone();
            let captured = captured.clone();
            let audit = captured_audit.clone();
            Box::pin(async move {
                if let Err(error) = validate_live_source(
                    &runtime,
                    &association,
                    context.cancellation_token().clone(),
                )
                .await
                {
                    return Ok(known_error(&context, &errors, error));
                }
                if runtime
                    .current_app_source(&source.server, &source.tool)
                    .await
                    .as_ref()
                    != Some(&source)
                {
                    return Ok(known_error(&context, &errors, McpAppError::Stale));
                }
                let route = McpOperation::for_context(policy.as_ref(), &context);
                let (projected, raw) = runtime
                    .call_app_tool(
                        &source.server,
                        &source.tool,
                        context.call(),
                        context.cancellation_token().clone(),
                        route,
                    )
                    .await?;
                if let Err(error) = validate_live_source(
                    &runtime,
                    &association,
                    context.cancellation_token().clone(),
                )
                .await
                {
                    return Ok(known_error(&context, &errors, error));
                }
                if runtime
                    .current_app_source(&source.server, &source.tool)
                    .await
                    .as_ref()
                    != Some(&source)
                {
                    return Ok(known_error(&context, &errors, McpAppError::Stale));
                }
                *audit.lock().expect("app audit lock poisoned") =
                    Some(json!({"arguments":context.call().arguments,"result":raw.raw}));
                *captured.lock().expect("app tool lock poisoned") = Some(raw);
                Ok(projected)
            })
        })
        .with_identity(identity)
        .with_execution_guard(move |cancel| guard.check_execution(cancel));
        let effect = dispatcher.scoped_effect(capability).map_err(failure)?;
        let call = ToolCall::new(
            uuid::Uuid::now_v7().to_string(),
            &definition.name,
            arguments,
        );
        tokio::time::timeout_at(
            inner.expires,
            self.execute(
                &record,
                &inner.call_id,
                &inner.association.server,
                tool,
                effect,
                call,
                inner.token.child_token(),
                cancellation,
                Some(audit_data),
            ),
        )
        .await
        .map_err(|_| {
            inner.token.cancel();
            McpAppError::Canceled
        })??;
        if let Some(error) = errors.lock().expect("app error lock poisoned").take() {
            return Err(error);
        }
        let raw = output
            .lock()
            .expect("app tool lock poisoned")
            .take()
            .ok_or(McpAppError::Denied)?;
        Ok(raw)
    }
    async fn execute(
        &self,
        record: &SessionRecord,
        recorded: &str,
        server: &str,
        tool: &str,
        effect: AgentTool,
        call: ToolCall,
        token: CancellationToken,
        cancellation: CancellationToken,
        audit_data: Option<Arc<Mutex<Option<Value>>>>,
    ) -> Result<(), McpAppError> {
        let session = record.id.clone();
        let turn = record.snapshot().turn;
        let recorded = recorded.to_owned();
        let server = server.to_owned();
        let tool = tool.to_owned();
        let arguments = call.arguments.clone();
        let audit = move |result: &waltr_agent::ToolResult| {
            let detail = audit_data
                .and_then(|value| value.lock().expect("app audit lock poisoned").take())
                .unwrap_or_else(|| json!({"arguments":arguments,"result":result}));
            PendingTimelineEvent::new(
                session,
                turn,
                PendingTimelineItemId::New,
                TimelineEventKind::ItemFinished,
                Some(TimelineChannel::from("mcp_app_audit")),
                TimelineContent::Json {
                    value: json!({"version":1,"recordedCallId":recorded,"server":server,"tool":tool,"callId":result.call_id,
                    "arguments":detail["arguments"],"result":detail["result"]}),
                },
            )
        };
        let work = self.sessions.execute_idle_host_tool(
            &record.id,
            record.revision,
            effect,
            call,
            audit,
            token.clone(),
        );
        tokio::pin!(work);
        let _result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => { token.cancel(); drop(work.await); return Err(McpAppError::Canceled); },
            _ = token.cancelled() => { drop(work.await); return Err(McpAppError::Canceled); },
            result = &mut work => result.map_err(|error| match error {
                waltr_agent::IdleHostEffectError::Busy => McpAppError::Busy,
                waltr_agent::IdleHostEffectError::Stale => McpAppError::Stale,
                waltr_agent::IdleHostEffectError::Canceled => McpAppError::Canceled,
                waltr_agent::IdleHostEffectError::Effect(error) => failure(error),
            })?,
        };
        if token.is_cancelled() {
            return Err(McpAppError::Canceled);
        }
        Ok(())
    }
    pub fn close_session_mcp_app(&self, handle: &McpAppHandle) {
        handle.inner.token.cancel();
    }
}

fn validate_source(
    dispatcher: &CapabilityDispatcher,
    association: &McpAppAssociation,
) -> Result<crate::permission::PermissionPolicy, McpAppError> {
    if association.version != 1 {
        return Err(McpAppError::Stale);
    }
    let cap = dispatcher
        .capabilities()
        .find(|cap| {
            cap.mcp_app_source.as_ref().is_some_and(|source| {
                source.server == association.server
                    && source.tool == association.tool
                    && source.source == association.source
                    && source.declaration == association.declaration
                    && source.ui.resource_uri.as_ref() == Some(&association.resource_uri)
            })
        })
        .ok_or(McpAppError::Stale)?;
    let policy = dispatcher.permission_policy();
    let level = policy
        .evaluate_target_tool_effect(
            cap.tool.identity().ok_or(McpAppError::Stale)?,
            &cap.definition().name,
        )
        .level;
    match level {
        crate::permission::PermissionLevel::Allow | crate::permission::PermissionLevel::Ask => {}
        crate::permission::PermissionLevel::Deny => return Err(McpAppError::Denied),
    }
    Ok(crate::permission::PermissionPolicy::new(level))
}
async fn validate_live_source(
    runtime: &McpClientRuntime,
    association: &McpAppAssociation,
    cancellation: CancellationToken,
) -> Result<(), McpAppError> {
    if association.source.is_empty()
        || !runtime.app_authority_current(&association.server).await
        || !runtime
            .refresh_app_source(&association.server, &association.tool, cancellation)
            .await
            .map_err(failure)?
            .is_some_and(|source| {
                source.source == association.source && source.declaration == association.declaration
            })
    {
        return Err(McpAppError::Stale);
    }
    Ok(())
}
fn recorded_association(
    page: &waltr_agent::SessionTimelinePage,
    call_id: &str,
) -> Result<McpAppAssociation, McpAppError> {
    let events = page
        .events
        .iter()
        .filter_map(|event| match &event.event {
            SessionTimelineProjectedEvent::Durable(event) => Some(event),
            _ => None,
        })
        .collect::<Vec<_>>();
    let calls = events.iter().filter(|event| matches!(&event.content, TimelineContent::ToolCall { call } if call.id == call_id)).collect::<Vec<_>>();
    let [call] = calls.as_slice() else {
        return Err(McpAppError::Stale);
    };
    let results = events.iter().filter(|event| event.kind == TimelineEventKind::ItemFinished && event.status == Some(waltr_agent::TimelineStatus::Finished)
        && event.parent_id.as_ref() == Some(&call.item_id) && matches!(&event.content, TimelineContent::ToolResult { result } if result.call_id == call_id)).collect::<Vec<_>>();
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
