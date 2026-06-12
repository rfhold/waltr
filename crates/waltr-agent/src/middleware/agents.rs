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
    ) -> Result<()> {
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
                    state.loaded.push(LoadedInstruction {
                        path: instruction.path,
                        content: instruction.content,
                        prompt: true,
                    });
                }
            }
            ctx.set_state_as(AGENTS_STATE_NAMESPACE, &state)?;
        }

        for instruction in state.loaded.iter().filter(|loaded| loaded.prompt) {
            prompt.push(instruction.content.clone());
        }
        Ok(())
    }

    async fn after_tool_call(&self, ctx: &mut ToolResultContext<'_>) -> Result<HookOutcome> {
        if self.read_roots.is_empty() {
            return Ok(HookOutcome::Continue);
        }
        let Some(call) = ctx.call else {
            return Ok(HookOutcome::Continue);
        };
        if call.name != "read" || ctx.result.content.starts_with("Tool error:") {
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
            collect_unloaded_instructions(&root, &target, &mut state, &mut reminders)?;
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

fn collect_unloaded_instructions(
    root: &Path,
    target: &Path,
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
