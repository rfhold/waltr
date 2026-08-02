//! Waltr Model Context Protocol server surface.

#![allow(
    clippy::useless_vec,
    reason = "the progressive_server macro generates this allocation"
)]

use crate::{config_service::ConfigService, session::SessionHost};
use futures_util::StreamExt as _;
use jaq_interpret::{Ctx, FilterT as _, ParseCtx, RcIter, Val};
use waltr::grpc::waltr::config::v1::{
    GetConfigRequest, GetConfigResponse, PatchConfigRequest,
    config_service_server::ConfigService as GeneratedConfigService,
};
use waltr_agent::{
    AgentSessionRunDisposition, AuthTarget, ExternalRequest, ExternalRequestKind,
    ExternalRequestStatus, PendingWork, PendingWorkStatus, SessionId, SessionRuntimeSettings,
    SessionStatus, SessionSummary, Suspension,
};
use mcp::server::{ServerError, ServerResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{sync::Arc, time::Duration};
use tonic::{Code, Request, Status};

const DEFAULT_WAIT_SECONDS: u64 = 60;
const MAX_WAIT_SECONDS: u64 = 300;
const WAIT_RECONCILE_INTERVAL: Duration = Duration::from_millis(500);

/// MCP server backed by Waltr's registered configuration and session services.
#[derive(Clone)]
pub struct WaltrMcpServer {
    config_service: Arc<ConfigService>,
    sessions: Arc<dyn SessionHost>,
}

impl std::fmt::Debug for WaltrMcpServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WaltrMcpServer")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionIdInput {
    /// Hosted session id.
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WipeInput {
    /// Must be true to delete every hosted agent session.
    confirm: bool,
}

#[derive(Debug, Serialize)]
struct WipeResult {
    wiped: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendInput {
    /// Hosted session id.
    id: String,
    /// User prompt to submit.
    prompt: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitInput {
    /// Hosted session id.
    id: String,
    /// jq expression evaluated against the same projection returned by `get`.
    jq: String,
    /// Maximum wait in seconds. Defaults to 60 and cannot exceed 300.
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PatchConfigurationInput {
    /// Current configuration `ETag` from `configuration.inspect`.
    expected_etag: String,
    /// Replacement providers section: `{ "providers": { "key": { ... } } }`.
    #[serde(default)]
    providers: Option<Value>,
    /// Replacement MCP servers section: `{ "mcp_servers": { "key": { ... } }`.
    #[serde(default)]
    mcp_servers: Option<Value>,
    /// Replacement runtime defaults section.
    #[serde(default)]
    runtime_defaults: Option<Value>,
}

impl WaltrMcpServer {
    /// Create a Waltr MCP server backed by the same services registered with gRPC.
    #[must_use]
    pub fn new(config_service: Arc<ConfigService>, sessions: Arc<dyn SessionHost>) -> Self {
        Self {
            config_service,
            sessions,
        }
    }

    async fn get_config_response(&self) -> ServerResult<GetConfigResponse> {
        let response = GeneratedConfigService::get_config(
            self.config_service.as_ref(),
            Request::new(GetConfigRequest::default()),
        )
        .await
        .map_err(|status| rpc_status_to_server_error(&status))?;
        Ok(response.into_inner())
    }

    async fn session_projection(&self, id: &SessionId) -> ServerResult<Value> {
        let session = self.sessions.load(id).await.map_err(session_error)?;
        let active = self.sessions.active_state(id).await;
        Ok(project_session(&session.record, active))
    }
}

#[allow(missing_docs)]
#[mcp::progressive_server(
    name = "waltr",
    version = env!("CARGO_PKG_VERSION"),
    description = "Waltr server configuration and hosted session MCP surface.",
    tool(
        name = "exec",
        description = "Inspect and update Waltr server configuration.",
        namespace(
            configuration,
            description = "Inspect and update Waltr server configuration through the same RPC contract used by Waltr clients."
        ),
        runbook_uri_template = "waltr://runbooks/{runbook}"
    ),
    tool(
        name = "sessions",
        description = "Inspect and control hosted Waltr sessions."
    )
)]
impl WaltrMcpServer {
    /// Inspect the current Waltr configuration before proposing or applying a change.
    #[action(
        tool = "exec",
        name = "configuration.inspect",
        resource = "waltr://configuration/current"
    )]
    async fn inspect_configuration(&self) -> ServerResult<Value> {
        response_to_value(self.get_config_response().await?)
    }

    /// Patch the Waltr configuration with whole-section replacements.
    #[action(tool = "exec", name = "configuration.patch")]
    async fn patch_configuration(&self, input: PatchConfigurationInput) -> ServerResult<Value> {
        let response = GeneratedConfigService::patch_config(
            self.config_service.as_ref(),
            Request::new(input.into_request()?),
        )
        .await
        .map_err(|status| rpc_status_to_server_error(&status))?;
        response_to_value(response.into_inner())
    }

    /// Configure Waltr by inspecting, preparing a minimal patch, and applying it.
    #[runbook(tool = "exec", name = "configuration")]
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    fn configuration_runbook(&self) -> ServerResult<String> {
        Ok("## Waltr configuration runbook\n\n\
1. Call `configuration.inspect` to load the current configuration and ETag.\n\
2. Decide the smallest top-level section replacement needed: `providers`, `mcp_servers`, or `runtime_defaults`.\n\
3. Build the replacement carefully: any provided top-level key replaces that entire section.\n\
4. Call `configuration.patch` with `expected_etag` from the inspected config.\n\
5. On validation or stale-ETag errors, inspect again before retrying.\n"
            .to_owned())
    }

    /// List durable hosted sessions, newest activity first.
    #[action(tool = "sessions", name = "list")]
    async fn list_sessions(&self) -> ServerResult<Value> {
        let summaries = self.sessions.list().await.map_err(session_error)?;
        let mut sessions = Vec::with_capacity(summaries.len());
        for summary in summaries {
            let active = self.sessions.active_state(&summary.id).await;
            sessions.push(project_summary(summary, active));
        }
        Ok(json!({ "sessions": sessions }))
    }

    /// Get one durable session using the stable MCP session projection.
    #[action(tool = "sessions", name = "get")]
    async fn get_session(&self, input: SessionIdInput) -> ServerResult<Value> {
        self.session_projection(&required_session_id(input.id)?)
            .await
    }

    /// Blindly delete every hosted agent session. Operators must ensure sessions are idle.
    #[action(tool = "sessions", name = "wipe")]
    async fn wipe_sessions(&self, input: WipeInput) -> ServerResult<Value> {
        if !input.confirm {
            return Err(ServerError::invalid_params("confirm must be true"));
        }
        self.sessions.wipe().await.map_err(session_error)?;
        response_to_value(WipeResult { wiped: true })
    }

    /// Idempotently interrupt a hosted session and cancel its queued runs.
    #[action(tool = "sessions", name = "interrupt")]
    async fn interrupt_session(&self, input: SessionIdInput) -> ServerResult<Value> {
        let id = required_session_id(input.id)?;
        self.sessions.interrupt(&id).await.map_err(session_error)?;
        self.session_projection(&id).await
    }

    /// Submit a prompt, queueing it when the session already has an active run.
    #[action(tool = "sessions", name = "send")]
    async fn send_session(&self, input: SendInput) -> ServerResult<Value> {
        let id = required_session_id(input.id)?;
        if input.prompt.trim().is_empty() {
            return Err(ServerError::invalid_params("prompt must not be empty"));
        }
        self.sessions.load(&id).await.map_err(session_error)?;
        let submission = self
            .sessions
            .submit_prompt(id, input.prompt)
            .await
            .map_err(run_error)?;
        let (disposition, queue_position) = match submission.disposition {
            AgentSessionRunDisposition::Started => ("started", None),
            AgentSessionRunDisposition::ReplacedActive => ("replaced_active", None),
            AgentSessionRunDisposition::Queued { position } => ("queued", Some(position)),
        };
        Ok(json!({
            "session_id": submission.session_id.as_str(),
            "run_id": submission.run_id,
            "disposition": disposition,
            "queue_position": queue_position
        }))
    }

    /// Wait until a jq condition matches, the session stops running, or time expires.
    #[action(tool = "sessions", name = "wait")]
    async fn wait_session(&self, input: WaitInput) -> ServerResult<Value> {
        let id = required_session_id(input.id)?;
        let timeout_seconds = input.timeout_seconds.unwrap_or(DEFAULT_WAIT_SECONDS);
        if timeout_seconds == 0 || timeout_seconds > MAX_WAIT_SECONDS {
            return Err(ServerError::invalid_params(
                "timeout_seconds must be between 1 and 300",
            ));
        }

        let mut defs = ParseCtx::new(Vec::new());
        let (parsed, parse_errors) = jaq_parse::parse(&input.jq, jaq_parse::main());
        if !parse_errors.is_empty() {
            return Err(ServerError::invalid_params(format!(
                "invalid jq filter: {parse_errors:?}"
            )));
        }
        let parsed = parsed.ok_or_else(|| ServerError::invalid_params("empty jq filter"))?;
        let filter = defs.compile(parsed);
        if !defs.errs.is_empty() {
            let errors = defs
                .errs
                .iter()
                .map(|(error, _)| error.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ServerError::invalid_params(format!(
                "invalid jq filter: {errors}"
            )));
        }

        // Subscribe before the initial durable load so no coordinator update can fall in between.
        let mut updates = self.sessions.watch_sessions();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
        let mut reconcile = tokio::time::interval_at(
            tokio::time::Instant::now() + WAIT_RECONCILE_INTERVAL,
            WAIT_RECONCILE_INTERVAL,
        );

        loop {
            let projection = self.session_projection(&id).await?;
            let inputs = RcIter::new(std::iter::empty());
            let outputs = filter
                .run((Ctx::new([], &inputs), Val::from(projection.clone())))
                .map(|output| {
                    output.map(Value::from).map_err(|error| {
                        ServerError::invalid_params(format!("jq filter failed: {error}"))
                    })
                })
                .collect::<ServerResult<Vec<_>>>()?;
            let evaluated_output = jq_output(&outputs);
            if outputs.iter().any(jq_truthy) {
                return Ok(wait_result("condition_met", projection, evaluated_output));
            }
            if !projection["running"].as_bool().unwrap_or(false) {
                return Ok(wait_result(
                    "session_not_running",
                    projection,
                    evaluated_output,
                ));
            }

            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    let projection = self.session_projection(&id).await?;
                    let inputs = RcIter::new(std::iter::empty());
                    let outputs = filter
                        .run((Ctx::new([], &inputs), Val::from(projection.clone())))
                        .map(|output| output.map(Value::from).map_err(|error| {
                            ServerError::invalid_params(format!("jq filter failed: {error}"))
                        }))
                        .collect::<ServerResult<Vec<_>>>()?;
                    let reason = if outputs.iter().any(jq_truthy) {
                        "condition_met"
                    } else if !projection["running"].as_bool().unwrap_or(false) {
                        "session_not_running"
                    } else {
                        "timeout"
                    };
                    return Ok(wait_result(reason, projection, jq_output(&outputs)));
                }
                _ = reconcile.tick() => {
                    // Reload durable state periodically to reconcile lagged or process-local events.
                }
                update = updates.next() => {
                    if let Some(update) = update {
                        let update_id = match &update.event {
                            waltr_agent::SessionListEvent::Upsert(summary) => &summary.id,
                            waltr_agent::SessionListEvent::Deleted(deleted_id) => deleted_id,
                            waltr_agent::SessionListEvent::Cleared => continue,
                        };
                        if update_id != &id {
                            continue;
                        }
                    }
                }
            }
        }
    }
}

impl PatchConfigurationInput {
    fn into_request(self) -> ServerResult<PatchConfigRequest> {
        if self.expected_etag.trim().is_empty() {
            return Err(ServerError::invalid_params("missing expected_etag"));
        }
        let mut request = Map::new();
        request.insert(
            "expected_etag".to_owned(),
            Value::String(self.expected_etag),
        );
        if let Some(value) = self.providers {
            request.insert("providers".to_owned(), value);
        }
        if let Some(value) = self.mcp_servers {
            request.insert("mcp_servers".to_owned(), value);
        }
        if let Some(value) = self.runtime_defaults {
            request.insert("runtime_defaults".to_owned(), value);
        }
        serde_json::from_value(Value::Object(request)).map_err(|error| {
            ServerError::invalid_params(format!("invalid configuration patch input: {error}"))
        })
    }
}

fn required_session_id(value: String) -> ServerResult<SessionId> {
    if value.trim().is_empty() {
        Err(ServerError::invalid_params("session id must not be empty"))
    } else {
        Ok(SessionId::new(value))
    }
}

fn project_session(
    record: &waltr_agent::SessionRecord,
    active: Option<waltr_agent::AgentSessionActiveRunState>,
) -> Value {
    let snapshot = record.snapshot();
    let summary = record.summary();
    let mut value = project_summary(summary, active);
    let object = value
        .as_object_mut()
        .expect("session projection is an object");
    object.insert("turn".to_owned(), json!(snapshot.turn));
    object.insert(
        "pending_work".to_owned(),
        Value::Array(snapshot.pending_work.iter().map(project_work).collect()),
    );
    object.insert(
        "active_external_requests".to_owned(),
        Value::Array(
            snapshot
                .external_requests
                .iter()
                .filter(|request| matches!(request.status, ExternalRequestStatus::Pending))
                .map(project_external_request)
                .collect(),
        ),
    );
    value
}

fn project_summary(
    summary: SessionSummary,
    active: Option<waltr_agent::AgentSessionActiveRunState>,
) -> Value {
    let (status, durably_running, suspension, failure, retry) = project_status(&summary.status);
    let running = session_is_running(&summary.status, durably_running, active.is_some());
    json!({
        "id": summary.id.as_str(),
        "revision": summary.revision.get(),
        "status": status,
        "running": running,
        "suspension": suspension,
        "failure": failure,
        "retry": retry,
        "active_run": active.map(|state| json!({
            "run_id": state.run_id,
            "queued_count": state.queued_count
        })),
        "current_prompt": summary.current_prompt,
        "timestamps": {
            "created_at_ms": unix_millis(summary.created_at),
            "updated_at_ms": unix_millis(summary.updated_at),
            "last_activity_at_ms": unix_millis(summary.last_activity_at)
        },
        "runtime": summary.runtime_settings.as_ref().map(project_runtime),
        "metrics": summary.metrics
    })
}

fn session_is_running(status: &SessionStatus, durably_running: bool, has_active_run: bool) -> bool {
    durably_running || (has_active_run && !matches!(status, SessionStatus::Suspended(_)))
}

fn project_status(status: &SessionStatus) -> (&'static str, bool, Value, Value, Value) {
    match status {
        SessionStatus::Idle => ("idle", false, Value::Null, Value::Null, Value::Null),
        SessionStatus::BuildingRequest => (
            "building_request",
            true,
            Value::Null,
            Value::Null,
            Value::Null,
        ),
        SessionStatus::StreamingProvider => (
            "streaming_provider",
            true,
            Value::Null,
            Value::Null,
            Value::Null,
        ),
        SessionStatus::RetryingProvider {
            attempt,
            max_attempts,
            delay_ms,
            message,
        } => (
            "retrying_provider",
            true,
            Value::Null,
            Value::Null,
            json!({
                "attempt": attempt,
                "max_attempts": max_attempts,
                "delay_ms": delay_ms,
                "message": message
            }),
        ),
        SessionStatus::EvaluatingToolCall => (
            "evaluating_tool_call",
            true,
            Value::Null,
            Value::Null,
            Value::Null,
        ),
        SessionStatus::RunningTool => ("running_tool", true, Value::Null, Value::Null, Value::Null),
        SessionStatus::Suspended(suspension) => (
            "suspended",
            false,
            project_suspension(suspension),
            Value::Null,
            Value::Null,
        ),
        SessionStatus::Finished => ("finished", false, Value::Null, Value::Null, Value::Null),
        SessionStatus::Failed {
            message,
            authentication_failure,
        } => (
            "failed",
            false,
            Value::Null,
            json!({
                "message": message,
                "authentication": authentication_failure.as_ref().map(|failure| {
                    let (target_kind, target_key) = match &failure.target {
                        AuthTarget::Provider { key } => ("provider", key.as_str()),
                        AuthTarget::McpServer { key } => ("mcp_server", key.as_str()),
                    };
                    json!({
                        "target_kind": target_kind,
                        "target_key": target_key,
                        "reason": snake_debug(&failure.reason)
                    })
                })
            }),
            Value::Null,
        ),
    }
}

fn project_suspension(suspension: &Suspension) -> Value {
    match suspension {
        Suspension::AwaitingToolResults { work } => json!({
            "kind": "awaiting_tool_results",
            "work": work.iter().map(project_work).collect::<Vec<_>>()
        }),
        Suspension::AwaitingExternalInput { request } => json!({
            "kind": "awaiting_external_input",
            "request": project_external_request(request)
        }),
        Suspension::Interrupted { work } => json!({
            "kind": "interrupted",
            "work": work.iter().map(project_work).collect::<Vec<_>>()
        }),
    }
}

fn project_work(work: &PendingWork) -> Value {
    let (status, detail) = match &work.status {
        PendingWorkStatus::Pending => ("pending", Value::Null),
        PendingWorkStatus::Canceled(reason) => ("canceled", json!(snake_debug(reason))),
        PendingWorkStatus::Finished(_) => ("finished", Value::Null),
        PendingWorkStatus::Failed { message } => ("failed", json!(message)),
    };
    json!({ "id": work.id, "turn": work.turn, "status": status, "detail": detail })
}

fn project_external_request(request: &ExternalRequest) -> Value {
    let (kind, custom_kind) = match &request.kind {
        ExternalRequestKind::Permission => ("permission", None),
        ExternalRequestKind::Question => ("question", None),
        ExternalRequestKind::ToolInput => ("tool_input", None),
        ExternalRequestKind::Custom(kind) => ("custom", Some(kind.as_str())),
    };
    json!({
        "id": request.id.as_str(),
        "kind": kind,
        "custom_kind": custom_kind,
        "tool_call_id": request.tool_call_id,
        "prompt": request.prompt,
        "payload": request.payload
    })
}

fn project_runtime(runtime: &SessionRuntimeSettings) -> Value {
    json!({
        "provider": {
            "key": runtime.provider.key,
            "api": snake_debug(&runtime.provider.api),
            "url": runtime.provider.url,
            "model_id": runtime.provider.model_id,
            "reasoning_effort": runtime.provider.reasoning_effort
        },
        "mcp_servers": runtime.mcp_servers.iter().map(|server| json!({
            "key": server.key,
            "kind": snake_debug(&server.kind),
            "command": server.command,
            "url": server.url
        })).collect::<Vec<_>>(),
        "mcp_trace_propagation": runtime.mcp_trace_propagation
    })
}

fn snake_debug(value: &impl std::fmt::Debug) -> String {
    let text = format!("{value:?}");
    let mut output = String::with_capacity(text.len());
    for (index, character) in text.chars().enumerate() {
        if character.is_ascii_uppercase() && index != 0 {
            output.push('_');
        }
        output.push(character.to_ascii_lowercase());
    }
    output
}

fn unix_millis(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn jq_truthy(value: &Value) -> bool {
    !matches!(value, Value::Bool(false) | Value::Null)
}

fn jq_output(outputs: &[Value]) -> Value {
    match outputs {
        [] => Value::Null,
        [value] => value.clone(),
        values => Value::Array(values.to_vec()),
    }
}

fn wait_result(reason: &str, session: Value, jq_output: Value) -> Value {
    json!({ "reason": reason, "session": session, "jq_output": jq_output })
}

fn response_to_value<T: Serialize>(response: T) -> ServerResult<Value> {
    serde_json::to_value(response)
        .map_err(|error| ServerError::internal(format!("failed to serialize response: {error}")))
}

fn session_error(error: waltr_agent::LlmError) -> ServerError {
    let message = error.to_string();
    if message.contains("session not found:") {
        ServerError::invalid_params(message)
    } else {
        ServerError::internal(message)
    }
}

fn run_error(error: waltr_agent::AgentSessionRunError) -> ServerError {
    let message = error.to_string();
    match error {
        waltr_agent::AgentSessionRunError::AlreadyRunning { .. }
        | waltr_agent::AgentSessionRunError::Canceled { .. }
        | waltr_agent::AgentSessionRunError::Runtime(
            waltr_agent::AgentSessionRuntimeError::FailedPrecondition(_),
        ) => ServerError::invalid_params(message),
        _ => session_error(waltr_agent::LlmError::Protocol {
            provider: "session-run",
            message,
        }),
    }
}

fn rpc_status_to_server_error(status: &Status) -> ServerError {
    let message = format!("config RPC {}: {}", status.code(), status.message());
    match status.code() {
        Code::InvalidArgument | Code::FailedPrecondition => ServerError::invalid_params(message),
        _ => ServerError::internal(message),
    }
}

#[cfg(test)]
mod tests {
    use super::session_is_running;
    use waltr_agent::{SessionStatus, Suspension};

    #[test]
    fn active_run_bridges_pre_journal_status_without_overriding_suspension() {
        assert!(session_is_running(&SessionStatus::Idle, false, true));
        assert!(session_is_running(&SessionStatus::Finished, false, true));
        assert!(!session_is_running(
            &SessionStatus::Suspended(Suspension::AwaitingToolResults { work: Vec::new() }),
            false,
            true,
        ));
        assert!(session_is_running(
            &SessionStatus::StreamingProvider,
            true,
            false,
        ));
    }
}
