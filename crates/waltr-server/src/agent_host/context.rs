use std::path::PathBuf;

use crate::permission::PermissionPolicy;
use waltr_agent::{
    AgentHostContext, HostContext, HostContextFuture, HostRequest, LlmError, ToolCall, ToolResult,
};

use super::{
    dispatcher::CapabilityDispatcher,
    middleware::{
        HookOutcome,
        chain::MiddlewareChain,
        context::{
            MiddlewareStateLog, ModelRequestDraft, RequestContext, SystemPromptParts, ToolDraft,
            ToolResultContext,
        },
    },
    sandbox_gateway::SandboxHandle,
};

/// Server context adapters run in the generic loop's durable context transaction.
#[derive(Clone)]
pub struct ServerHostContext {
    pub(crate) chain: MiddlewareChain,
    pub(crate) policy: PermissionPolicy,
    pub(crate) template_base_dir: Option<PathBuf>,
    pub(crate) sandbox: Option<SandboxHandle>,
}

impl ServerHostContext {
    pub(crate) fn new(
        chain: MiddlewareChain,
        policy: PermissionPolicy,
        template_base_dir: Option<PathBuf>,
        sandbox: Option<SandboxHandle>,
        dispatcher: CapabilityDispatcher,
    ) -> Self {
        Self {
            chain: chain.with_dispatcher(dispatcher),
            policy,
            template_base_dir,
            sandbox,
        }
    }
}

fn require_continue(outcome: HookOutcome) -> waltr_agent::Result<()> {
    match outcome {
        HookOutcome::Continue => Ok(()),
        other => Err(LlmError::Protocol {
            provider: "server-context",
            message: format!("context hook requires host interaction: {other:?}"),
        }),
    }
}

impl AgentHostContext for ServerHostContext {
    fn render_prompt<'a>(
        &'a self,
        host: HostContext<'a>,
        prompt: &'a str,
        submission: u64,
        prompt_index: usize,
    ) -> waltr_agent::HostPromptFuture<'a> {
        Box::pin(async move {
            let mut state = MiddlewareStateLog::from_host(host.state);
            let root = self
                .template_base_dir
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new(""));
            let mut runtime = super::template::TemplateRuntime::new(
                root,
                &mut state,
                super::template::TemplateRunContext {
                    middleware_chain: &self.chain,
                    permission_policy: &self.policy,
                    cancellation_token: host.cancellation,
                    session_id: host.session_id,
                    turn: host.turn,
                    sandbox: self.sandbox.as_ref(),
                },
                super::template::TemplateOrigin::UserPrompt {
                    submission,
                    prompt_index,
                },
            );
            let output = runtime.render_template(prompt).await;
            let tool_ids = runtime.rendered_tool_ids().to_vec();
            drop(runtime);
            state.flush_host(host.state)?;
            let output = output?;
            let prompt = match output {
                super::template::TemplateRender::Rendered(prompt) => prompt,
            };
            state.flush_host(host.state)?;
            Ok(waltr_agent::HostPromptRender { prompt, tool_ids })
        })
    }

    fn search_templates<'a>(
        &'a self,
        host: HostContext<'a>,
        request: &'a waltr_agent::SessionTemplateSearchRequest,
    ) -> waltr_agent::HostSearchFuture<'a> {
        Box::pin(async move {
            let state = MiddlewareStateLog::from_host(host.state);
            let root = self
                .template_base_dir
                .as_deref()
                .ok_or_else(|| LlmError::Protocol {
                    provider: "server-context",
                    message: "template workspace root is unavailable".into(),
                })?;
            super::resource::search_session_templates_with_state(root, self, request, Some(&state))
                .await
        })
    }

    fn prepare_request<'a>(
        &'a self,
        host: HostContext<'a>,
        request: &'a mut HostRequest,
    ) -> HostContextFuture<'a> {
        Box::pin(async move {
            let mut state = MiddlewareStateLog::from_host(host.state);
            let outcome = async {
                let mut context = RequestContext {
                    middleware_state: &mut state,
                    middleware_chain: self.chain.clone(),
                    permission_policy: self.policy.clone(),
                    cancellation_token: host.cancellation,
                    session_id: host.session_id,
                    turn: host.turn,
                };
                let mut prompt = SystemPromptParts::default();
                require_continue(
                    self.chain
                        .build_system_prompt(&mut context, &mut prompt)
                        .await?,
                )?;
                let mut tools = ToolDraft::default();
                self.chain.list_tools(&mut context, &mut tools).await?;
                self.chain
                    .edit_tool_metadata(&mut context, &mut tools)
                    .await?;
                let system_prompt = request
                    .system_prompt
                    .iter()
                    .cloned()
                    .chain(prompt.parts)
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let mut draft = ModelRequestDraft {
                    prompt: request.prompt.clone(),
                    system_prompt: (!system_prompt.is_empty()).then_some(system_prompt),
                    tools: tools.tools,
                    prompt_turns: request.prompt_turns.clone(),
                    turns: request.turns.clone(),
                };
                require_continue(
                    self.chain
                        .before_provider_request(&mut context, &mut draft)
                        .await?,
                )?;
                request.prompt = draft.prompt;
                request.system_prompt = draft.system_prompt;
                request.tools = draft.tools;
                request.prompt_turns = draft.prompt_turns;
                request.turns = draft.turns;
                Ok(())
            }
            .await;
            state.flush_host(host.state)?;
            outcome
        })
    }

    fn process_result<'a>(
        &'a self,
        host: HostContext<'a>,
        call: Option<&'a ToolCall>,
        result: &'a mut ToolResult,
    ) -> HostContextFuture<'a> {
        Box::pin(async move {
            let mut state = MiddlewareStateLog::from_host(host.state);
            let mut context = ToolResultContext {
                call,
                result,
                middleware_state: &mut state,
                middleware_chain: self.chain.clone(),
                permission_policy: self.policy.clone(),
                cancellation_token: host.cancellation,
                session_id: host.session_id,
                turn: host.turn,
            };
            let outcome = self
                .chain
                .after_tool_call(&mut context)
                .await
                .and_then(require_continue);
            state.flush_host(host.state)?;
            outcome
        })
    }
}
