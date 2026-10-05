//! Waltr Model Context Protocol server surface.

use crate::{
    agent_definition::{AgentDefinitionLoadError, AgentDefinitionState},
    session::SessionHost,
};
use futures_util::StreamExt as _;
use jaq_interpret::{Ctx, FilterT as _, ParseCtx, RcIter, Val};
use waltr_agent::{
    AgentSessionRunDisposition, AuthTarget, ExternalRequest, ExternalRequestKind,
    ExternalRequestStatus, InferenceSettings, PendingWork, PendingWorkStatus, SessionCapabilities,
    SessionCoordinationPhase, SessionCoordinationState, SessionId, SessionProgress,
    SessionProgressPhase, SessionPromptDisposition, SessionRunStage, SessionStatus, SessionSummary,
    Suspension,
};
use mcp::server::{ServerError, ServerResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tonic::{Code, Status};

#[path = "mcp_service/apps/mod.rs"]
mod served_apps;
use served_apps::status as status_app;

const DEFAULT_WAIT_SECONDS: u64 = 60;
const MAX_WAIT_SECONDS: u64 = 300;
const WAIT_RECONCILE_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ResourceCreateInput {
    /// Canonical metadata URI, for example waltr://resources/agents%2Fcoding.yaml.
    uri: String,
    /// Complete strict YAML for an absent persisted slot; never overwrites a persisted file.
    source: String,
    /// Exact guarded revision from the metadata resource, including the effective hash for absent slots.
    expected_revision: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ResourceDestroyInput {
    /// Canonical metadata URI of the persisted override to remove.
    uri: String,
    /// Exact revision from metadata. Removal restores bundled fallback if present.
    expected_revision: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TextEditInput {
    /// Canonical metadata URI of the effective YAML to edit (not the /source URI).
    uri: String,
    /// Exact guarded revision from metadata. A bundled edit creates a persisted override.
    expected_revision: String,
    /// Ordered edits, all validated before one atomic replacement against the original revision.
    #[schemars(length(min = 1, max = 256))]
    edits: Vec<TextEdit>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum TextEdit {
    Replace {
        /// Nonempty text matching exactly once, including overlapping matches.
        old_text: String,
        /// Replacement text, which must differ from old_text.
        new_text: String,
    },
    Insert {
        /// Nonempty inserted text.
        text: String,
        /// Insert at start/end or immediately before/after a unique anchor.
        placement: Placement,
        /// Required and nonempty for before/after; forbidden for start/end.
        #[serde(default, deserialize_with = "deserialize_present_anchor")]
        anchor: Option<String>,
    },
}

fn deserialize_present_anchor<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

impl JsonSchema for TextEdit {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TextEdit".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        json!({
            "oneOf": [
                {
                    "type": "object", "additionalProperties": false,
                    "required": ["operation", "old_text", "new_text"],
                    "properties": {
                        "operation": {"const": "replace"},
                        "old_text": {"type": "string", "minLength": 1},
                        "new_text": {"type": "string"}
                    }
                },
                {
                    "type": "object", "additionalProperties": false,
                    "required": ["operation", "text", "placement"],
                    "properties": {
                        "operation": {"const": "insert"},
                        "text": {"type": "string", "minLength": 1},
                        "placement": {"enum": ["start", "end"]}
                    }
                },
                {
                    "type": "object", "additionalProperties": false,
                    "required": ["operation", "text", "placement", "anchor"],
                    "properties": {
                        "operation": {"const": "insert"},
                        "text": {"type": "string", "minLength": 1},
                        "placement": {"enum": ["before", "after"]},
                        "anchor": {"type": "string", "minLength": 1}
                    }
                }
            ]
        })
        .try_into()
        .expect("text edit schema is an object")
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Placement {
    Start,
    End,
    Before,
    After,
}

fn unique_anchor(source: &str, anchor: &str) -> ServerResult<usize> {
    if anchor.is_empty() {
        return Err(ServerError::invalid_params("anchor must not be empty"));
    }
    let mut positions = source
        .char_indices()
        .filter_map(|(position, _)| source[position..].starts_with(anchor).then_some(position));
    let position = positions
        .next()
        .ok_or_else(|| ServerError::invalid_params("anchor not found"))?;
    if positions.next().is_some() {
        return Err(ServerError::invalid_params("anchor is ambiguous"));
    }
    Ok(position)
}

fn apply_text_edits(source: &str, edits: &[TextEdit]) -> ServerResult<String> {
    if edits.is_empty() || edits.len() > 256 {
        return Err(ServerError::invalid_params(
            "edits must contain between 1 and 256 entries",
        ));
    }
    let mut output = source.to_owned();
    for edit in edits {
        match edit {
            TextEdit::Replace { old_text, new_text } => {
                let position = unique_anchor(&output, old_text)?;
                if old_text == new_text {
                    return Err(ServerError::invalid_params("edit must change the source"));
                }
                output.replace_range(position..position + old_text.len(), new_text);
            }
            TextEdit::Insert {
                text,
                placement,
                anchor,
            } => {
                if text.is_empty() {
                    return Err(ServerError::invalid_params("insert text must not be empty"));
                }
                let position = match placement {
                    Placement::Start | Placement::End => {
                        if anchor.is_some() {
                            return Err(ServerError::invalid_params(
                                "start/end must not have an anchor",
                            ));
                        }
                        if matches!(placement, Placement::Start) {
                            0
                        } else {
                            output.len()
                        }
                    }
                    Placement::Before | Placement::After => {
                        let anchor = anchor.as_deref().ok_or_else(|| {
                            ServerError::invalid_params("before/after require an anchor")
                        })?;
                        let position = unique_anchor(&output, anchor)?;
                        position
                            + if matches!(placement, Placement::After) {
                                anchor.len()
                            } else {
                                0
                            }
                    }
                };
                output.insert_str(position, text);
            }
        }
    }
    if output == source {
        return Err(ServerError::invalid_params("edits must change the source"));
    }
    Ok(output)
}

fn encode_component(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            output.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(output, "%{byte:02X}").expect("writing to string");
        }
    }
    output
}

fn decode_component(value: &str) -> ServerResult<String> {
    let invalid = || ServerError::invalid_params("invalid canonical Waltr URI");
    let mut bytes = Vec::new();
    let mut input = value.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let high = input
                .next()
                .and_then(|b| char::from(b).to_digit(16))
                .ok_or_else(invalid)?;
            let low = input
                .next()
                .and_then(|b| char::from(b).to_digit(16))
                .ok_or_else(invalid)?;
            bytes.push(u8::try_from(high * 16 + low).map_err(|_| invalid())?);
        } else {
            bytes.push(byte);
        }
    }
    let decoded = String::from_utf8(bytes).map_err(|_| invalid())?;
    if decoded.is_empty()
        || decoded.chars().any(char::is_control)
        || encode_component(&decoded) != value
    {
        return Err(invalid());
    }
    Ok(decoded)
}

fn resource_path(uri: &str) -> ServerResult<String> {
    let component = uri
        .strip_prefix("waltr://resources/")
        .ok_or_else(|| ServerError::invalid_params("expected waltr://resources/{path} URI"))?;
    decode_component(component)
}

fn resource_revision(detail: &Value) -> String {
    let persisted = detail["persisted_etag"].as_str().unwrap_or_default();
    if detail["persisted"] == false {
        format!(
            "{persisted}@{}",
            detail["effective_etag"].as_str().unwrap_or("absent")
        )
    } else {
        persisted.to_owned()
    }
}

/// MCP server backed by Waltr's registered configuration and session services.
#[derive(Clone)]
pub struct WaltrMcpServer {
    sessions: Arc<dyn SessionHost>,
    resources: AgentDefinitionState,
    catalog: Arc<mcp::SkillCatalog>,
    readiness: Option<crate::readiness::Readiness>,
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
    /// jq expression evaluated against the same projection as waltr://sessions/{id}.
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

impl WaltrMcpServer {
    /// Create a Waltr MCP server backed by the same services registered with gRPC.
    /// Returns an error if the embedded skill catalog fails validation.
    pub fn new(
        sessions: Arc<dyn SessionHost>,
        resources: AgentDefinitionState,
    ) -> mcp::Result<Self> {
        let catalog = mcp::SkillCatalog::new()
            .with_skill(
                "resource-administration",
                &[
                    (
                        "SKILL.md",
                        include_bytes!("../resources/skills/resource-administration/SKILL.md"),
                    ),
                    (
                        "references/resources.md",
                        include_bytes!(
                            "../resources/skills/resource-administration/references/resources.md"
                        ),
                    ),
                ],
            )?
            .with_skill(
                "hosted-sessions",
                &[
                    (
                        "SKILL.md",
                        include_bytes!("../resources/skills/hosted-sessions/SKILL.md"),
                    ),
                    (
                        "references/sessions.md",
                        include_bytes!(
                            "../resources/skills/hosted-sessions/references/sessions.md"
                        ),
                    ),
                ],
            )?;
        Ok(Self {
            sessions,
            resources,
            catalog: Arc::new(catalog),
            readiness: None,
        })
    }

    pub(crate) fn with_readiness(mut self, readiness: crate::readiness::Readiness) -> Self {
        self.readiness = Some(readiness);
        self
    }

    const fn resources(&self) -> &AgentDefinitionState {
        &self.resources
    }

    async fn session_projection(&self, id: &SessionId) -> ServerResult<Value> {
        let session = self.sessions.load(id).await.map_err(session_error)?;
        let summary = self
            .sessions
            .list()
            .await
            .map_err(session_error)?
            .into_iter()
            .find(|summary| &summary.id == id)
            .unwrap_or_else(|| session.record.summary());
        let active = self.sessions.active_state(id).await;
        Ok(project_session(&session.record, &summary, active))
    }
}

#[allow(missing_docs)]
impl WaltrMcpServer {
    /// Inspect all active resources or one canonical persisted slot.
    fn inspect_resources(&self, input: InspectResourcesInput) -> ServerResult<Value> {
        let path = input.path;
        let resources = self
            .resources()
            .inspect(path.as_deref())
            .map_err(|error| agent_definition_error(&error))?;
        response_to_value(resources)
    }

    /// List durable hosted sessions, newest activity first.
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
            "uri": format!("waltr://sessions/{}", encode_component(submission.session_id.as_str())),
            "agent_name": agent_name,
            "run_id": submission.run_id,
            "disposition": disposition,
            "queue_position": queue_position
        }))
    }

    /// Get one durable session using the stable MCP session projection.
    async fn get_session(&self, input: SessionIdInput) -> ServerResult<Value> {
        self.session_projection(&required_session_id(input.id)?)
            .await
    }

    /// Blindly delete every hosted agent session. Operators must ensure sessions are idle.
    async fn wipe_sessions(&self, input: WipeInput) -> ServerResult<Value> {
        if !input.confirm {
            return Err(ServerError::invalid_params("confirm must be true"));
        }
        self.sessions.wipe().await.map_err(session_error)?;
        response_to_value(WipeResult { wiped: true })
    }

    /// Idempotently interrupt a hosted session and cancel its queued runs.
    async fn interrupt_session(&self, input: SessionIdInput) -> ServerResult<Value> {
        let id = required_session_id(input.id)?;
        self.sessions.interrupt(&id).await.map_err(session_error)?;
        self.session_projection(&id).await
    }

    /// Submit a prompt, queueing it when the session already has an active run.
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
            "uri": format!("waltr://sessions/{}", encode_component(submission.session_id.as_str())),
            "run_id": submission.run_id,
            "disposition": disposition,
            "queue_position": queue_position
        }))
    }

    /// Wait until a jq condition matches, the session stops running, or time expires.
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
            if !projection_running(&projection) {
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
                    } else if !projection_running(&projection) {
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

impl WaltrMcpServer {
    fn resource_detail(&self, uri: &str) -> ServerResult<Value> {
        let path = resource_path(uri)?;
        let mut detail = self.inspect_resources(InspectResourcesInput { path: Some(path) })?;
        detail["uri"] = json!(uri);
        detail["source_uri"] = json!(format!("{uri}/source"));
        detail["revision"] = json!(resource_revision(&detail));
        Ok(detail)
    }

    async fn create_resource(&self, input: ResourceCreateInput) -> ServerResult<Value> {
        let detail = self.resource_detail(&input.uri)?;
        if detail["persisted"] != false || resource_revision(&detail) != input.expected_revision {
            return Err(ServerError::invalid_params(
                "failed precondition: slot occupied or revision changed; read and retry",
            ));
        }
        self.resources
            .replace_guarded(
                &resource_path(&input.uri)?,
                input.source,
                detail["persisted_etag"].as_str().unwrap_or_default(),
                detail["source"].as_str(),
            )
            .await
            .map_err(|error| agent_definition_error(&error))?;
        self.resource_detail(&input.uri)
    }

    async fn edit_resource(&self, input: TextEditInput) -> ServerResult<Value> {
        let detail = self.resource_detail(&input.uri)?;
        if resource_revision(&detail) != input.expected_revision {
            return Err(ServerError::invalid_params(
                "failed precondition: revision changed; read and retry",
            ));
        }
        let source = detail["source"]
            .as_str()
            .ok_or_else(|| ServerError::invalid_params("resource has no source; use create"))?;
        let output = apply_text_edits(source, &input.edits)?;
        self.resources
            .replace_guarded(
                &resource_path(&input.uri)?,
                output,
                detail["persisted_etag"].as_str().unwrap_or_default(),
                Some(source),
            )
            .await
            .map_err(|error| agent_definition_error(&error))?;
        self.resource_detail(&input.uri)
    }

    async fn destroy_resource(&self, input: ResourceDestroyInput) -> ServerResult<Value> {
        let detail = self.resource_detail(&input.uri)?;
        if detail["persisted"] != true || resource_revision(&detail) != input.expected_revision {
            return Err(ServerError::invalid_params(
                "failed precondition: no persisted override or revision changed; read and retry",
            ));
        }
        self.resources
            .reset(&resource_path(&input.uri)?, &input.expected_revision)
            .await
            .map_err(|error| agent_definition_error(&error))?;
        self.resource_detail(&input.uri)
    }
}

impl mcp::server::ServerHandler for WaltrMcpServer {
    fn server_info(&self) -> mcp::server::ServerInfo {
        mcp::server::ServerInfo::new("waltr", env!("CARGO_PKG_VERSION"))
    }

    fn capabilities(&self) -> mcp::server::ServerCapabilities {
        let mut capabilities = mcp::server::ServerCapabilities::new()
            .tools()
            .resources()
            .skills();
        served_apps::advertise(&mut capabilities);
        capabilities
    }

    fn skill_catalog(&self) -> Option<Arc<mcp::SkillCatalog>> {
        Some(self.catalog.clone())
    }

    fn list_tools(
        &self,
        _cursor: Option<String>,
        context: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<ServerResult<mcp::McpToolList>> {
        use mcp::progressive::{ActionInputSchema, input_schema, json_schema_for, tool_definition};
        macro_rules! tool {
            ($name:literal, $description:literal, $( $action:literal => $ty:ty ),+ ) => {
                tool_definition($name, $description, input_schema(&[$($action),+], vec![$(ActionInputSchema { action: $action, schema: Some(json_schema_for::<$ty>()) }),+]))
            };
        }
        Box::pin(async move {
            Ok(mcp::McpToolList {
                tools: vec![
                    status_app::tool(&context),
                    tool!("create", "Create a session or an absent persisted resource slot (including a bundled override).", "session.create" => CreateInput, "resource.create" => ResourceCreateInput),
                    tool_definition(
                        "edit",
                        "Apply ordered atomic text edits to effective YAML guarded by its revision.",
                        json_schema_for::<TextEditInput>(),
                    ),
                    tool!("destroy", "Remove a persisted override, restoring bundled fallback if present, or explicitly wipe ALL sessions.", "resource.destroy" => ResourceDestroyInput, "session.wipe" => WipeInput),
                    tool!("execute", "Control hosted sessions.", "session.send" => SendInput, "session.interrupt" => SessionIdInput),
                    tool!("query", "Wait for a bounded jq condition on a hosted session.", "session.wait" => WaitInput),
                ],
                next_cursor: None,
            })
        })
    }

    fn call_tool(
        &self,
        call: mcp::McpToolCall,
        _context: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<ServerResult<mcp::McpToolResult>> {
        let server = self.clone();
        Box::pin(async move {
            if call.name == "status" {
                if !call.arguments.is_object()
                    || call
                        .arguments
                        .as_object()
                        .is_some_and(|args| !args.is_empty())
                {
                    return Err(ServerError::invalid_params("status takes an empty object"));
                }
                let ready = match &server.readiness {
                    Some(readiness) => readiness.is_ready().await,
                    None => false,
                };
                let observed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                return Ok(mcp::McpToolResult::new(json!({
                    "content": [{"type":"text", "text": if ready { "Waltr is ready" } else { "Waltr is not ready" }}],
                    "structuredContent": {"ready":ready,"observedAtUnixMs":observed}
                })));
            }
            if call.name == "edit" {
                let input =
                    serde_json::from_value::<TextEditInput>(call.arguments).map_err(|error| {
                        ServerError::invalid_params(format!("invalid edit arguments: {error}"))
                    })?;
                let output = server.edit_resource(input).await?;
                return mcp::progressive::tool_result(output, None);
            }
            let request = mcp::progressive::parse_arguments(&call.name, call.arguments)?;
            macro_rules! input {
                () => {
                    mcp::progressive::deserialize_action_input(&request.action, request.input)?
                };
            }
            let output = match (call.name.as_str(), request.action.as_str()) {
                ("create", "session.create") => server.create_session(input!()).await?,
                ("create", "resource.create") => server.create_resource(input!()).await?,
                ("destroy", "resource.destroy") => server.destroy_resource(input!()).await?,
                ("destroy", "session.wipe") => server.wipe_sessions(input!()).await?,
                ("execute", "session.send") => server.send_session(input!()).await?,
                ("execute", "session.interrupt") => server.interrupt_session(input!()).await?,
                ("query", "session.wait") => server.wait_session(input!()).await?,
                _ => return Err(mcp::progressive::unknown_action(&request.action)),
            };
            mcp::progressive::tool_result(output, request.filter.as_deref())
        })
    }

    fn list_resources(
        &self,
        _cursor: Option<String>,
        context: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<ServerResult<mcp::McpResourceList>> {
        Box::pin(async move {
            let mut list = mcp::McpResourceList {
                resources: vec![
                    mcp::progressive::action_resource_definition(
                        "waltr://resources",
                        "resources",
                        "Resource catalog with canonical item and source URIs.",
                    ),
                    mcp::progressive::action_resource_definition(
                        "waltr://sessions",
                        "sessions",
                        "Durable hosted sessions with canonical item URIs.",
                    ),
                ],
                next_cursor: None,
            };
            list.resources.extend(
                served_apps::RESOURCES
                    .iter()
                    .filter(|app| app.supported(&context))
                    .map(|app| app.descriptor()),
            );
            Ok(list)
        })
    }

    fn list_resource_templates(
        &self,
        _cursor: Option<String>,
        _context: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<ServerResult<mcp::McpResourceTemplateList>> {
        Box::pin(async {
            let mut source = mcp::progressive::action_resource_template_definition(
                "waltr://resources/{path}/source",
                "resource.source",
                "Exact effective YAML. Path is one canonically percent-encoded component.",
            );
            source.mime_type = Some("application/yaml".to_owned());
            Ok(mcp::McpResourceTemplateList {
                resource_templates: vec![
                    mcp::progressive::action_resource_template_definition(
                        "waltr://resources/{path}",
                        "resource",
                        "Slot metadata, persisted/effective/active sources and guarded revision.",
                    ),
                    source,
                    mcp::progressive::action_resource_template_definition(
                        "waltr://sessions/{id}",
                        "session",
                        "Stable hosted session projection. ID is one canonically percent-encoded component.",
                    ),
                ],
                next_cursor: None,
            })
        })
    }

    fn read_resource(
        &self,
        uri: String,
        context: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<ServerResult<mcp::McpResourceResult>> {
        let server = self.clone();
        Box::pin(async move {
            if let Some(app) = served_apps::RESOURCES
                .iter()
                .find(|app| app.uri == uri && app.supported(&context))
            {
                return Ok(app.resource());
            }
            if let Some(result) = server.catalog.read(&uri) {
                return Ok(result);
            }
            let value = if uri == "waltr://resources" {
                let mut catalog = server.inspect_resources(InspectResourcesInput { path: None })?;
                if let Some(entries) = catalog["entries"].as_array_mut() {
                    for entry in entries {
                        let item = format!(
                            "waltr://resources/{}",
                            encode_component(entry["path"].as_str().unwrap_or_default())
                        );
                        entry["uri"] = json!(item);
                        entry["source_uri"] = json!(format!("{item}/source"));
                    }
                }
                catalog
            } else if uri == "waltr://sessions" {
                server.list_sessions().await?
            } else if let Some(id) = uri.strip_prefix("waltr://sessions/") {
                server
                    .get_session(SessionIdInput {
                        id: decode_component(id)?,
                    })
                    .await?
            } else if uri.starts_with("waltr://resources/") {
                if let Some(item) = uri.strip_suffix("/source") {
                    let detail = server.resource_detail(item)?;
                    let text = detail["source"]
                        .as_str()
                        .ok_or_else(|| ServerError::resource_not_found(uri.clone()))?;
                    return Ok(mcp::McpResourceResult::text(uri, "application/yaml", text));
                }
                server.resource_detail(&uri)?
            } else {
                return Err(ServerError::resource_not_found(uri));
            };
            Ok(mcp::progressive::json_resource_result(uri, value))
        })
    }
}

fn projection_running(projection: &Value) -> bool {
    projection["running"].as_bool().unwrap_or(false)
}

fn project_session(
    record: &waltr_agent::SessionRecord,
    summary: &SessionSummary,
    active: Option<waltr_agent::AgentSessionActiveRunState>,
) -> Value {
    let snapshot = record.snapshot();
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
    summary: &SessionSummary,
    active: Option<waltr_agent::AgentSessionActiveRunState>,
) -> Value {
    let (status, suspension, failure, retry) = project_status(&summary.status);
    json!({
        "id": summary.id.as_str(),
        "uri": format!("waltr://sessions/{}", encode_component(summary.id.as_str())),
        "revision": summary.revision.get(),
        "status": status,
        "running": summary.progress.running,
        "suspension": suspension,
        "failure": failure,
        "retry": retry,
        "coordination": project_coordination(&summary.coordination),
        "progress": project_progress(&summary.progress),
        "capabilities": project_capabilities(&summary.capabilities),
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

fn project_coordination(coordination: &SessionCoordinationState) -> Value {
    let phase = match coordination.phase {
        SessionCoordinationPhase::Ready => "ready",
        SessionCoordinationPhase::Running => "running",
        SessionCoordinationPhase::CompletionBarrier => "completion_barrier",
        SessionCoordinationPhase::Paused => "paused",
    };
    json!({
        "phase": phase,
        "reason": coordination.reason.as_ref().map(|reason| json!({
            "code": reason.code(),
            "source": reason.source(),
            "trigger": reason.trigger(),
        })),
        "queued_count": coordination.queued_count,
        "continuation_armed": coordination.continuation_armed,
    })
}

fn project_progress(progress: &SessionProgress) -> Value {
    let phase = match progress.phase {
        SessionProgressPhase::Unknown => "unknown",
        SessionProgressPhase::Idle => "idle",
        SessionProgressPhase::Running => "running",
        SessionProgressPhase::CompletionBarrier => "completion_barrier",
        SessionProgressPhase::Paused => "paused",
        SessionProgressPhase::AwaitingToolResults => "awaiting_tool_results",
        SessionProgressPhase::AwaitingExternalInput => "awaiting_external_input",
        SessionProgressPhase::Interrupted => "interrupted",
        SessionProgressPhase::Finished => "finished",
        SessionProgressPhase::Failed => "failed",
    };
    let run_stage = progress.run_stage.map(|stage| match stage {
        SessionRunStage::BuildingRequest => "building_request",
        SessionRunStage::StreamingProvider => "streaming_provider",
        SessionRunStage::RetryingProvider => "retrying_provider",
        SessionRunStage::EvaluatingToolCall => "evaluating_tool_call",
        SessionRunStage::RunningTool => "running_tool",
    });
    json!({
        "phase": phase,
        "run_stage": run_stage,
        "reason": progress.reason.as_ref().map(|reason| json!({
            "code": reason.code(),
            "source": reason.source(),
            "trigger": reason.trigger(),
        })),
        "queued_count": progress.queued_count,
        "continuation_armed": progress.continuation_armed,
        "running": progress.running,
    })
}

fn project_capabilities(capabilities: &SessionCapabilities) -> Value {
    let prompt_disposition = match capabilities.prompt_disposition {
        SessionPromptDisposition::Immediate => "immediate",
        SessionPromptDisposition::Queued => "queued",
        SessionPromptDisposition::SupersedeSuspension => "supersede_suspension",
    };
    json!({
        "prompt_disposition": prompt_disposition,
        "can_respond_external_request": capabilities.can_respond_external_request,
        "can_interrupt": capabilities.can_interrupt,
    })
}

fn project_status(status: &SessionStatus) -> (&'static str, Value, Value, Value) {
    match status {
        SessionStatus::Idle => ("idle", Value::Null, Value::Null, Value::Null),
        SessionStatus::BuildingRequest => {
            ("building_request", Value::Null, Value::Null, Value::Null)
        }
        SessionStatus::StreamingProvider => {
            ("streaming_provider", Value::Null, Value::Null, Value::Null)
        }
        SessionStatus::RetryingProvider {
            attempt,
            max_attempts,
            delay_ms,
            message,
        } => (
            "retrying_provider",
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
            Value::Null,
            Value::Null,
            Value::Null,
        ),
        SessionStatus::RunningTool => ("running_tool", Value::Null, Value::Null, Value::Null),
        SessionStatus::Suspended(suspension) => (
            "suspended",
            project_suspension(suspension),
            Value::Null,
            Value::Null,
        ),
        SessionStatus::Finished => ("finished", Value::Null, Value::Null, Value::Null),
        SessionStatus::Failed {
            message,
            authentication_failure,
        } => (
            "failed",
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
        LlmError, SessionListUpdate,
    };
    use std::{io, path::PathBuf, pin::Pin};
    use tower::ServiceExt as _;

    struct UnusedSessionHost;

    #[tonic::async_trait]
    impl SessionHost for UnusedSessionHost {
        async fn shutdown(&self) {}

        fn set_completion_admission_handler(
            &self,
            _: Option<Arc<dyn waltr_agent::SessionCompletionCoordinationHandler>>,
        ) {
        }

        async fn create(&self, _: Option<String>) -> Result<AgentSession, Status> {
            unreachable!()
        }

        async fn create_exact(
            &self,
            _: waltr_agent::SessionCreateRequest,
        ) -> Result<AgentSession, LlmError> {
            Err(LlmError::api(
                "unused-session-host",
                "exact session creation is unsupported",
            ))
        }

        async fn capture_latest_completed(
            &self,
            _: &SessionId,
        ) -> Result<waltr_agent::CompletedSessionCapture, waltr_agent::SessionCaptureError> {
            Err(waltr_agent::SessionCaptureError::FailedPrecondition(
                "session capture is unsupported".to_owned(),
            ))
        }

        async fn acquire_pause_reject_busy(
            &self,
            id: &SessionId,
            _: waltr_agent::SessionCoordinationReason,
        ) -> Result<waltr_agent::SessionPauseLease, waltr_agent::SessionPauseError> {
            Err(waltr_agent::SessionPauseError::Unavailable {
                session_id: id.clone(),
            })
        }

        async fn capture_paused_effective(
            &self,
            lease: &waltr_agent::SessionPauseLease,
        ) -> Result<waltr_agent::CompletedSessionCapture, waltr_agent::SessionPauseError> {
            Err(waltr_agent::SessionPauseError::Unavailable {
                session_id: lease.session_id().clone(),
            })
        }

        async fn apply_paused_mask_mutation(
            &self,
            lease: &waltr_agent::SessionPauseLease,
            _: waltr_agent::SessionMaskMutation,
        ) -> Result<waltr_agent::SessionMaskMutationOutcome, waltr_agent::SessionPauseError> {
            Err(waltr_agent::SessionPauseError::Unavailable {
                session_id: lease.session_id().clone(),
            })
        }

        async fn arm_paused_continuation(
            &self,
            lease: &waltr_agent::SessionPauseLease,
            _: waltr_agent::TimelineCursor,
            _: waltr_agent::AgentRunLimits,
        ) -> Result<AgentSessionRunSubmission, waltr_agent::SessionPauseError> {
            Err(waltr_agent::SessionPauseError::Unavailable {
                session_id: lease.session_id().clone(),
            })
        }

        async fn release_pause(
            &self,
            lease: &waltr_agent::SessionPauseLease,
        ) -> Result<(), waltr_agent::SessionPauseError> {
            Err(waltr_agent::SessionPauseError::Unavailable {
                session_id: lease.session_id().clone(),
            })
        }

        async fn start_durable_prompt(
            &self,
            id: SessionId,
            _: waltr_agent::AgentRunLimits,
        ) -> Result<AgentSessionRunSubmission, AgentSessionRunError> {
            Err(AgentSessionRunError::Unavailable { session_id: id })
        }

        async fn list(&self) -> Result<Vec<SessionSummary>, LlmError> {
            Ok(vec![
                waltr_agent::SessionRecord::new("session /?#%").summary(),
            ])
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
            None
        }

        async fn submit_prompt(
            &self,
            _: SessionId,
            _: String,
        ) -> Result<AgentSessionRunSubmission, AgentSessionRunError> {
            unreachable!()
        }

        async fn set_metadata_value_conditionally(
            &self,
            _: &SessionId,
            _: String,
            _: serde_json::Value,
            _: waltr_agent::MetadataSetCondition,
        ) -> Result<waltr_agent::MetadataSetOutcome, LlmError> {
            Err(LlmError::api(
                "unused-session-host",
                "metadata mutation is unsupported",
            ))
        }

        fn watch_sessions(&self) -> Pin<Box<dyn Stream<Item = SessionListUpdate> + Send>> {
            Box::pin(tokio_stream::empty())
        }
    }

    #[test]
    fn canonical_uri_components_round_trip_and_reject_aliases() {
        for input in ["agents/coding.yaml", "session /?#%", "unicode-\u{00e9}"] {
            assert_eq!(decode_component(&encode_component(input)).unwrap(), input);
        }
        for invalid in [
            "",
            "agents/coding.yaml",
            "%2f",
            "%61",
            "%",
            "%GG",
            "%FF",
            "a?b",
            "a#b",
            "%00",
        ] {
            assert!(decode_component(invalid).is_err(), "{invalid}");
        }
        assert!(resource_path("https://resources/agents%2Fcoding.yaml").is_err());
    }

    #[test]
    fn text_edits_are_ordered_unique_and_nonempty() {
        let edits: Vec<TextEdit> = serde_json::from_value(json!([
            {"operation":"replace","old_text":"alpha","new_text":"beta"},
            {"operation":"insert","text":"!","placement":"after","anchor":"beta"},
            {"operation":"insert","text":"[","placement":"start"},
            {"operation":"insert","text":"]","placement":"end"}
        ]))
        .unwrap();
        assert_eq!(apply_text_edits("alpha", &edits).unwrap(), "[beta!]");
        for edit in [
            json!({"operation":"replace","old_text":"","new_text":"x"}),
            json!({"operation":"replace","old_text":"a","new_text":"b"}),
            json!({"operation":"replace","old_text":"aa","new_text":"b"}),
            json!({"operation":"replace","old_text":"aaa","new_text":"aaa"}),
            json!({"operation":"insert","text":"","placement":"start"}),
            json!({"operation":"insert","text":"x","placement":"start","anchor":"aaa"}),
            json!({"operation":"insert","text":"x","placement":"before"}),
            json!({"operation":"insert","text":"x","placement":"after","anchor":"a"}),
        ] {
            let edit: TextEdit = serde_json::from_value(edit).unwrap();
            assert!(apply_text_edits("aaa", &[edit]).is_err());
        }
        assert!(apply_text_edits("aaa", &[]).is_err());
        let excessive: Vec<TextEdit> = (0..257)
            .map(|_| TextEdit::Insert {
                text: "x".to_owned(),
                placement: Placement::End,
                anchor: None,
            })
            .collect();
        assert!(apply_text_edits("aaa", &excessive).is_err());
    }

    #[tokio::test]
    async fn direct_edit_http_contract_rejects_envelopes_and_null_anchors_without_writes() {
        let root = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&root, &[], "coding").unwrap();
        let server = Arc::new(WaltrMcpServer::new(Arc::new(UnusedSessionHost), state).unwrap());
        let router = mcp::server::streamable_http_router(server.clone());
        let uri = "waltr://resources/agents%2Fcoding.yaml";
        let original = server.resource_detail(uri).unwrap();
        let arguments = json!({
            "uri": uri, "expected_revision": original["revision"],
            "edits": [{"operation":"insert", "text":"\n# direct edit\n", "placement":"end"}]
        });
        let tools = mcp_request(
            &router,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await;
        let edit = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "edit")
            .unwrap();
        let schema = &edit["inputSchema"];
        assert_eq!(
            schema["required"],
            json!(["uri", "expected_revision", "edits"])
        );
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["properties"].get("action").is_none());
        assert!(schema["properties"].get("input").is_none());
        assert!(schema["properties"].get("filter").is_none());
        let alternatives = schema["properties"]["edits"]["items"]["oneOf"]
            .as_array()
            .unwrap();
        assert_eq!(alternatives.len(), 3);
        let unanchored = &alternatives[1];
        assert_eq!(
            unanchored["properties"]["placement"]["enum"],
            json!(["start", "end"])
        );
        assert_eq!(unanchored["additionalProperties"], false);
        assert!(unanchored["properties"].get("anchor").is_none());
        let anchored = &alternatives[2];
        assert_eq!(
            anchored["properties"]["placement"]["enum"],
            json!(["before", "after"])
        );
        assert!(
            anchored["required"]
                .as_array()
                .unwrap()
                .contains(&json!("anchor"))
        );
        assert_eq!(anchored["properties"]["anchor"]["type"], "string");
        assert_eq!(anchored["properties"]["anchor"]["minLength"], 1);

        let mut rejected = vec![json!({"action":"resource.edit", "input":arguments})];
        for placement in ["start", "end", "before", "after"] {
            let mut null_anchor = arguments.clone();
            null_anchor["edits"][0]["placement"] = json!(placement);
            null_anchor["edits"][0]["anchor"] = Value::Null;
            rejected.push(null_anchor);
        }
        for placement in ["before", "after"] {
            let mut missing_anchor = arguments.clone();
            missing_anchor["edits"][0]["placement"] = json!(placement);
            rejected.push(missing_anchor);
        }
        for arguments in rejected {
            let response = mcp_request(
                &router,
                json!({
                    "jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"edit", "arguments":arguments}
                }),
            )
            .await;
            assert_eq!(response["error"]["code"], -32602, "{response}");
            assert_eq!(server.resource_detail(uri).unwrap(), original);
            assert!(!root.join(".waltr/resources/agents/coding.yaml").exists());
        }

        let response = mcp_request(
            &router,
            json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"edit", "arguments":arguments}
            }),
        )
        .await;
        assert!(response.get("error").is_none(), "{response}");
        let updated = server.resource_detail(uri).unwrap();
        assert_eq!(updated["persisted"], true);
        assert!(
            updated["source"]
                .as_str()
                .unwrap()
                .ends_with("\n# direct edit\n")
        );
        assert_eq!(response["result"]["structuredContent"], updated);
        assert_eq!(
            std::fs::read_to_string(root.join(".waltr/resources/agents/coding.yaml")).unwrap(),
            updated["source"].as_str().unwrap()
        );
    }

    #[tokio::test]
    async fn edits_guard_bundled_source_revision_and_validate_atomically() {
        let server = test_server();
        let uri = "waltr://resources/agents%2Fcoding.yaml";
        let original = server.resource_detail(uri).unwrap();
        let source = original["source"].as_str().unwrap();
        let revision = original["revision"].as_str().unwrap();
        assert!(revision.contains('@'));
        let invalid = server
            .edit_resource(TextEditInput {
                uri: uri.to_owned(),
                expected_revision: revision.to_owned(),
                edits: vec![TextEdit::Replace {
                    old_text: source.to_owned(),
                    new_text: "not a valid resource".to_owned(),
                }],
            })
            .await;
        assert!(invalid.is_err());
        assert_eq!(server.resource_detail(uri).unwrap(), original);
        let edited = server
            .edit_resource(TextEditInput {
                uri: uri.to_owned(),
                expected_revision: revision.to_owned(),
                edits: vec![TextEdit::Insert {
                    text: "\n# override\n".to_owned(),
                    placement: Placement::End,
                    anchor: None,
                }],
            })
            .await
            .unwrap();
        assert_eq!(edited["persisted"], true);
        let stale = server
            .edit_resource(TextEditInput {
                uri: uri.to_owned(),
                expected_revision: revision.to_owned(),
                edits: vec![TextEdit::Insert {
                    text: "\n# stale\n".to_owned(),
                    placement: Placement::End,
                    anchor: None,
                }],
            })
            .await
            .unwrap_err();
        assert_server_error(&stale, -32602, "revision changed", &[]);
        assert_eq!(server.resource_detail(uri).unwrap(), edited);
        let guarded = server
            .resources
            .replace_guarded(
                "agents/coding.yaml",
                source.to_owned(),
                edited["persisted_etag"].as_str().unwrap(),
                Some("different source"),
            )
            .await
            .unwrap_err();
        assert!(guarded.is_failed_precondition());
        assert_eq!(server.resource_detail(uri).unwrap(), edited);
    }

    #[tokio::test]
    async fn collections_templates_sources_and_session_uris_are_discoverable() {
        let router = mcp::server::streamable_http_router(test_server());
        let resources = mcp_request(
            &router,
            json!({"jsonrpc":"2.0","id":1,"method":"resources/list"}),
        )
        .await;
        let entries = resources["result"]["resources"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["uri"], "waltr://resources");
        assert_eq!(entries[1]["uri"], "waltr://sessions");
        let templates = mcp_request(
            &router,
            json!({"jsonrpc":"2.0","id":1,"method":"resources/templates/list"}),
        )
        .await;
        assert_eq!(
            templates["result"]["resourceTemplates"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let catalog = mcp_request(&router, json!({"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"waltr://resources"}})).await;
        let catalog: Value =
            serde_json::from_str(catalog["result"]["contents"][0]["text"].as_str().unwrap())
                .unwrap();
        for entry in catalog["entries"].as_array().unwrap() {
            assert_eq!(
                resource_path(entry["uri"].as_str().unwrap()).unwrap(),
                entry["path"].as_str().unwrap()
            );
        }
        let source = mcp_request(&router, json!({"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"waltr://resources/agents%2Fcoding.yaml/source"}})).await;
        assert_eq!(
            source["result"]["contents"][0]["mimeType"],
            "application/yaml"
        );
        assert!(
            source["result"]["contents"][0]["text"]
                .as_str()
                .unwrap()
                .contains("kind: Agent")
        );
        let sessions = mcp_request(&router, json!({"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"waltr://sessions"}})).await;
        let sessions: Value =
            serde_json::from_str(sessions["result"]["contents"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(
            sessions["sessions"][0]["uri"],
            "waltr://sessions/session%20%2F%3F%23%25"
        );
        let rejected = mcp_request(&router, json!({"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"waltr://resources/agents/coding.yaml"}})).await;
        assert_eq!(rejected["error"]["code"], -32602);
        let wipe = mcp_request(&router, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"destroy","arguments":{"action":"session.wipe","input":{"confirm":false}}}})).await;
        assert_eq!(wipe["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn resource_reads_and_uniform_tools_remain_behind_local_admission() {
        let router = mcp::server::streamable_http_router(test_server()).layer(
            axum::middleware::from_fn_with_state(
                crate::local_admission::LocalBearer::new("test-generation").unwrap(),
                crate::local_admission::require_local_protocol_bearer,
            ),
        );
        for (method, params) in [
            ("resources/list", json!({})),
            ("resources/templates/list", json!({})),
            ("resources/read", json!({"uri":"waltr://resources"})),
            (
                "tools/call",
                json!({"name":"destroy","arguments":{"action":"session.wipe","input":{"confirm":true}}}),
            ),
        ] {
            for bearer in [None, Some("Bearer wrong-generation")] {
                let mut request =
                    HttpRequest::post("/mcp").header("content-type", "application/json");
                if let Some(bearer) = bearer {
                    request = request.header("authorization", bearer);
                }
                let response = router
                    .clone()
                    .oneshot(
                        request
                            .body(Body::from(
                                json!({
                                    "jsonrpc":"2.0","id":1,"method":method,"params":params,
                                })
                                .to_string(),
                            ))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
            }
        }
    }

    #[test]
    fn mcp_activity_uses_agent_progress_for_every_wait_state() {
        for (phase, running) in [
            (SessionProgressPhase::Running, true),
            (SessionProgressPhase::CompletionBarrier, true),
            (SessionProgressPhase::Paused, true),
            (SessionProgressPhase::AwaitingToolResults, false),
            (SessionProgressPhase::AwaitingExternalInput, false),
            (SessionProgressPhase::Interrupted, false),
            (SessionProgressPhase::Failed, false),
        ] {
            let record = waltr_agent::SessionRecord::new("session-activity");
            let mut summary = record.summary();
            summary.progress.phase = phase;
            summary.progress.running = running;
            let projected = project_summary(&summary, None);
            assert_eq!(projected["running"], running, "{phase:?}");
            assert_eq!(projected["progress"]["running"], running, "{phase:?}");
            assert_eq!(projection_running(&projected), running, "{phase:?}");
        }

        let mut summary = waltr_agent::SessionRecord::new("no-recombination").summary();
        summary.status = SessionStatus::StreamingProvider;
        summary.coordination.phase = SessionCoordinationPhase::Paused;
        summary.progress.running = false;
        assert_eq!(project_summary(&summary, None)["running"], false);

        let mut summary = waltr_agent::SessionRecord::new("suspended-active").summary();
        summary.status = SessionStatus::Suspended(Suspension::Interrupted { work: Vec::new() });
        summary.coordination.phase = SessionCoordinationPhase::Running;
        summary.progress = SessionProgress::from_state(&summary.status, &summary.coordination);
        summary.capabilities =
            SessionCapabilities::from_state(&summary.status, &summary.coordination);
        let projected = project_summary(&summary, None);
        assert_eq!(projected["running"], false);
        assert_eq!(projected["progress"]["running"], false);
        assert!(!projection_running(&projected));
    }

    #[test]
    fn mcp_projects_structured_agent_coordination_without_active_run_derivation() {
        let record = waltr_agent::SessionRecord::new("session-paused");
        let mut summary = record.summary();
        summary.coordination = waltr_agent::SessionCoordinationState {
            phase: SessionCoordinationPhase::Paused,
            reason: Some(
                waltr_agent::SessionCoordinationReason::new(
                    "context-compaction",
                    Some("compact-and-continue".to_owned()),
                    Some("run-completed".to_owned()),
                )
                .unwrap(),
            ),
            queued_count: 2,
            continuation_armed: true,
        };
        summary.progress = waltr_agent::SessionProgress {
            phase: SessionProgressPhase::Paused,
            reason: summary.coordination.reason.clone(),
            queued_count: 2,
            continuation_armed: true,
            running: true,
            ..waltr_agent::SessionProgress::default()
        };
        summary.capabilities = SessionCapabilities {
            prompt_disposition: SessionPromptDisposition::Queued,
            can_respond_external_request: false,
            can_interrupt: false,
        };

        let projected = project_summary(&summary, None);
        assert_eq!(projected["running"], true);
        assert_eq!(projected["coordination"]["phase"], "paused");
        assert_eq!(
            projected["coordination"]["reason"]["code"],
            "context-compaction"
        );
        assert_eq!(projected["coordination"]["queued_count"], 2);
        assert_eq!(projected["coordination"]["continuation_armed"], true);
        assert_eq!(projected["progress"]["phase"], "paused");
        assert_eq!(
            projected["progress"]["reason"]["code"],
            "context-compaction"
        );
        assert_eq!(projected["capabilities"]["prompt_disposition"], "queued");
        assert_eq!(projected["status"], "idle");
        assert!(projected["active_run"].is_null());
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
        let record_summary = record.summary();
        let session = project_session(&record, &record_summary, None);

        assert_eq!(summary["metadata"]["title"], "Projected title");
        assert_eq!(summary["metadata"]["unknown"]["null"], Value::Null);
        assert_eq!(session["metadata"], summary["metadata"]);
    }

    #[tokio::test]
    async fn resource_handlers_create_edit_and_destroy_without_overwrite() {
        let root = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&root, &[], "coding").unwrap();
        let server = WaltrMcpServer::new(Arc::new(UnusedSessionHost), state).unwrap();

        let uri = "waltr://resources/agents%2Fcoding.yaml";
        let inspected = server.resource_detail(uri).unwrap();
        let revision = inspected["revision"].as_str().unwrap().to_owned();
        let source = "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: coding\nspec:\n  description: managed\n";
        let replaced = server
            .create_resource(ResourceCreateInput {
                uri: uri.to_owned(),
                source: source.to_owned(),
                expected_revision: revision.clone(),
            })
            .await
            .unwrap();
        assert_eq!(replaced["persisted_source"], source);
        let occupied = server
            .create_resource(ResourceCreateInput {
                uri: uri.to_owned(),
                source: source.to_owned(),
                expected_revision: replaced["revision"].as_str().unwrap().to_owned(),
            })
            .await
            .unwrap_err();
        assert_server_error(&occupied, -32602, "slot occupied", &[]);

        let edited = server
            .edit_resource(TextEditInput {
                uri: uri.to_owned(),
                expected_revision: replaced["revision"].as_str().unwrap().to_owned(),
                edits: vec![TextEdit::Replace {
                    old_text: "managed".to_owned(),
                    new_text: "edited".to_owned(),
                }],
            })
            .await
            .unwrap();
        assert!(edited["source"].as_str().unwrap().contains("edited"));

        let reset = server
            .destroy_resource(ResourceDestroyInput {
                uri: uri.to_owned(),
                expected_revision: edited["revision"].as_str().unwrap().to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(reset["origin"], "bundled");
        assert_eq!(reset["persisted"], false);
        assert_eq!(reset["revision"], revision);
    }

    #[tokio::test]
    async fn public_mcp_publishes_only_service_actions_and_rejects_removed_actions() {
        let router = mcp::server::streamable_http_router(test_server());

        let tools = mcp_request(
            &router,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        )
        .await;
        let names = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["status", "create", "edit", "destroy", "execute", "query"]
        );
        let schema = tools["result"]["tools"].to_string();
        assert!(!schema.contains("resources.inspect"));
        assert!(!schema.contains("session.get"));
        assert!(!schema.contains("session.list"));
        assert!(!schema.contains("configuration.inspect"));

        assert!(!schema.contains("help"));
        assert!(!schema.contains("runbooks"));

        let definitions = mcp_request(
            &router,
            json!({"jsonrpc":"2.0","id":1,"method":"resources/read", "params":{"uri":"waltr://resources"}}),
        )
        .await;
        assert!(definitions.get("error").is_none(), "{definitions}");
        assert!(definitions.to_string().contains("agents/coding.yaml"));

        for tool in ["exec", "sessions", "query", "execute"] {
            for action in ["resources.inspect", "session.get", "session.list", "help"] {
                let rejected = mcp_request(
                    &router,
                    json!({
                        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": { "name": tool, "arguments": { "action": action } }
                    }),
                )
                .await;
                let code = if matches!(tool, "exec" | "sessions") {
                    -32020
                } else {
                    -32602
                };
                assert_eq!(rejected["error"]["code"], code, "{rejected}");
            }
        }
        let old_resource = mcp_request(
            &router,
            json!({"jsonrpc":"2.0","id":1,"method":"resources/read",
                "params":{"uri":"waltr://runbooks/resources"}}),
        )
        .await;
        assert!(old_resource.get("error").is_some(), "{old_resource}");
    }

    #[tokio::test]
    async fn public_mcp_skills_list_get_and_read_match_complete_embedded_manifest() {
        use sha2::{Digest as _, Sha256};

        let router = mcp::server::streamable_http_router(test_server());
        let discovery = mcp_request(
            &router,
            json!({"jsonrpc":"2.0","id":1,"method":"server/discover"}),
        )
        .await;
        assert_eq!(
            discovery["result"]["capabilities"]["extensions"][mcp::skills::SKILLS_EXTENSION],
            json!({})
        );
        assert!(discovery["result"]["capabilities"]["resources"].is_object());

        let listed = mcp_request(
            &router,
            json!({"jsonrpc":"2.0","id":1,"method":"skills/list"}),
        )
        .await;
        let result = &listed["result"];
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["cacheScope"], "private");
        assert_eq!(result["ttlMs"], 0);
        assert!(result.get("nextCursor").is_none());
        let skills = result["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0]["uri"], "skill://hosted-sessions/SKILL.md");
        assert_eq!(skills[1]["uri"], "skill://resource-administration/SKILL.md");
        for skill in skills {
            let uri = skill["uri"].as_str().unwrap();
            let fetched = mcp_request(
                &router,
                json!({"jsonrpc":"2.0","id":1,"method":"skills/get","params":{"uri":uri}}),
            )
            .await;
            assert_eq!(fetched["result"]["skill"], *skill);
            assert_eq!(fetched["result"]["resultType"], "complete");
            assert_eq!(fetched["result"]["cacheScope"], "private");
            assert_eq!(fetched["result"]["ttlMs"], 0);
            let manifest = skill["resources"].as_array().unwrap();
            assert_eq!(manifest.len(), 2);
            for file in manifest {
                let read = mcp_request(
                    &router,
                    json!({"jsonrpc":"2.0","id":1,"method":"resources/read",
                        "params":{"uri":file["uri"]}}),
                )
                .await;
                assert_eq!(read["result"]["resultType"], "complete");
                assert_eq!(read["result"]["cacheScope"], "private");
                assert_eq!(read["result"]["ttlMs"], 0);
                let content = &read["result"]["contents"][0];
                assert_eq!(content["uri"], file["uri"]);
                assert_eq!(content["mimeType"], "text/markdown");
                let bytes = content["text"].as_str().unwrap().as_bytes();
                assert_eq!(file["size"], bytes.len() as u64);
                let hexadecimal = Sha256::digest(bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                assert_eq!(file["digest"], format!("sha256:{hexadecimal}"));
                if file["uri"] == skill["uri"] {
                    assert_eq!(
                        mcp::skills::parse_skill_frontmatter(bytes).unwrap(),
                        skill["frontmatter"].as_object().unwrap().clone()
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn agent_definition_actions_classify_and_sanitize_failures() {
        let root = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&root, &[], "coding").unwrap();
        let inner = Arc::new(WaltrMcpServer::new(Arc::new(UnusedSessionHost), state).unwrap());

        let validation = inner
            .inspect_resources(InspectResourcesInput {
                path: Some("/private/tmp/invalid.yml".to_owned()),
            })
            .unwrap_err();
        assert_server_error(&validation, -32602, "invalid canonical", &["/private/tmp"]);

        let stale_error = inner
            .create_resource(ResourceCreateInput {
                uri: "waltr://resources/agents%2Fcoding.yaml".to_owned(),
                source: "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: coding\nspec: {}\n".to_owned(),
                expected_revision: "stale".to_owned(),
            })
            .await
            .unwrap_err();
        assert_server_error(&stale_error, -32602, "read and retry", &["sha256:"]);

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
        Arc::new(WaltrMcpServer::new(Arc::new(UnusedSessionHost), state).unwrap())
    }

    #[tokio::test]
    async fn status_app_is_linked_only_with_assets_and_html_capability() {
        let root = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&root, &[], "coding").unwrap();
        let persistence = crate::persistence::Persistence::initialize(
            crate::persistence::DatabaseConfig::for_tests().unwrap(),
        )
        .await
        .unwrap();
        let (_definitions, definitions) =
            tokio::sync::watch::channel(crate::DefinitionWatcherHealth::Running);
        let (_routines, routines) =
            tokio::sync::watch::channel(crate::routine::EngineHealth::Running);
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let readiness = crate::readiness::Readiness {
            persistence,
            oauth: None,
            definitions,
            routines,
            sandbox_alive: alive.clone(),
        };
        assert!(readiness.is_ready().await);
        let server = Arc::new(
            WaltrMcpServer::new(Arc::new(UnusedSessionHost), state)
                .unwrap()
                .with_readiness(readiness),
        );
        let router = mcp::server::streamable_http_router(server);
        let tools = mcp_request(
            &router,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await;
        let status = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "status")
            .unwrap();
        assert!(status.get("_meta").is_none());
        let call = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"status","arguments":{}}});
        let first = mcp_request(&router, call.clone()).await;
        assert_eq!(first["result"]["structuredContent"]["ready"], true);
        assert!(first["result"]["_meta"].get("ui").is_none());
        tokio::time::sleep(Duration::from_millis(2)).await;
        alive.store(false, std::sync::atomic::Ordering::Release);
        let second = mcp_request(&router, call).await;
        assert_eq!(second["result"]["structuredContent"]["ready"], false);
        assert!(
            second["result"]["structuredContent"]["observedAtUnixMs"]
                .as_u64()
                .unwrap()
                > first["result"]["structuredContent"]["observedAtUnixMs"]
                    .as_u64()
                    .unwrap()
        );
        let meta = json!({
            "io.modelcontextprotocol/protocolVersion": mcp::protocol::MCP_PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {"extensions":{"io.modelcontextprotocol/ui":{"mimeTypes":[mcp::apps::UI_MIME_TYPE]}}},
            "io.modelcontextprotocol/clientInfo":{"name":"apps-test","version":"1"}
        });
        let tools = mcp_request(
            &router,
            json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"_meta":meta}}),
        )
        .await;
        let status = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "status")
            .unwrap();
        let resource = mcp_request(&router, json!({"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"ui://waltr/status","_meta":meta}})).await;
        let resources = mcp_request(
            &router,
            json!({"jsonrpc":"2.0","id":6,"method":"resources/list","params":{"_meta":meta}}),
        )
        .await;
        let discovered = mcp_request(
            &router,
            json!({"jsonrpc":"2.0","id":7,"method":"server/discover","params":{"_meta":meta}}),
        )
        .await;
        if cfg!(feature = "apps-served-assets") {
            assert_eq!(status["_meta"]["ui"]["resourceUri"], status_app::URI);
            assert_eq!(
                discovered["result"]["capabilities"]["extensions"][mcp::apps::UI_EXTENSION],
                json!({})
            );
            assert!(
                resources["result"]["resources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["uri"] == status_app::URI
                        && entry["mimeType"] == mcp::apps::UI_MIME_TYPE)
            );
            let decoded = mcp::apps::decode_resource(
                &mcp::McpResourceResult::new(resource["result"].clone()),
                status_app::URI,
                mcp::apps::DEFAULT_MAX_HTML_BYTES,
            )
            .unwrap();
            assert!(decoded.html.contains("Waltr Status"));
            assert!(decoded.html.contains("ui/initialize"));
            assert!(decoded.html.contains("ui/notifications/initialized"));
            assert!(decoded.html.contains("tools/call"));
            assert!(decoded.html.contains("observedAtUnixMs"));
            assert!(!decoded.html.contains("<!-- STATUS_SCRIPT -->"));
            assert!(!decoded.html.contains("<script src="));
            assert!(!decoded.html.contains("https://cdn"));
            let csp = decoded.ui.csp.unwrap();
            assert_eq!(csp.connect_domains, Some(vec![]));
            assert_eq!(csp.resource_domains, Some(vec![]));
            assert_eq!(csp.frame_domains, Some(vec![]));
            assert_eq!(csp.base_uri_domains, Some(vec![]));
            assert_eq!(
                decoded.ui.permissions,
                Some(mcp::apps::UiPermissions::default())
            );
            let refresh = mcp_request(&router, json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"status","arguments":{},"_meta":meta}})).await;
            assert_eq!(refresh["result"]["structuredContent"]["ready"], false);
            assert_eq!(refresh["result"]["content"][0]["text"], "Waltr is not ready");
        } else {
            assert!(status.get("_meta").is_none());
            assert!(resource.get("error").is_some());
            assert!(
                discovered["result"]["capabilities"]["extensions"]
                    .get(mcp::apps::UI_EXTENSION)
                    .is_none()
            );
            assert!(
                !resources["result"]["resources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["uri"] == status_app::URI)
            );
        }
        let denied = mcp_request(&router, json!({"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"ui://waltr/status"}})).await;
        assert!(denied.get("error").is_some());
        for capability in [
            json!({"mimeTypes": []}),
            json!({"mimeTypes": ["text/html"]}),
            json!({"mimeTypes": null}),
        ] {
            let mut unsupported = meta.clone();
            unsupported["io.modelcontextprotocol/clientCapabilities"]["extensions"]
                [mcp::apps::UI_EXTENSION] = capability;
            let tools = mcp_request(&router, json!({"jsonrpc":"2.0","id":9,"method":"tools/list","params":{"_meta":unsupported}})).await;
            assert!(
                tools["result"]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|tool| tool["name"] == "status")
                    .unwrap()
                    .get("_meta")
                    .is_none()
            );
            let denied = mcp_request(&router, json!({"jsonrpc":"2.0","id":10,"method":"resources/read","params":{"uri":status_app::URI,"_meta":unsupported}})).await;
            assert!(denied.get("error").is_some());
        }
    }

    async fn mcp_request(router: &Router, mut body: Value) -> Value {
        body["params"] = body.get("params").cloned().unwrap_or_else(|| json!({}));
        if body["params"].get("_meta").is_none() {
            body["params"]["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": mcp::protocol::MCP_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {
                    "extensions": { "io.modelcontextprotocol/skills": {} }
                },
                "io.modelcontextprotocol/clientInfo": { "name": "waltr-server-tests", "version": "1" }
            });
        }
        let method = body["method"].as_str().unwrap();
        let mut request = HttpRequest::post("/mcp")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
            .header("mcp-method", method);
        if method == "tools/call" {
            request = request.header("mcp-name", body["params"]["name"].as_str().unwrap());
        } else if method == "resources/read" {
            request = request.header("mcp-name", body["params"]["uri"].as_str().unwrap());
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
