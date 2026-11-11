//! Durable helpers and the interpreter-to-neutral completion observer adapter.

use crate::routine::{
    definition::{Contract, SCHEMA_VERSION, Schema},
    ports::{
        HelperCapture, HelperPayload, HelperRequest, HelperResult, HelperRunner, HelperSelection,
        PortError, PortFuture,
    },
};
use crate::{
    agent_host::{
        AgentDefinitionBundle, Capability,
        runtime_factory::ServerAgentRuntimeFactory as AgentSessionRuntimeFactory,
    },
    permission::PermissionPolicy,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    num::NonZeroU64,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use waltr_agent::{
    AgentRunLimits, CompletionDecision, CompletionFeedback, CompletionObserver,
    CompletionObserverError, CompletionObserverFuture, CompletionProposal, CompletionValidation,
    InferenceSettings, SessionCoordinator, SessionCopy, SessionCreateRequest,
    SessionExecutionOwner, SessionId, SessionStart, SessionStartMessage, SessionStatus,
    SessionStore, ToolDefinition,
};
use waltr_script::{CallbackRef, Limits, StructuredValue};

const CAPTURE_TOOL: &str = "_helper_capture";
mod inference;
pub use inference::{HelperInput, InferenceSelector};
type OriginGuard = Arc<dyn Fn() -> PortFuture<()> + Send + Sync>;

// Only typed variants enter diagnostics; error messages and opaque host context never do.
fn auth_category(reason: waltr_agent::AuthenticationFailureReason) -> &'static str {
    use waltr_agent::AuthenticationFailureReason::*;
    match reason {
        CredentialRequired => "auth_credential_required",
        CredentialRejected => "auth_credential_rejected",
        TokenExchangeFailed => "auth_token_exchange_failed",
        NetworkFailed => "auth_network_failed",
        ResponseInvalid => "auth_response_invalid",
        ConfigurationRequired => "auth_configuration_required",
    }
}

fn llm_category(error: &waltr_agent::LlmError) -> &'static str {
    use waltr_agent::LlmError::*;
    match error {
        AuthenticationFailure { failure, .. } => auth_category(failure.reason),
        Authorization { .. } => "authorization",
        MissingConfig { .. } => "missing_config",
        Http(_) => "http",
        WebSocket(_) => "websocket",
        Request(_) => "request",
        Json(_) => "json",
        Io(_) => "io",
        Api { .. } => "provider_api",
        Protocol { .. } => "protocol",
    }
}

fn runtime_category(error: &waltr_agent::AgentSessionRuntimeError) -> &'static str {
    use waltr_agent::AgentSessionRuntimeError::*;
    match error {
        Authentication { reason, .. } => auth_category(*reason),
        Agent(error) => llm_category(error),
        FailedPrecondition(_) => "runtime_precondition",
        Internal(_) => "runtime_internal",
    }
}

fn helper_failure(
    stage: &'static str,
    category: &'static str,
    session: Option<&SessionId>,
    result: PortError,
) -> PortError {
    tracing::warn!(
        stage,
        category,
        helper_session_id = session.map(SessionId::as_str),
        "routine helper failed"
    );
    result
}

fn owned_failure(error: waltr_agent::OwnedExecutionError, id: &SessionId) -> PortError {
    let category = match error {
        waltr_agent::OwnedExecutionError::Unavailable => "owned_unavailable",
        waltr_agent::OwnedExecutionError::Uncertain => "owned_uncertain",
    };
    helper_failure("run", category, Some(id), PortError::Uncertain)
}

fn match_completion_response(
    assistant_text: Option<&str>,
    response: &str,
    id: &SessionId,
) -> Result<(), PortError> {
    // Completion proposals represent an absent final assistant text as an empty string.
    if assistant_text.unwrap_or_default() != response {
        return Err(helper_failure(
            "completion_match",
            "response_mismatch",
            Some(id),
            PortError::Uncertain,
        ));
    }
    Ok(())
}

fn status_category(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Failed {
            authentication_failure: Some(failure),
            ..
        } => auth_category(failure.reason),
        SessionStatus::Failed { .. } => "failed",
        SessionStatus::Idle => "idle",
        SessionStatus::BuildingRequest => "building_request",
        SessionStatus::StreamingProvider => "streaming_provider",
        SessionStatus::RetryingProvider { .. } => "retrying_provider",
        SessionStatus::EvaluatingToolCall => "evaluating_tool_call",
        SessionStatus::RunningTool => "running_tool",
        SessionStatus::Suspended(_) => "suspended",
        SessionStatus::Finished => "finished",
    }
}

/// Host-constructed ad-hoc helper. The runtime must have been built with only its declared
/// prompt, inference and concrete tools; this adapter never inherits ambient session tools.
#[derive(Clone)]
pub struct RestrictedHelperProfile {
    /// Explicit isolated runtime, not a catalog agent with copied policy fields.
    pub runtime: crate::agent_host::HostRuntimeBuilder,
    /// Explicit durable inference identity.
    pub inference: InferenceSettings,
    /// Explicit task prefix, at most 16384 bytes.
    pub prompt: String,
    /// Total provider turns, including corrections; one remains one.
    pub max_turns: u64,
}

struct OwnerState<S: SessionStore + Clone + Send + Sync + 'static> {
    closed: bool,
    owners: Vec<SessionExecutionOwner<S>>,
}

/// Real coordinator-backed implementation. All runtime/catalog/source inputs are supplied
/// by the authenticated host constructor, never selected from an ambient current session.
pub struct CoordinatorHelperRunner<S: SessionStore + Clone + Send + Sync + 'static> {
    sessions: SessionCoordinator<S>,
    factory: Arc<AgentSessionRuntimeFactory>,
    inference: Option<InferenceSettings>,
    catalog: Option<Arc<crate::resource_catalog::AgentDefinitionSnapshot>>,
    source_inference: Option<InferenceSettings>,
    restricted: BTreeMap<String, RestrictedHelperProfile>,
    captures: Arc<Mutex<BTreeMap<String, Contract>>>,
    capture_bindings: Arc<Mutex<Option<BTreeMap<String, HelperSelection>>>>,
    input_models: BTreeMap<(String, String), waltr_agent::TransientModelCapabilities>,
    source: Option<SessionCopy>,
    principal: String,
    named: Arc<Mutex<Option<BTreeMap<String, AgentDefinitionBundle>>>>,
    owners: Arc<tokio::sync::Mutex<OwnerState<S>>>,
    runtime: tokio::runtime::Handle,
    restriction: Option<PermissionPolicy>,
    origin_guard: Option<OriginGuard>,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> std::fmt::Debug
    for CoordinatorHelperRunner<S>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoordinatorHelperRunner")
            .finish_non_exhaustive()
    }
}

impl<S: SessionStore + Clone + Send + Sync + 'static> Clone for CoordinatorHelperRunner<S> {
    fn clone(&self) -> Self {
        Self {
            sessions: self.sessions.clone(),
            factory: self.factory.clone(),
            inference: self.inference.clone(),
            catalog: self.catalog.clone(),
            source_inference: self.source_inference.clone(),
            restricted: self.restricted.clone(),
            captures: self.captures.clone(),
            capture_bindings: self.capture_bindings.clone(),
            input_models: self.input_models.clone(),
            source: self.source.clone(),
            principal: self.principal.clone(),
            named: self.named.clone(),
            owners: self.owners.clone(),
            runtime: self.runtime.clone(),
            restriction: self.restriction.clone(),
            origin_guard: self.origin_guard.clone(),
        }
    }
}

impl<S: SessionStore + Clone + Send + Sync + 'static> CoordinatorHelperRunner<S> {
    fn capture_contract(&self, selection: &HelperSelection, name: Option<&str>) -> Result<Option<Contract>, PortError> {
        let Some(name) = name else { return Ok(None); };
        let bindings = self.capture_bindings.lock().map_err(|_| PortError::Unavailable)?;
        if bindings.as_ref().is_some_and(|bindings| bindings.get(name) != Some(selection)) {
            return Err(PortError::Denied);
        }
        self.captures.lock().map_err(|_| PortError::Unavailable)?.get(name).cloned().map(Some).ok_or(PortError::Denied)
    }
    /// Bind a private invocation to authenticated inputs. `source` is the agent-owned,
    /// rollback-effective completed-history projection, not a raw source record.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sessions: SessionCoordinator<S>,
        factory: Arc<AgentSessionRuntimeFactory>,
        inference: impl Into<Option<InferenceSettings>>,
        restricted: BTreeMap<String, RestrictedHelperProfile>,
        captures: BTreeMap<String, Contract>,
        source: Option<SessionCopy>,
        principal: String,
    ) -> Result<Self, PortError> {
        if principal.is_empty()
            || principal.len() > 256
            || restricted.len() > 32
            || captures.len() > 32
            || restricted.values().any(|profile| {
                profile.prompt.len() > 16384 || !(1..=64).contains(&profile.max_turns)
            })
            || captures.values().any(|contract| {
                contract.validate_capture().is_err() || contract.schema_version != SCHEMA_VERSION
                    || serde_json::to_vec(contract).map_or(true, |bytes| bytes.len() > 16384)
            })
        {
            return Err(PortError::Invalid);
        }
        Ok(Self {
            sessions,
            factory,
            inference: inference.into(),
            catalog: None,
            source_inference: None,
            restricted,
            captures: Arc::new(Mutex::new(captures)),
            capture_bindings: Arc::new(Mutex::new(None)),
            input_models: BTreeMap::new(),
            source,
            principal,
            named: Arc::new(Mutex::new(None)),
            owners: Arc::new(tokio::sync::Mutex::new(OwnerState {
                closed: false,
                owners: Vec::new(),
            })),
            runtime: tokio::runtime::Handle::current(),
            restriction: None,
            origin_guard: None,
        })
    }

    /// Retain normal permission and Skills consent inside the original owned helper run.
    #[must_use]

    pub(crate) fn with_origin_guard(
        mut self,
        guard: impl Fn() -> PortFuture<()> + Send + Sync + 'static,
    ) -> Self {
        self.origin_guard = Some(Arc::new(guard));
        self
    }

    #[cfg(test)]
    pub(crate) fn with_input_models(mut self, models: BTreeMap<(String, String), waltr_agent::TransientModelCapabilities>) -> Self {
        self.input_models = models;
        self
    }

    pub(crate) fn with_catalog(mut self, catalog: Arc<crate::resource_catalog::AgentDefinitionSnapshot>, source: Option<InferenceSettings>) -> Self {
        self.catalog = Some(catalog);
        self.source_inference = source;
        self
    }

    /// Narrow the actual resolved runtime, retaining its named policy and ordinary lifecycle.
    #[must_use]
    pub fn with_permission_restriction(mut self, restriction: PermissionPolicy) -> Self {
        self.restriction = Some(match self.restriction {
            Some(current) => current.intersection(restriction),
            None => restriction,
        });
        self
    }
}

impl<S: SessionStore + Clone + Send + Sync + 'static> HelperRunner for CoordinatorHelperRunner<S> {
    fn select_inference(&self, selector: &InferenceSelector) -> Result<Option<Arc<dyn HelperRunner>>, PortError> {
        let Some(catalog) = &self.catalog else {
            return if selector.is_default() { Ok(None) } else { Err(PortError::UnsupportedCapability) };
        };
        let mut selected = self.clone();
        selected.inference = Some(selector.resolve(catalog, self.source_inference.as_ref())?);
        Ok(Some(Arc::new(selected)))
    }
    fn preflight_captures(&self) -> PortFuture<()> {
        let this = self.clone();
        Box::pin(async move {
            let bindings = this.capture_bindings.lock().map_err(|_| PortError::Unavailable)?
                .clone().ok_or(PortError::Invalid)?;
            for (capture, selection) in bindings {
                this.capture_contract(&selection, Some(&capture))?;
                let policy = match &selection {
                    HelperSelection::Named { name } => {
                        let bundle = this.named.lock().map_err(|_| PortError::Unavailable)?
                            .as_ref().and_then(|named| named.get(name)).cloned().ok_or(PortError::Denied)?;
                        this.factory
                            .helper_capture_policy(&bundle)
                            .map_err(|error| {
                                helper_failure(
                                    "capture_preflight",
                                    runtime_category(&error),
                                    None,
                                    PortError::Unavailable,
                                )
                            })?
                    }
                    HelperSelection::Restricted { profile } => this.restricted.get(profile)
                        .ok_or(PortError::Denied)?.runtime.helper_capture_policy(),
                };
                let policy = match &this.restriction {
                    Some(restriction) => policy.intersection(restriction.clone()),
                    None => policy,
                };
                if policy.evaluate_target_tool_effect(&format!("builtin:{CAPTURE_TOOL}"), CAPTURE_TOOL).level == crate::permission::PermissionLevel::Deny {
                    return Err(PortError::Denied);
                }
            }
            Ok(())
        })
    }
    fn bind_captures(&self, captures: BTreeMap<String, crate::routine::definition::CaptureContract>) -> PortFuture<()> {
        let this = self.clone();
        Box::pin(async move {
            if captures.len() > 32 || captures.values().any(|capture| capture.contract.validate_capture().is_err()) {
                return Err(PortError::Invalid);
            }
            let mut bindings = this.capture_bindings.lock().map_err(|_| PortError::Unavailable)?;
            if bindings.is_some() { return Err(PortError::Conflict); }
            let mut contracts = this.captures.lock().map_err(|_| PortError::Unavailable)?;
            *bindings = Some(captures.iter().map(|(name, capture)| (name.clone(), capture.selection.clone())).collect());
            *contracts = captures.into_iter().map(|(name, capture)| (name, capture.contract)).collect();
            Ok(())
        })
    }
    fn input_capabilities(&self, selection: HelperSelection) -> PortFuture<waltr_agent::TransientModelCapabilities> {
        let this = self.clone();
        Box::pin(async move {
            let inference = match &selection {
                HelperSelection::Named { name } => {
                    if !this.named.lock().map_err(|_| PortError::Unavailable)?.as_ref().is_some_and(|named| named.contains_key(name)) { return Err(PortError::Denied); }
                    this.inference.as_ref().ok_or(PortError::Invalid)?
                }
                HelperSelection::Restricted { profile } => &this.restricted.get(profile).ok_or(PortError::Denied)?.inference,
            };
            if this.catalog.is_some() {
                // Every supported inference encoder implements UTF-8; image/PDF model support is not catalogued.
                Ok(waltr_agent::TransientModelCapabilities { png: false, jpeg: false, webp: false, pdf: false, text: true, max_body_bytes: 32 * 1024 * 1024 })
            } else {
                this.input_models.get(&(inference.provider.key.clone(), inference.provider.model_id.clone())).copied().ok_or(PortError::UnsupportedCapability)
            }
        })
    }
    fn run_with_inputs(&self, request: HelperRequest, source: Option<SessionCopy>, payload: Option<HelperPayload>, cancellation: CancellationToken) -> PortFuture<HelperResult> {
        let mut runner = self.clone();
        runner.source = source;
        runner.run_internal(request, payload, cancellation)
    }
    fn preflight_payload(
        &self,
        selection: HelperSelection,
        capabilities: waltr_agent::TransientModelCapabilities,
        capture: Option<String>,
    ) -> PortFuture<()> {
        let this = self.clone();
        Box::pin(async move {
            this.capture_contract(&selection, capture.as_deref())?;
            let allowed = this.input_capabilities(selection.clone()).await?;
            if capabilities.png && !allowed.png || capabilities.jpeg && !allowed.jpeg
                || capabilities.webp && !allowed.webp || capabilities.pdf && !allowed.pdf
                || capabilities.text && !allowed.text || capabilities.max_body_bytes > allowed.max_body_bytes {
                return Err(PortError::Denied);
            }
            if capabilities.max_body_bytes == 0
                || capabilities.max_body_bytes > 32 * 1024 * 1024
            {
                return Err(PortError::Invalid);
            }
            let api = match selection {
                HelperSelection::Named { name } => {
                    if !this
                        .named
                        .lock()
                        .map_err(|_| PortError::Unavailable)?
                        .as_ref()
                        .is_some_and(|names| names.contains_key(&name))
                    {
                        return Err(PortError::Denied);
                    }
                    this.inference.as_ref().ok_or(PortError::Invalid)?.provider.api
                }
                HelperSelection::Restricted { profile } => {
                    let profile = this.restricted.get(&profile).ok_or(PortError::Denied)?;
                    if profile.runtime.model().api != profile.inference.provider.api {
                        return Err(PortError::Invalid);
                    }
                    profile.runtime.model().api
                }
            };
            match api {
                waltr_agent::ModelApi::AnthropicMessages
                | waltr_agent::ModelApi::OpenAiChatCompletions
                | waltr_agent::ModelApi::OpenAiResponses
                | waltr_agent::ModelApi::OpenAiResponsesWebSocket => {}
            }
            Ok(())
        })
    }
    fn run_with_source(
        &self,
        request: HelperRequest,
        source: Option<SessionCopy>,
        cancellation: CancellationToken,
    ) -> PortFuture<HelperResult> {
        let mut runner = self.clone();
        if let Some(source) = source {
            runner.source = Some(source);
        }
        runner.run(request, cancellation)
    }
    fn pin_named(&self, names: Vec<String>) -> PortFuture<()> {
        let this = self.clone();
        Box::pin(async move {
            if names.len() > 32 {
                return Err(PortError::Invalid);
            }
            let mut named = this.named.lock().map_err(|_| PortError::Unavailable)?;
            if named.is_some() {
                return Err(PortError::Conflict);
            }
            let mut pinned = BTreeMap::new();
            for name in names {
                let bundle = if let Some(catalog) = &this.catalog {
                    catalog.catalog().resolved(&name).ok_or(PortError::Denied)?;
                    AgentDefinitionBundle::new(&name, catalog.sources().to_vec())
                } else { this.factory.bundle_for_agent(&name).map_err(|error| {
                    helper_failure(
                        "pin_named",
                        runtime_category(&error),
                        None,
                        PortError::Denied,
                    )
                })? };
                pinned.insert(name, bundle);
            }
            *named = Some(pinned);
            Ok(())
        })
    }
    fn run(
        &self,
        request: HelperRequest,
        cancellation: CancellationToken,
    ) -> PortFuture<HelperResult> {
        self.run_internal(request, None, cancellation)
    }
    fn run_with_payload(
        &self,
        request: HelperRequest,
        payload: HelperPayload,
        cancellation: CancellationToken,
    ) -> PortFuture<HelperResult> {
        self.run_internal(request, Some(payload), cancellation)
    }

    fn cleanup(&self) -> PortFuture<()> {
        let owners = self.owners.clone();
        Box::pin(async move {
            let owned = {
                let mut state = owners.lock().await;
                state.closed = true;
                state.owners.clone()
            };
            let mut failed = false;
            for owner in owned {
                failed |= owner.close().await.is_err();
            }
            if failed {
                Err(PortError::Uncertain)
            } else {
                Ok(())
            }
        })
    }
}

impl<S: SessionStore + Clone + Send + Sync + 'static> CoordinatorHelperRunner<S> {
    fn run_internal(
        &self,
        request: HelperRequest,
        payload: Option<HelperPayload>,
        cancellation: CancellationToken,
    ) -> PortFuture<HelperResult> {
        let this = self.clone();
        Box::pin(async move {
            let completion_bytes = payload.as_ref().map_or(262144, |p| p.completion_bytes);
            if let Some(payload) = &payload {
                if !payload.attachments.is_empty() {
                    this.preflight_payload(request.selection.clone(), payload.capabilities, request.capture.clone()).await?;
                }
            }
            if let Some(guard) = &this.origin_guard {
                guard().await.map_err(|error| {
                    helper_failure("origin_preflight", "origin_ended", None, error)
                })?;
            }
            let capture_bytes = if payload.is_some() {
                completion_bytes
            } else {
                65536
            };
            if payload.as_ref().is_some_and(|p| {
                p.response_bytes == 0
                    || p.response_bytes > 1048576
                    || p.completion_bytes == 0
                    || p.completion_bytes > 32 * 1024 * 1024
            }) {
                return Err(PortError::Invalid);
            }
            if request.task.is_empty()
                || request.task.len() > 16384
                || !(1..=64).contains(&request.max_turns)
                || request.deadline.is_zero()
                || request.deadline > Duration::from_secs(300)
                || cancellation.is_cancelled()
            {
                return Err(PortError::Invalid);
            }
            let started = tokio::time::Instant::now();
            let capture = this.capture_contract(&request.selection, request.capture.as_deref())?;
            let mut create = SessionCreateRequest::new(SessionId::generate());
            let (runtime, definition_identity, task) = match &request.selection {
                HelperSelection::Named { name } => {
                    let bundle = this
                        .named
                        .lock()
                        .map_err(|_| PortError::Unavailable)?
                        .as_ref()
                        .and_then(|pinned| pinned.get(name))
                        .cloned()
                        .ok_or(PortError::Denied)?;
                    let identity = bundle.identity();
                    create.agent_definition =
                        Some(serde_json::to_value(bundle).map_err(|_| PortError::Invalid)?);
                    create.inference_settings = Some(this.inference.clone().ok_or(PortError::Invalid)?);
                    (None, identity, request.task)
                }
                HelperSelection::Restricted { profile } => {
                    let selected = this.restricted.get(profile).ok_or(PortError::Denied)?;
                    if request.max_turns > selected.max_turns {
                        return Err(PortError::Denied);
                    }
                    create.inference_settings = Some(selected.inference.clone());
                    (
                        Some(selected.runtime.clone()),
                        format!("restricted:{profile}"),
                        format!("{}\n{}", selected.prompt, request.task),
                    )
                }
            };
            let messages = vec![SessionStartMessage::user(task).map_err(|_| PortError::Invalid)?];
            create.start = match this.source.clone() {
                Some(copy) => SessionStart::Copy { copy, messages },
                None => SessionStart::Fresh { messages },
            };
            let mut owners = this.owners.lock().await;
            if owners.closed || owners.owners.len() >= 64 {
                return Err(PortError::Denied);
            }
            let session = this
                .sessions
                .clone()
                .with_session_event_sink(Arc::new(crate::agent_host::session_roles::PrivateCreationEvents))
                .create(create)
                .await
                .map_err(|error| {
                    helper_failure(
                        "session_create",
                        llm_category(&error),
                        None,
                        PortError::Unavailable,
                    )
                })?;
            let id = session.id().clone();
            this.factory
                .session_roles
                .register(
                    &session.record,
                    crate::agent_host::session_roles::SessionRole::Helper,
                )
                .map_err(|_| {
                    helper_failure(
                        "session_register",
                        "session_role",
                        Some(&id),
                        PortError::Unavailable,
                    )
                })?;
            let owner = SessionExecutionOwner::on_runtime(
                this.sessions.clone(),
                id.clone(),
                this.runtime.clone(),
            );
            owners.owners.push(owner.clone());
            drop(owners);
            let remaining = request
                .deadline
                .checked_sub(started.elapsed())
                .ok_or_else(|| {
                    helper_failure(
                        "runtime_assembly",
                        "deadline",
                        Some(&id),
                        PortError::Unavailable,
                    )
                })?;
            let mut factory = (*this.factory).clone();
            if let Some(restriction) = &this.restriction {
                factory = factory.with_permission_restriction(restriction.clone());
            }
            if let Some(contract) = &capture {
                let contract = contract.clone();
                let schema = capture_schema(&contract.schema);
                factory = factory.with_host_tool_provider(move |_, _, _| {
                    let contract = contract.clone();
                    Ok(vec![Capability::new(ToolDefinition::new(CAPTURE_TOOL,
                        "Submit the declared structured capture before your final response. This is not semantic completion validation.", schema.clone()), move |context| {
                        if !contract.accepts_host(&context.call().arguments, capture_bytes) {
                            return Ok(context.result("capture does not match declared contract").with_error());
                        }
                        Ok(context.result(context.call().arguments.to_string()))
                    })])
                });
            }
            let mut runtime = match runtime {
                Some(mut builder) => {
                    if let Some(contract) = &capture {
                        let contract = contract.clone();
                        builder = builder.with_tool(Capability::new(
                            ToolDefinition::new(
                                CAPTURE_TOOL,
                                "Submit the declared structured capture.",
                                capture_schema(&contract.schema),
                            ),
                            move |context| {
                                if !contract.accepts_host(&context.call().arguments, capture_bytes)
                                {
                                    return Ok(context
                                        .result("capture does not match declared contract")
                                        .with_error());
                                }
                                Ok(context.result(context.call().arguments.to_string()))
                            },
                        ));
                    }
                    if let Some(restriction) = &this.restriction {
                        builder = builder.narrowed_permissions(restriction.clone());
                    }
                    builder
                        .with_dispatch_context(Some(id.clone()), factory.capability_policy())
                        .build()
                }
                None => tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(helper_failure("runtime_assembly", "cancelled", Some(&id), PortError::Unavailable)),
                    result = tokio::time::timeout(remaining, factory.runtime_for_record(&session.record, this.sessions.clone())) =>
                        result.map_err(|_| helper_failure("runtime_assembly", "deadline", Some(&id), PortError::Unavailable))?
                            .map_err(|error| helper_failure("runtime_assembly", runtime_category(&error), Some(&id), PortError::Unavailable))?.0,
                },
            };
            let accepted = Arc::new(Mutex::new(None));
            if let Some(guard) = this.origin_guard.clone() {
                let guard_id = id.clone();
                runtime = runtime.with_tool_execution_guard(move |cancellation| {
                    let guard = guard.clone();
                    let id = guard_id.clone();
                    Box::pin(async move {
                        if cancellation.is_cancelled() {
                            helper_failure(
                                "tool_origin",
                                "cancelled",
                                Some(&id),
                                PortError::Unavailable,
                            );
                            return Err(waltr_agent::LlmError::Protocol {
                                provider: "routine",
                                message: "helper origin cancelled".into(),
                            });
                        }
                        guard().await.map_err(|_| {
                            helper_failure(
                                "tool_origin",
                                "origin_ended",
                                Some(&id),
                                PortError::Unavailable,
                            );
                            waltr_agent::LlmError::Protocol {
                                provider: "routine",
                                message: "helper origin authority ended".into(),
                            }
                        })
                    })
                });
            }
            let observer = Arc::new(CapturingObserver {
                semantic: request.observer,
                capture,
                accepted: accepted.clone(),
                completion_bytes: capture_bytes,
                response_bytes: payload.as_ref().map_or(usize::MAX, |p| p.response_bytes),
            });
            let remaining = request
                .deadline
                .checked_sub(started.elapsed())
                .ok_or_else(|| {
                    helper_failure(
                        "completion_validation",
                        "deadline",
                        Some(&id),
                        PortError::Unavailable,
                    )
                })?;
            let validation = CompletionValidation::new(observer, request.max_turns, remaining)
                .and_then(|validation| validation.with_encoded_byte_limit(completion_bytes))
                .map_err(|_| {
                    helper_failure(
                        "completion_validation",
                        "invalid_limits",
                        Some(&id),
                        PortError::Invalid,
                    )
                })?;
            let limits = AgentRunLimits {
                max_provider_turns: NonZeroU64::new(request.max_turns),
            };
            let run_cancellation = cancellation.child_token();
            let outcome = async {
                match &payload {
                    Some(payload) if !payload.attachments.is_empty() => {
                        let record = this.sessions.store().load(&id).await.map_err(|_| {
                            helper_failure(
                                "transient_input",
                                "session_store",
                                Some(&id),
                                PortError::Unavailable,
                            )
                        })?;
                        let event = record
                            .timeline_events()
                            .filter(|event| {
                                event.channel.as_ref().is_some_and(|channel| {
                                    channel.as_str() == waltr_agent::timeline::CHANNEL_USER_TEXT
                                })
                            })
                            .last()
                            .ok_or_else(|| {
                                helper_failure(
                                    "transient_input",
                                    "missing_task_event",
                                    Some(&id),
                                    PortError::Invalid,
                                )
                            })?;
                        let context =
                            record.materialize_context(waltr_agent::SessionContextMode::Effective);
                        let target = waltr_agent::TransientPromptTarget {
                            cursor: event.cursor,
                            turn: context.next_turn,
                            prompt_index: context.prompt_turns.len().checked_sub(1).ok_or_else(
                                || {
                                    helper_failure(
                                        "transient_input",
                                        "missing_prompt",
                                        Some(&id),
                                        PortError::Invalid,
                                    )
                                },
                            )?,
                        };
                        let input = waltr_agent::TransientInput::new(
                            &payload.budget,
                            target,
                            payload.capabilities,
                            payload.attachments.clone(),
                        )
                        .map_err(|error| {
                            helper_failure(
                                "transient_input",
                                llm_category(&error),
                                Some(&id),
                                PortError::Invalid,
                            )
                        })?;
                        owner
                            .helper_with_transient_input(
                                runtime,
                                input,
                                validation,
                                limits,
                                run_cancellation.clone(),
                            )
                            .await
                            .map_err(|error| owned_failure(error, &id))
                    }
                    _ => owner
                        .helper(runtime, validation, limits, run_cancellation.clone())
                        .await
                        .map_err(|error| owned_failure(error, &id)),
                }
            };
            tokio::pin!(outcome);
            let invalid_origin = async {
                let Some(guard) = &this.origin_guard else {
                    return std::future::pending::<()>().await;
                };
                loop {
                    if guard().await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            };
            let outcome = tokio::select! { biased;
                () = invalid_origin => {
                    helper_failure("run_origin", "origin_ended", Some(&id), PortError::Uncertain);
                    run_cancellation.cancel();
                    drop(outcome.await);
                    return Err(PortError::Uncertain);
                }
                result = &mut outcome => result.map_err(|_| PortError::Uncertain)?,
            };
            if let Some(guard) = &this.origin_guard {
                guard().await.map_err(|_| {
                    helper_failure(
                        "completion_origin",
                        "origin_ended",
                        Some(&id),
                        PortError::Uncertain,
                    )
                })?;
            }
            if outcome.status != SessionStatus::Finished {
                return Err(helper_failure(
                    "run_status",
                    status_category(&outcome.status),
                    Some(&id),
                    PortError::Uncertain,
                ));
            }
            let (proposal, captures) = accepted
                .lock()
                .map_err(|_| {
                    helper_failure(
                        "completion_acceptance",
                        "lock_poisoned",
                        Some(&id),
                        PortError::Unavailable,
                    )
                })?
                .take()
                .ok_or_else(|| {
                    helper_failure(
                        "completion_acceptance",
                        "missing_acceptance",
                        Some(&id),
                        PortError::Uncertain,
                    )
                })?;
            match_completion_response(outcome.assistant_text.as_deref(), &proposal.response, &id)?;
            let result = HelperResult {
                session_id: id.clone(),
                definition_identity,
                response: proposal.response,
                captures,
                evidence: proposal.evidence,
            };
            if payload
                .as_ref()
                .is_some_and(|p| result.response.len() > p.response_bytes)
            {
                return Err(PortError::Invalid);
            }
            if payload.is_none()
                && serde_json::to_vec(&result)
                    .map_err(|_| PortError::Invalid)?
                    .len()
                    > 65536
            {
                return Err(PortError::Invalid);
            }
            Ok(result)
        })
    }
}

type AcceptedCapture = Arc<Mutex<Option<(CompletionProposal, Vec<HelperCapture>)>>>;

struct CapturingObserver {
    completion_bytes: usize,
    response_bytes: usize,
    semantic: Option<Arc<dyn CompletionObserver>>,
    capture: Option<Contract>,
    accepted: AcceptedCapture,
}

impl CompletionObserver for CapturingObserver {
    fn observe(
        &self,
        proposal: CompletionProposal,
        cancellation: CancellationToken,
    ) -> CompletionObserverFuture {
        let semantic = self.semantic.clone();
        let contract = self.capture.clone();
        let accepted = self.accepted.clone();
        let completion_bytes = self.completion_bytes;
        let response_bytes = self.response_bytes;
        Box::pin(async move {
            if proposal.response.len() > response_bytes {
                return Err(CompletionObserverError::Invalid);
            }
            let mut captures = Vec::new();
            if let Some(contract) = contract {
                for evidence in &proposal.evidence {
                    if evidence.call.name == CAPTURE_TOOL
                        && evidence.result.name == CAPTURE_TOOL
                        && evidence.result.call_id == evidence.call.id
                        && !evidence.result.is_error {
                        let value: Value = serde_json::from_str(&evidence.result.content)
                            .map_err(|_| CompletionObserverError::Invalid)?;
                        if !contract.accepts_host(&value, completion_bytes) {
                            return Err(CompletionObserverError::Invalid);
                        }
                        captures.push(HelperCapture {
                            call_id: evidence.call.id.clone(),
                            item_id: evidence.item_id.clone(),
                            value,
                        });
                    }
                }
                if captures.is_empty() {
                    return Ok(CompletionDecision::Reject {
                        feedback: CompletionFeedback {
                            proposal_id: proposal.proposal_id,
                            call_id: None,
                            message: "Submit a valid declared structured capture before finishing."
                                .into(),
                        },
                    });
                }
            }
            let decision = match semantic {
                Some(observer) => observer.observe(proposal.clone(), cancellation).await?,
                None => CompletionDecision::Accept,
            };
            if matches!(decision, CompletionDecision::Accept) {
                *accepted
                    .lock()
                    .map_err(|_| CompletionObserverError::Failed)? = Some((proposal, captures));
            }
            Ok(decision)
        })
    }
}

/// Invoke rooted script callbacks without introducing interpreter types into the agent.
#[derive(Debug)]
pub struct ScriptCompletionObserver {
    /// Invocation-owned callback; never persisted or resumed.
    pub callback: CallbackRef,
    /// Invocation's value limits also bound proposals and verdicts.
    pub limits: Limits,
}

impl CompletionObserver for ScriptCompletionObserver {
    fn observe(
        &self,
        proposal: CompletionProposal,
        cancellation: CancellationToken,
    ) -> CompletionObserverFuture {
        let callback = self.callback.clone();
        let limits = self.limits.clone();
        Box::pin(async move {
            let proposal =
                serde_json::to_value(proposal).map_err(|_| CompletionObserverError::Invalid)?;
            let argument = StructuredValue::new(proposal, &limits)
                .map_err(|_| CompletionObserverError::Invalid)?;
            let verdict = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(CompletionObserverError::Failed),
                verdict = callback.invoke(argument) => verdict.map_err(|_| CompletionObserverError::Failed)?,
            };
            serde_json::from_value(verdict.into_json())
                .map_err(|_| CompletionObserverError::Invalid)
        })
    }
}

fn capture_schema(schema: &Schema) -> Value {
    match schema {
        Schema::Null => json!({"type":"null"}),
        Schema::Nullable { schema } => json!({"anyOf":[{"type":"null"},capture_schema(schema)]}),
        Schema::Boolean => json!({"type":"boolean"}),
        Schema::Number { min, max } => json!({"type":"number","minimum":min,"maximum":max}),
        Schema::String { max_length } => json!({"type":"string","maxLength":max_length}),
        Schema::Array { max_items, items } => {
            json!({"type":"array","maxItems":max_items,"items":capture_schema(items)})
        }
        Schema::Object {
            properties,
            required,
        } => {
            json!({"type":"object","properties":properties.iter().map(|(name, schema)| (name.clone(), capture_schema(schema))).collect::<BTreeMap<_,_>>(),"required":required,"additionalProperties":false})
        }
    }
}

#[cfg(test)]
#[path = "helper_diagnostic_tests.rs"]
mod helper_diagnostic_tests;

#[cfg(test)]
mod capture_tests {
    use super::*;
    #[tokio::test]
    async fn capture_admission_uses_pinned_normal_policy_and_ceilings_without_building_runtime() {
        use crate::agent_host::{AgentDefinitionCatalog, HostRuntimeBuilder};
        use crate::permission::{PermissionEffect, PermissionLevel, PermissionToolSelector};
        let root = crate::test_support::temp_root();
        let sandbox = crate::sandbox::LocalSandbox::start(root.clone()).await.unwrap();
        let model = waltr_agent::ModelRef::new(waltr_agent::ProviderId::OpenAi, "test", waltr_agent::ModelApi::OpenAiResponses);
        let inference = InferenceSettings { provider:waltr_agent::InferenceProviderSettings {
            key:"test".into(), api:waltr_agent::ModelApi::OpenAiResponses, url:"https://unused.invalid".into(), model_id:"test".into(),
            model_cost:None, headers:Default::default(), oauth:None, reasoning_effort:None,
        }};
        for (permission, level) in [("deny",PermissionLevel::Deny),("ask",PermissionLevel::Ask),("allow",PermissionLevel::Allow)] {
            let source = format!("apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{name: editor}}\nspec:\n  permissions: [{{name: _helper_capture, permission: {permission}}}]\n");
            let catalog = AgentDefinitionCatalog::from_sources(vec![source], "editor").unwrap();
            for (ceiling, identity_denied) in [(PermissionLevel::Allow,false),(PermissionLevel::Ask,false),(PermissionLevel::Deny,false),(PermissionLevel::Allow,true)] {
                for restricted in [false,true] {
                    let factory = Arc::new(AgentSessionRuntimeFactory::new(
                        sandbox.handle(), root.clone(), crate::auth::FileCredentialStore::new(root.join("unused-auth.json")),
                        catalog.resolved("editor").unwrap().clone(),
                    ).with_agent_catalog(catalog.clone()).with_runtime_provider(|_| {
                        panic!("capture admission must not construct providers, authenticate, or connect MCP")
                    }));
                    let policy = PermissionPolicy::new(PermissionLevel::Deny).with_tool_rule(PermissionToolSelector::exact(CAPTURE_TOOL),PermissionEffect::new(level));
                    let runtime = HostRuntimeBuilder::new(model.clone(),Arc::new(waltr_agent::provider::openai::OpenAiResponses::new("unused"))).with_permission_policy(policy);
                    let selection = if restricted { HelperSelection::Restricted {profile:"isolated".into()} } else { HelperSelection::Named {name:"editor".into()} };
                    let sessions = SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
                    let restriction = if identity_denied { PermissionPolicy::new(ceiling).with_tool_rule(PermissionToolSelector::exact("builtin:_helper_capture"),PermissionEffect::deny()) } else { PermissionPolicy::new(ceiling) };
                    let runner = CoordinatorHelperRunner::new(sessions.clone(),factory,inference.clone(),BTreeMap::from([("isolated".into(),RestrictedHelperProfile {
                        runtime,inference:inference.clone(),prompt:"explicit prefix".into(),max_turns:4,
                    })]),BTreeMap::new(),None,"owner".into()).unwrap().with_permission_restriction(restriction);
                    let contract: Contract = serde_json::from_value(json!({"schemaVersion":SCHEMA_VERSION,"schema":{"type":"boolean"}})).unwrap();
                    runner.bind_captures(BTreeMap::from([("arbitrary-resource-capture".into(),crate::routine::definition::CaptureContract {selection,contract})])).await.unwrap();
                    runner.pin_named(vec!["editor".into()]).await.unwrap();
                    let result = runner.preflight_captures().await;
                    if level == PermissionLevel::Deny || ceiling == PermissionLevel::Deny || identity_denied {
                        assert_eq!(result,Err(PortError::Denied));
                    } else {
                        result.unwrap();
                    }
                    assert!(sessions.store().list().await.unwrap().is_empty(),"admission must neither start a helper nor fabricate an approval session");
                }
            }
        }
    }
    #[tokio::test]
    async fn capture_requires_real_successful_exact_tool_evidence_not_a_schema_grant() {
        let contract: Contract = serde_json::from_value(json!({"schemaVersion":SCHEMA_VERSION,"schema":{"type":"object","properties":{"editable":{"type":"boolean"}},"required":["editable"]}})).unwrap();
        let evidence = waltr_agent::CompletionToolEvidence {
            target:None,
            call:waltr_agent::ToolCall::new("real-call",CAPTURE_TOOL,json!({"editable":true})),
            result:waltr_agent::ToolResult::new("real-call",CAPTURE_TOOL,"{\"editable\":true}"),
            item_id:serde_json::from_value(json!("i1")).unwrap(), turn:1,
        };
        let proposal = CompletionProposal { session_id:SessionId::generate(), proposal_id:"candidate".into(), turn:2, response:"done".into(), evidence:vec![evidence.clone()] };
        let accepted = Arc::new(Mutex::new(None));
        let observer = CapturingObserver {completion_bytes:65536,response_bytes:1024,semantic:None,capture:Some(contract),accepted:accepted.clone()};
        for invalid in [
            waltr_agent::CompletionToolEvidence { result:evidence.result.clone().with_error(), ..evidence.clone() },
            waltr_agent::CompletionToolEvidence { call:waltr_agent::ToolCall::new("real-call","foreign",json!({"editable":true})), ..evidence.clone() },
            waltr_agent::CompletionToolEvidence { result:waltr_agent::ToolResult::new("foreign-call",CAPTURE_TOOL,"{\"editable\":true}"), ..evidence.clone() },
        ] {
            let denied = CompletionProposal { evidence:vec![invalid], ..proposal.clone() };
            assert!(matches!(observer.observe(denied,CancellationToken::new()).await.unwrap(),CompletionDecision::Reject {..}));
            assert!(accepted.lock().unwrap().is_none());
        }
        assert!(matches!(observer.observe(proposal,CancellationToken::new()).await.unwrap(),CompletionDecision::Accept));
        let (_, captures) = accepted.lock().unwrap().take().unwrap();
        assert_eq!(captures[0].call_id,"real-call");
        assert_eq!(captures[0].value,json!({"editable":true}));
    }
}
