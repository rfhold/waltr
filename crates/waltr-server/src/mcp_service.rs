//! Waltr Model Context Protocol server surface.

#![allow(
    clippy::useless_vec,
    reason = "the progressive_server macro generates this allocation"
)]

use crate::{
    agent_definition::{AgentDefinitionLoadError, AgentDefinitionState},
    session::SessionHost,
};
use futures_util::StreamExt as _;
use jaq_interpret::{Ctx, FilterT as _, ParseCtx, RcIter, Val};
use waltr_agent::{
    AgentSessionRunDisposition, AuthTarget, ExternalRequest, ExternalRequestKind,
    ExternalRequestStatus, InferenceSettings, PendingWork, PendingWorkStatus, SessionId,
    SessionStatus, SessionSummary, Suspension,
};
use mcp::server::{ServerError, ServerResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tonic::{Code, Status};

const DEFAULT_WAIT_SECONDS: u64 = 60;
const MAX_WAIT_SECONDS: u64 = 300;
const WAIT_RECONCILE_INTERVAL: Duration = Duration::from_millis(500);

/// MCP server backed by Waltr's registered configuration and session services.
#[derive(Clone)]
pub struct WaltrMcpServer {
    sessions: Arc<dyn SessionHost>,
    resources: AgentDefinitionState,
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
struct CreateInput {
    /// Exact startup agent name. Omit to use the server default.
    #[serde(default)]
    agent_name: Option<String>,
    /// Initial user prompt to start immediately.
    prompt: String,
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
struct InspectResourcesInput {
    /// Canonical relative path such as `agents/coding.yaml`. Omit to list all entries.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReplaceResourceInput {
    /// Canonical relative path such as `agents/coding.yaml`.
    path: String,
    /// Complete strict YAML source for exactly one resource.
    source: String,
    /// Current persisted-slot `ETag` from `resources.inspect`.
    expected_etag: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ResetResourceInput {
    /// Canonical relative path such as `agents/coding.yaml`.
    path: String,
    /// Current persisted-slot `ETag` from `resources.inspect`.
    expected_etag: String,
}

impl WaltrMcpServer {
    /// Create a Waltr MCP server backed by the same services registered with gRPC.
    #[must_use]
    pub fn new(sessions: Arc<dyn SessionHost>, resources: AgentDefinitionState) -> Self {
        Self {
            sessions,
            resources,
        }
    }

    const fn resources(&self) -> &AgentDefinitionState {
        &self.resources
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
    description = "Waltr resource administration and hosted session MCP surface.",
    tool(
        name = "exec",
        description = "Inspect and update complete Waltr resources.",
        namespace(
            resources,
            description = "Inspect, replace, and reset complete live YAML resources."
        ),
        runbook_uri_template = "waltr://runbooks/{runbook}"
    ),
    tool(
        name = "sessions",
        description = "Inspect and control hosted Waltr sessions."
    )
)]
impl WaltrMcpServer {
    /// Inspect all active resources or one canonical persisted slot.
    #[action(tool = "exec", name = "resources.inspect")]
    fn inspect_resources(&self, input: InspectResourcesInput) -> ServerResult<Value> {
        let path = input.path;
        let resources = self
            .resources()
            .inspect(path.as_deref())
            .map_err(|error| agent_definition_error(&error))?;
        response_to_value(resources)
    }

    /// Validate and atomically replace one complete resource file.
    #[action(tool = "exec", name = "resources.replace")]
    async fn replace_resource(&self, input: ReplaceResourceInput) -> ServerResult<Value> {
        let result = self
            .resources()
            .replace(&input.path, input.source, &input.expected_etag)
            .await
            .map_err(|error| agent_definition_error(&error))?;
        drop(result);
        self.inspect_resources(InspectResourcesInput {
            path: Some(input.path),
        })
    }

    /// Remove one persisted resource, restoring its bundle fallback when present.
    #[action(tool = "exec", name = "resources.reset")]
    async fn reset_resource(&self, input: ResetResourceInput) -> ServerResult<Value> {
        let result = self
            .resources()
            .reset(&input.path, &input.expected_etag)
            .await
            .map_err(|error| agent_definition_error(&error))?;
        drop(result);
        self.inspect_resources(InspectResourcesInput {
            path: Some(input.path),
        })
    }

    /// Administer Waltr resources with stale-write protection.
    #[runbook(tool = "exec", name = "resources")]
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    fn configuration_runbook(&self) -> ServerResult<String> {
        Ok("## Waltr resource runbook\n\n\
1. Call `resources.inspect` for the canonical resource path.\n\
2. Edit one complete strict YAML resource.\n\
3. Call `resources.replace` with its persisted-slot `expected_etag`.\n\
4. Use `resources.reset` to remove a persisted override and reveal its bundled fallback.\n\
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
            sessions.push(project_summary(&summary, active));
        }
        Ok(json!({ "sessions": sessions }))
    }

    /// Create a hosted session and immediately start its required prompt.
    #[action(tool = "sessions", name = "create")]
    async fn create_session(&self, input: CreateInput) -> ServerResult<Value> {
        if input.prompt.trim().is_empty() {
            return Err(ServerError::invalid_params("prompt must not be empty"));
        }
        let session = self
            .sessions
            .create(input.agent_name)
            .await
            .map_err(|status| session_creation_error(&status))?;
        let session_id = session.record.id.clone();
        let agent_name = session.record.summary().agent_name;
        let submission = self
            .sessions
            .submit_prompt(session_id.clone(), input.prompt)
            .await
            .map_err(|error| {
                ServerError::internal(format!(
                    "session {} was created, but prompt submission failed: {error}",
                    session_id.as_str()
                ))
            })?;
        let (disposition, queue_position) = match submission.disposition {
            AgentSessionRunDisposition::Started => ("started", None),
            AgentSessionRunDisposition::ReplacedActive => ("replaced_active", None),
            AgentSessionRunDisposition::Queued { position } => ("queued", Some(position)),
        };
        Ok(json!({
            "session_id": submission.session_id.as_str(),
            "agent_name": agent_name,
            "run_id": submission.run_id,
            "disposition": disposition,
            "queue_position": queue_position
        }))
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
    #[allow(
        clippy::too_many_lines,
        reason = "the wait loop keeps jq evaluation and race-free session reconciliation together"
    )]
    async fn wait_session(&self, input: WaitInput) -> ServerResult<Value> {
        let id = required_session_id(input.id)?;
        let timeout_seconds = input.timeout_seconds.unwrap_or(DEFAULT_WAIT_SECONDS);
        if timeout_seconds == 0 || timeout_seconds > MAX_WAIT_SECONDS {
            return Err(ServerError::invalid_params(
                "timeout_seconds must be between 1 and 300",
            ));
        }

        let mut defs = ParseCtx::new(Vec::new());
        defs.insert_natives(jaq_core::core());
        defs.insert_defs(jaq_std::std());
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
            let outputs = evaluate_jq(&filter, &projection)?;
            let evaluated_output = jq_output(&outputs);
            if outputs.iter().any(jq_truthy) {
                return Ok(wait_result("condition_met", &projection, &evaluated_output));
            }
            if !projection["running"].as_bool().unwrap_or(false) {
                return Ok(wait_result(
                    "session_not_running",
                    &projection,
                    &evaluated_output,
                ));
            }

            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {
                    let projection = self.session_projection(&id).await?;
                    let outputs = evaluate_jq(&filter, &projection)?;
                    let reason = if outputs.iter().any(jq_truthy) {
                        "condition_met"
                    } else if !projection["running"].as_bool().unwrap_or(false) {
                        "session_not_running"
                    } else {
                        "timeout"
                    };
                    return Ok(wait_result(reason, &projection, &jq_output(&outputs)));
                }
                _ = reconcile.tick() => {
                    // Reload durable state periodically to reconcile lagged or process-local events.
                }
                _update = updates.next() => {}
            }
        }
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
    let mut value = project_summary(&summary, active);
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
    summary: &SessionSummary,
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
        "inference": summary.inference_settings.as_ref().map(project_inference),
        "metrics": summary.metrics,
        "metadata": &summary.metadata_values
    })
}

const fn session_is_running(
    status: &SessionStatus,
    durably_running: bool,
    has_active_run: bool,
) -> bool {
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

fn project_inference(settings: &InferenceSettings) -> Value {
    json!({
        "provider": {
            "key": settings.provider.key,
            "api": snake_debug(&settings.provider.api),
            "url": settings.provider.url,
            "model_id": settings.provider.model_id,
            "reasoning_effort": settings.provider.reasoning_effort
        }
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

const fn jq_truthy(value: &Value) -> bool {
    !matches!(value, Value::Bool(false) | Value::Null)
}

fn evaluate_jq(filter: &jaq_interpret::Filter, input: &Value) -> ServerResult<Vec<Value>> {
    const MAX_FILTER_OUTPUTS: usize = 256;

    let inputs = RcIter::new(std::iter::empty());
    let mut outputs = Vec::new();
    for output in filter.run((Ctx::new([], &inputs), Val::from(input.clone()))) {
        let output = output
            .map(Value::from)
            .map_err(|error| ServerError::invalid_params(format!("jq filter failed: {error}")))?;
        if outputs.len() == MAX_FILTER_OUTPUTS {
            return Err(ServerError::invalid_params(format!(
                "jq filter produced more than {MAX_FILTER_OUTPUTS} results"
            )));
        }
        outputs.push(output);
    }
    Ok(outputs)
}

fn jq_output(outputs: &[Value]) -> Value {
    match outputs {
        [] => Value::Null,
        [value] => value.clone(),
        values => Value::Array(values.to_vec()),
    }
}

fn wait_result(reason: &str, session: &Value, jq_output: &Value) -> Value {
    json!({ "reason": reason, "session": session, "jq_output": jq_output })
}

fn response_to_value<T: Serialize>(response: T) -> ServerResult<Value> {
    serde_json::to_value(response)
        .map_err(|error| ServerError::internal(format!("failed to serialize response: {error}")))
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the signature is used directly as a Result::map_err conversion"
)]
fn session_error(error: waltr_agent::LlmError) -> ServerError {
    let message = error.to_string();
    if message.contains("session not found:") {
        ServerError::invalid_params(message)
    } else {
        ServerError::internal(message)
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the signature is used directly as a Result::map_err conversion"
)]
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

fn session_creation_error(status: &Status) -> ServerError {
    if status.code() == Code::InvalidArgument {
        ServerError::invalid_params(status.message())
    } else {
        ServerError::internal(format!(
            "session creation failed with {}: {}",
            status.code(),
            status.message()
        ))
    }
}

fn agent_definition_error(error: &AgentDefinitionLoadError) -> ServerError {
    if error.is_failed_precondition() {
        ServerError::invalid_params(
            "failed precondition: agent-definition state changed; inspect and retry",
        )
    } else if error.is_validation() {
        ServerError::invalid_params(error.sanitized_validation_message())
    } else {
        ServerError::internal("agent-definition service unavailable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::Request as HttpRequest,
    };
    use futures_util::Stream;
    use waltr_agent::{
        AgentSession, AgentSessionActiveRunState, AgentSessionRunError, AgentSessionRunSubmission,
        LlmError, SessionListUpdate, SessionStatus, Suspension,
    };
    use std::{io, path::PathBuf, pin::Pin};
    use tower::ServiceExt as _;

    struct UnusedSessionHost;

    #[tonic::async_trait]
    impl SessionHost for UnusedSessionHost {
        async fn create(&self, _: Option<String>) -> Result<AgentSession, Status> {
            unreachable!()
        }

        async fn list(&self) -> Result<Vec<SessionSummary>, LlmError> {
            unreachable!()
        }

        async fn load(&self, _: &SessionId) -> Result<AgentSession, LlmError> {
            unreachable!()
        }

        async fn wipe(&self) -> Result<(), LlmError> {
            unreachable!()
        }

        async fn interrupt(&self, _: &SessionId) -> Result<AgentSession, LlmError> {
            unreachable!()
        }

        async fn active_state(&self, _: &SessionId) -> Option<AgentSessionActiveRunState> {
            unreachable!()
        }

        async fn submit_prompt(
            &self,
            _: SessionId,
            _: String,
        ) -> Result<AgentSessionRunSubmission, AgentSessionRunError> {
            unreachable!()
        }

        fn watch_sessions(&self) -> Pin<Box<dyn Stream<Item = SessionListUpdate> + Send>> {
            Box::pin(tokio_stream::empty())
        }
    }

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

    #[test]
    fn hosted_session_projection_includes_complete_metadata() {
        let mut record = waltr_agent::SessionRecord::new("session-metadata");
        record.set_metadata_value("title", json!("Projected title"));
        record.set_metadata_value(
            "unknown",
            json!({ "null": null, "bool": true, "array": [1, "two"] }),
        );

        let summary = project_summary(&record.summary(), None);
        let session = project_session(&record, None);

        assert_eq!(summary["metadata"]["title"], "Projected title");
        assert_eq!(summary["metadata"]["unknown"]["null"], Value::Null);
        assert_eq!(session["metadata"], summary["metadata"]);
    }

    #[tokio::test]
    async fn agent_definition_action_handlers_dispatch_inspect_replace_and_reset() {
        let root = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&root, &[], "coding").unwrap();
        let server = WaltrMcpServer::new(Arc::new(UnusedSessionHost), state);

        let inspected = server
            .inspect_resources(InspectResourcesInput {
                path: Some("agents/coding.yaml".to_owned()),
            })
            .unwrap();
        let expected_etag = inspected["persisted_etag"].as_str().unwrap().to_owned();
        let source = "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: coding\nspec:\n  description: managed\n";
        let replaced = server
            .replace_resource(ReplaceResourceInput {
                path: "agents/coding.yaml".to_owned(),
                source: source.to_owned(),
                expected_etag,
            })
            .await
            .unwrap();
        assert_eq!(replaced["persisted_source"], source);

        let reset = server
            .reset_resource(ResetResourceInput {
                path: "agents/coding.yaml".to_owned(),
                expected_etag: replaced["persisted_etag"].as_str().unwrap().to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(reset["origin"], "bundled");
        assert_eq!(reset["persisted"], false);
    }

    #[tokio::test]
    async fn public_mcp_publishes_agent_definition_schema_help_and_calls() {
        let router = mcp::server::streamable_http_router(test_server());

        let tools = mcp_request(
            &router,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        )
        .await;
        let exec = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "exec")
            .unwrap();
        let schema = exec["inputSchema"].to_string();
        assert!(schema.contains("resources"));
        assert!(!schema.contains("configuration.inspect"));

        let help = call_exec(&router, json!({ "action": "help" })).await;
        assert!(help.to_string().contains("resources"));

        let definitions = call_exec(
            &router,
            json!({ "action": "resources.inspect", "input": {} }),
        )
        .await;
        assert!(definitions.get("error").is_none(), "{definitions}");
        assert!(definitions.to_string().contains("agents/coding.yaml"));

        let sessions = mcp_request(
            &router,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "sessions", "arguments": { "action": "help" } }
            }),
        )
        .await;
        assert!(sessions["result"].is_object());
    }

    #[tokio::test]
    async fn agent_definition_actions_classify_and_sanitize_failures() {
        let root = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&root, &[], "coding").unwrap();
        let inner = Arc::new(WaltrMcpServer::new(Arc::new(UnusedSessionHost), state));

        let validation = inner
            .inspect_resources(InspectResourcesInput {
                path: Some("/private/tmp/invalid.yml".to_owned()),
            })
            .unwrap_err();
        assert_server_error(&validation, -32602, "invalid canonical", &["/private/tmp"]);

        let inspected = inner
            .inspect_resources(InspectResourcesInput {
                path: Some("agents/coding.yaml".to_owned()),
            })
            .unwrap();
        let stale_error = inner
            .replace_resource(ReplaceResourceInput {
                path: "agents/coding.yaml".to_owned(),
                source: "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: coding\nspec: {}\n".to_owned(),
                expected_etag: format!("{}-stale", inspected["persisted_etag"].as_str().unwrap()),
            })
            .await
            .unwrap_err();
        assert_server_error(&stale_error, -32602, "failed precondition:", &["sha256:"]);

        let read = agent_definition_error(&AgentDefinitionLoadError::Read {
            path: PathBuf::from("/private/tmp/secret.yaml"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "permission denied detail"),
        });
        assert_server_error(
            &read,
            -32603,
            "agent-definition service unavailable",
            &["/private/tmp", "permission denied"],
        );
        let write = agent_definition_error(&AgentDefinitionLoadError::Write {
            path: PathBuf::from("/private/tmp/secret.yaml"),
            source: io::Error::other("disk detail"),
        });
        assert_server_error(
            &write,
            -32603,
            "service unavailable",
            &["secret", "disk detail"],
        );
        let internal = agent_definition_error(&AgentDefinitionLoadError::Watch(
            notify::Error::generic("watcher detail"),
        ));
        assert_server_error(
            &internal,
            -32603,
            "service unavailable",
            &["watcher detail"],
        );
    }

    fn test_server() -> Arc<WaltrMcpServer> {
        let root = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&root, &[], "coding").unwrap();
        Arc::new(WaltrMcpServer::new(Arc::new(UnusedSessionHost), state))
    }

    async fn call_exec(router: &Router, arguments: Value) -> Value {
        mcp_request(
            router,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "exec", "arguments": arguments }
            }),
        )
        .await
    }

    async fn mcp_request(router: &Router, mut body: Value) -> Value {
        body["params"] = body.get("params").cloned().unwrap_or_else(|| json!({}));
        body["params"]["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": mcp::protocol::MCP_PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/clientInfo": { "name": "waltr-server-tests", "version": "1" }
        });
        let method = body["method"].as_str().unwrap();
        let mut request = HttpRequest::post("/mcp")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
            .header("mcp-method", method);
        if method == "tools/call" {
            request = request.header("mcp-name", body["params"]["name"].as_str().unwrap());
        }
        let response = router
            .clone()
            .oneshot(
                request
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let json = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or(text.as_str());
        serde_json::from_str(json).unwrap()
    }

    fn assert_server_error(error: &ServerError, code: i64, message: &str, absent: &[&str]) {
        let debug = format!("{error:?}");
        assert!(debug.contains(&format!("code: {code}")), "{debug}");
        assert!(debug.contains(message), "{debug}");
        for value in absent {
            assert!(!debug.contains(value), "{debug}");
        }
    }
}
