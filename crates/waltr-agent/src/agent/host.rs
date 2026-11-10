//! Host-supplied context and runtime assembly contracts.

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::middleware::{
    AgentMiddleware, HookOutcome,
    context::{
        MiddlewareStateLog, ModelRequestDraft, RequestContext, ToolDraft, ToolResultContext,
    },
};
use crate::{
    AgentRuntime, CompletionTurn, PromptTurn, Result, SessionId, SessionRecord, ToolCall,
    ToolDefinition, ToolResult,
};

/// Borrowed asynchronous host hook. Hooks must honor the supplied cancellation token.
pub type HostContextFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

pub(crate) fn reserved_context_namespace(namespace: &str) -> bool {
    matches!(
        namespace,
        "host-dispatch" | "host-effect-results" | "completion-validation" | "transient-input"
    )
}

/// Narrow durable context access; the host cannot obtain the loop's private state log.
pub trait HostContextState: Send {
    /// The engine owns durable effect association and result settlement.
    fn effects(&self) -> Option<Arc<dyn HostEffectExecutor>> {
        None
    }
    /// Snapshot host context only; execution and admission state is never exposed.
    fn snapshot(&self) -> BTreeMap<String, Value>;
    /// Read a host-owned context namespace.
    fn get(&self, namespace: &str) -> Option<&Value>;
    /// Queue a host-owned context write before the next provider dispatch/result.
    fn set(&mut self, namespace: String, value: Value) -> Result<()>;
}

impl HostContextState for MiddlewareStateLog {
    fn effects(&self) -> Option<Arc<dyn HostEffectExecutor>> {
        self.effects.clone()
    }
    fn snapshot(&self) -> BTreeMap<String, Value> {
        self.host_snapshot()
    }
    fn get(&self, namespace: &str) -> Option<&Value> {
        if reserved_context_namespace(namespace) {
            return None;
        }
        MiddlewareStateLog::get(self, namespace)
    }

    fn set(&mut self, namespace: String, value: Value) -> Result<()> {
        if reserved_context_namespace(&namespace) {
            return Err(crate::LlmError::Protocol {
                provider: "host-context",
                message: "reserved execution namespace".into(),
            });
        }
        MiddlewareStateLog::set(self, namespace, value);
        Ok(())
    }
}

pub type HostEffectFuture<'a> = Pin<Box<dyn Future<Output = Result<HostEffectResult>> + Send + 'a>>;

pub struct HostEffectResult {
    /// An uncertain handler error still carries the context writes committed before it.
    pub result: Result<ToolResult>,
    pub context: BTreeMap<String, Value>,
}

/// A neutral engine journal port, not a host-supplied binding or execution authority.
pub trait HostEffectExecutor: Send + Sync {
    fn execute(
        &self,
        tool: super::AgentTool,
        call: ToolCall,
        turn: u64,
        context: BTreeMap<String, Value>,
    ) -> HostEffectFuture<'_>;
}

impl std::fmt::Debug for dyn HostEffectExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostEffectExecutor")
    }
}

/// Trusted run association and host context, not model-supplied execution metadata.
pub struct HostContext<'a> {
    /// Actual durable session id.
    pub session_id: &'a str,
    /// Current provider turn.
    pub turn: u64,
    /// Cancellation for the owning run.
    pub cancellation: CancellationToken,
    /// Public request metadata, when available; never authorization state.
    pub metadata: Option<&'a BTreeMap<String, Value>>,
    /// Durable host context namespaces.
    pub state: &'a mut dyn HostContextState,
}

/// Provider-neutral mutable request. Contains no provider wire data or host authority.
#[derive(Debug, Clone)]
pub struct HostRequest {
    /// Current user prompt.
    pub prompt: String,
    /// Assembled system context.
    pub system_prompt: Option<String>,
    /// Model-visible declarations; visibility grants no execution permission.
    pub tools: Vec<ToolDefinition>,
    /// Chronological prompt replay.
    pub prompt_turns: Vec<PromptTurn>,
    /// Provider-neutral conversation history.
    pub turns: Vec<CompletionTurn>,
}

/// Host context assembly and result processing, without permission or transport types.
/// Tool execution and interactive waits belong in the supplied tool handler.
pub trait AgentHostContext: Send + Sync {
    fn render_prompt<'a>(
        &'a self,
        _context: HostContext<'a>,
        prompt: &'a str,
        _submission: u64,
        _prompt_index: usize,
    ) -> HostPromptFuture<'a> {
        Box::pin(async move {
            Ok(HostPromptRender {
                prompt: prompt.into(),
                tool_ids: Vec::new(),
            })
        })
    }

    fn search_templates<'a>(
        &'a self,
        _context: HostContext<'a>,
        _request: &'a crate::SessionTemplateSearchRequest,
    ) -> HostSearchFuture<'a> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn finish_prompt(&self, _state: &mut dyn HostContextState, _tool_ids: &[String]) -> Result<()> {
        Ok(())
    }
    /// Assemble context and declarations immediately before a provider request.
    fn prepare_request<'a>(
        &'a self,
        _context: HostContext<'a>,
        _request: &'a mut HostRequest,
    ) -> HostContextFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    /// Process a completed result. Mutation drops invocation-local presentation metadata.
    fn process_result<'a>(
        &'a self,
        _context: HostContext<'a>,
        _call: Option<&'a ToolCall>,
        _result: &'a mut ToolResult,
    ) -> HostContextFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

/// Host assembly result, using the ordinary runtime and model loop.
pub type HostRuntimeFuture<'a> = Pin<Box<dyn Future<Output = Result<AgentRuntime>> + Send + 'a>>;

/// Host lifecycle completion, including joined connection/worker retirement.
pub type HostRuntimeRetireFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// Host-owned configuration/audit changes at the coordinator's serialized run boundary.
pub struct HostConfigurationUpdate {
    pub configuration: Option<Value>,
    pub timeline: Vec<crate::PendingTimelineEvent>,
}

/// Neutral prompt-preprocessing result; execution identities remain opaque to the loop.
pub struct HostPromptRender {
    pub prompt: String,
    pub tool_ids: Vec<String>,
}

pub type HostPromptFuture<'a> = Pin<Box<dyn Future<Output = Result<HostPromptRender>> + Send + 'a>>;
pub type HostSearchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<crate::SessionTemplateOption>>> + Send + 'a>>;

/// Runtime ownership port for the session coordinator. Hosts interpret configuration.
pub trait AgentHostRuntimeFactory: Send + Sync {
    fn retire_all(&self) -> HostRuntimeRetireFuture<'_> {
        Box::pin(async { Ok(()) })
    }
    fn configuration_for_cycle(
        &self,
        _record: &SessionRecord,
    ) -> Result<Option<HostConfigurationUpdate>> {
        Ok(None)
    }
    /// Assemble the current provider, host tools, and context for this durable record.
    fn build<'a>(&'a self, record: &'a SessionRecord) -> HostRuntimeFuture<'a>;

    /// Assemble read-only discovery context without requiring every optional host source.
    fn build_for_discovery<'a>(&'a self, record: &'a SessionRecord) -> HostRuntimeFuture<'a> {
        self.build(record)
    }

    /// Stop and join host-owned resources after the coordinator has stopped the run.
    fn retire<'a>(&'a self, _session_id: &'a SessionId) -> HostRuntimeRetireFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone)]
pub(super) struct HostContextMiddleware(pub Arc<dyn AgentHostContext>);

impl AgentMiddleware for HostContextMiddleware {
    async fn before_provider_request(
        &self,
        ctx: &mut RequestContext<'_>,
        draft: &mut ModelRequestDraft,
    ) -> Result<HookOutcome> {
        let mut request = HostRequest {
            prompt: draft.prompt.clone(),
            system_prompt: draft.system_prompt.clone(),
            tools: draft.tools.clone(),
            prompt_turns: draft.prompt_turns.clone(),
            turns: draft.turns.clone(),
        };
        self.0
            .prepare_request(
                HostContext {
                    session_id: ctx.session_id,
                    turn: ctx.turn,
                    cancellation: ctx.cancellation_token.clone(),
                    metadata: Some(&ctx.session_metadata_values),
                    state: ctx.middleware_state,
                },
                &mut request,
            )
            .await?;
        draft.prompt = request.prompt;
        draft.system_prompt = request.system_prompt;
        draft.tools = request.tools;
        draft.prompt_turns = request.prompt_turns;
        draft.turns = request.turns;
        Ok(HookOutcome::Continue)
    }

    async fn after_tool_call(&self, ctx: &mut ToolResultContext<'_>) -> Result<HookOutcome> {
        self.0
            .process_result(
                HostContext {
                    session_id: ctx.session_id,
                    turn: ctx.turn,
                    cancellation: ctx.cancellation_token.clone(),
                    metadata: None,
                    state: ctx.middleware_state,
                },
                ctx.call,
                ctx.result,
            )
            .await?;
        Ok(HookOutcome::Continue)
    }
}

#[derive(Clone)]
pub(super) struct HostToolRegistry(pub Vec<super::AgentTool>);

impl AgentMiddleware for HostToolRegistry {
    async fn list_tools(&self, _ctx: &mut RequestContext<'_>, tools: &mut ToolDraft) -> Result<()> {
        for tool in &self.0 {
            if tool.model_exposure {
                tools.push(tool.definition().clone());
            }
        }
        Ok(())
    }
}
