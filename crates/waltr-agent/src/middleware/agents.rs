//! `AGENTS.md` instructions middleware.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    Result,
    prompt::{
        AGENTS_INSTRUCTION_FILE, AgentsPromptConfig, canonicalize_existing,
        render_agents_instruction,
    },
    template::{TemplateRender, TemplateRuntime, TemplateSource},
    tools::filesystem::{ReadInput, model_path, resolve_existing_path},
};

use super::{
    AgentMiddleware, HookOutcome,
    context::{MiddlewareStateAccess, RequestContext, SystemPromptParts, ToolResultContext},
};

const AGENTS_STATE_NAMESPACE: &str = "agents";

/// Middleware that captures `AGENTS.md` content into durable state and replays it verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentsInstructionsMiddleware {
    prompt_configs: Vec<AgentsPromptConfig>,
    read_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct AgentsState {
    #[serde(default)]
    prompt_captured: bool,
    #[serde(default)]
    loaded: Vec<LoadedInstruction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LoadedInstruction {
    path: PathBuf,
    content: String,
    #[serde(default)]
    prompt: bool,
}

impl AgentsInstructionsMiddleware {
    /// Create middleware from prompt configurations and filesystem read-context roots.
    pub const fn new(prompt_configs: Vec<AgentsPromptConfig>, read_roots: Vec<PathBuf>) -> Self {
        Self {
            prompt_configs,
            read_roots,
        }
    }
}

impl AgentMiddleware for AgentsInstructionsMiddleware {
    async fn build_system_prompt(
        &self,
        ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<HookOutcome> {
        let mut state = ctx
            .state_as::<AgentsState>(AGENTS_STATE_NAMESPACE)?
            .unwrap_or_default();
        if !state.prompt_captured {
            state.prompt_captured = true;
            for config in &self.prompt_configs {
                for instruction in config.instructions() {
                    if state
                        .loaded
                        .iter()
                        .any(|loaded| loaded.path == instruction.path)
                    {
                        continue;
                    }
                    let base_dir = instruction
                        .path
                        .parent()
                        .map_or_else(|| config.root.as_path(), |parent| parent);
                    let mut template_runtime = TemplateRuntime {
                        base_dir,
                        middleware_chain: &ctx.middleware_chain,
                        permission_policy: &ctx.permission_policy,
                        middleware_state: ctx.middleware_state,
                        cancellation_token: ctx.cancellation_token.clone(),
                        session_id: ctx.session_id,
                        turn: ctx.turn,
                        source: TemplateSource::RuntimePrompt,
                    };
                    let content = match template_runtime
                        .render_template(&instruction.content)
                        .await?
                    {
                        TemplateRender::Rendered(content) => content,
                        TemplateRender::Suspend(suspension) => {
                            return Ok(HookOutcome::Suspend(suspension));
                        }
                    };
                    state.loaded.push(LoadedInstruction {
                        path: instruction.path,
                        content,
                        prompt: true,
                    });
                }
            }
            ctx.set_state_as(AGENTS_STATE_NAMESPACE, &state)?;
        }

        for instruction in state.loaded.iter().filter(|loaded| loaded.prompt) {
            prompt.push(instruction.content.clone());
        }
        Ok(HookOutcome::Continue)
    }

    async fn after_tool_call(&self, ctx: &mut ToolResultContext<'_>) -> Result<HookOutcome> {
        if self.read_roots.is_empty() {
            return Ok(HookOutcome::Continue);
        }
        let Some(call) = ctx.call else {
            return Ok(HookOutcome::Continue);
        };
        if call.name != "read" || ctx.result.is_error {
            return Ok(HookOutcome::Continue);
        }
        let Ok(input) = serde_json::from_value::<ReadInput>(call.arguments.clone()) else {
            return Ok(HookOutcome::Continue);
        };

        let mut state = ctx
            .state_as::<AgentsState>(AGENTS_STATE_NAMESPACE)?
            .unwrap_or_default();
        let mut reminders = Vec::new();
        for root in &self.read_roots {
            let Ok(root) = root.canonicalize() else {
                continue;
            };
            let Some(target) = resolve_existing_path(&root, &model_path(&input.file_path)) else {
                continue;
            };
            if !target.is_file()
                || !target.starts_with(&root)
                || target
                    .file_name()
                    .is_some_and(|name| name == AGENTS_INSTRUCTION_FILE)
            {
                continue;
            }
            collect_unloaded_instructions(&root, &target, ctx, &mut state, &mut reminders).await?;
        }

        if !reminders.is_empty() {
            ctx.result.content.push_str("\n\n<system-reminder>\n");
            ctx.result.content.push_str(&reminders.join("\n\n"));
            ctx.result.content.push_str("\n</system-reminder>");
            ctx.set_state_as(AGENTS_STATE_NAMESPACE, &state)?;
        }
        Ok(HookOutcome::Continue)
    }
}

async fn collect_unloaded_instructions(
    root: &Path,
    target: &Path,
    ctx: &mut ToolResultContext<'_>,
    state: &mut AgentsState,
    reminders: &mut Vec<String>,
) -> Result<()> {
    let Some(mut directory) = target.parent().map(Path::to_path_buf) else {
        return Ok(());
    };
    loop {
        if !directory.starts_with(root) {
            break;
        }
        let instruction_path = directory.join(AGENTS_INSTRUCTION_FILE);
        if instruction_path.is_file() {
            let instruction_path = canonicalize_existing(&instruction_path);
            if instruction_path != target
                && !state
                    .loaded
                    .iter()
                    .any(|loaded| loaded.path == instruction_path)
            {
                let instruction_text = fs::read_to_string(&instruction_path)?;
                let content = render_agents_instruction(&instruction_path, &instruction_text);
                let base_dir = instruction_path
                    .parent()
                    .map_or_else(|| root, |parent| parent);
                let mut template_runtime = TemplateRuntime {
                    base_dir,
                    middleware_chain: &ctx.middleware_chain,
                    permission_policy: &ctx.permission_policy,
                    middleware_state: ctx.middleware_state,
                    cancellation_token: ctx.cancellation_token.clone(),
                    session_id: ctx.session_id,
                    turn: ctx.turn,
                    source: TemplateSource::RuntimePrompt,
                };
                let content = match template_runtime.render_template(&content).await? {
                    TemplateRender::Rendered(content) => content,
                    TemplateRender::Suspend(_) => content,
                };
                reminders.push(content.clone());
                state.loaded.push(LoadedInstruction {
                    path: instruction_path,
                    content,
                    prompt: false,
                });
            }
        }
        if directory == root || !directory.pop() {
            break;
        }
    }
    Ok(())
}
