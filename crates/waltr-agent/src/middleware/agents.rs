//! `AGENTS.md` instructions middleware.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};

use crate::{
    Result,
    prompt::{AGENTS_INSTRUCTION_FILE, AgentsPromptConfig, render_agents_instruction},
    sandbox_gateway::SandboxHandle,
    template::{
        TemplateOrigin, TemplateRender, TemplateRunContext, TemplateRuntime,
        clear_sandbox_template_results,
    },
    tools::filesystem::ReadInput,
};

use super::{
    AgentMiddleware, HookOutcome,
    context::{MiddlewareStateAccess, RequestContext, SystemPromptParts, ToolResultContext},
};

const AGENTS_STATE_NAMESPACE: &str = "agents";

/// Middleware that captures `AGENTS.md` content into durable state and replays it verbatim.
#[derive(Debug, Clone, Default)]
pub struct AgentsInstructionsMiddleware {
    prompt_configs: Vec<AgentsPromptConfig>,
    read_roots: Vec<PathBuf>,
    sandbox: Option<SandboxHandle>,
    // This is process-local recursion state, never part of durable instruction state.
    rendering: Arc<Mutex<HashSet<(String, PathBuf)>>>,
}

struct InstructionRenderGuard {
    rendering: Arc<Mutex<HashSet<(String, PathBuf)>>>,
    key: (String, PathBuf),
}

impl InstructionRenderGuard {
    fn enter(
        rendering: &Arc<Mutex<HashSet<(String, PathBuf)>>>,
        session: &str,
        path: &Path,
    ) -> Option<Self> {
        let key = (session.to_owned(), path.to_path_buf());
        rendering.lock().unwrap().insert(key.clone()).then(|| Self {
            rendering: Arc::clone(rendering),
            key,
        })
    }
}

impl Drop for InstructionRenderGuard {
    fn drop(&mut self) {
        self.rendering.lock().unwrap().remove(&self.key);
    }
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
    pub fn new(prompt_configs: Vec<AgentsPromptConfig>, read_roots: Vec<PathBuf>) -> Self {
        Self {
            prompt_configs,
            read_roots,
            sandbox: None,
            rendering: Arc::default(),
        }
    }

    pub(crate) fn with_sandbox(mut self, sandbox: SandboxHandle) -> Self {
        self.sandbox = Some(sandbox);
        self
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
            let sandbox = self
                .sandbox
                .as_ref()
                .ok_or_else(|| crate::LlmError::Protocol {
                    provider: "sandbox",
                    message: "AGENTS.md requires sandbox".into(),
                })?;
            state.prompt_captured = true;
            for config in &self.prompt_configs {
                let root = sandbox.resolve(&config.root.to_string_lossy()).await?;
                let path = PathBuf::from(root.canonical_path).join(&config.filename);
                if let Some(text) = read_instruction(sandbox, &path).await? {
                    let instruction = crate::AgentsInstruction {
                        path: path.clone(),
                        content: render_agents_instruction(&path, &text),
                    };
                    if state
                        .loaded
                        .iter()
                        .any(|loaded| loaded.path == instruction.path)
                    {
                        continue;
                    }
                    let Some(_rendering) = InstructionRenderGuard::enter(
                        &self.rendering,
                        ctx.session_id,
                        &instruction.path,
                    ) else {
                        continue;
                    };
                    let base_dir = instruction
                        .path
                        .parent()
                        .map_or_else(|| config.root.as_path(), |parent| parent);
                    let mut template_runtime = TemplateRuntime::new(
                        base_dir,
                        ctx.middleware_state,
                        TemplateRunContext {
                            middleware_chain: &ctx.middleware_chain,
                            permission_policy: &ctx.permission_policy,
                            cancellation_token: ctx.cancellation_token.clone(),
                            session_id: ctx.session_id,
                            turn: ctx.turn,
                            sandbox: Some(sandbox),
                        },
                        TemplateOrigin::Instruction {
                            path: &instruction.path,
                        },
                    );
                    let content = match template_runtime
                        .render_template(&instruction.content)
                        .await?
                    {
                        TemplateRender::Rendered(content) => content,
                        TemplateRender::Suspend(suspension) => {
                            return Ok(HookOutcome::Suspend(suspension));
                        }
                    };
                    let tool_ids = template_runtime.rendered_tool_ids().to_vec();
                    clear_sandbox_template_results(template_runtime.middleware_state, &tool_ids)?;
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
        let sandbox = self
            .sandbox
            .as_ref()
            .ok_or_else(|| crate::LlmError::Protocol {
                provider: "sandbox",
                message: "AGENTS.md requires sandbox".into(),
            })?;
        for root in &self.read_roots {
            let model_path = model_path(&input.file_path);
            let target = if model_path.is_absolute() {
                model_path
            } else {
                root.join(model_path)
            };
            let target = match sandbox.resolve(&target.to_string_lossy()).await {
                Ok(data) if data.path_kind == "file" => PathBuf::from(data.canonical_path),
                Err(_) => continue,
                Ok(_) => continue,
            };
            let root = PathBuf::from(
                sandbox
                    .resolve(&root.to_string_lossy())
                    .await?
                    .canonical_path,
            );
            if !target.starts_with(&root)
                || target
                    .file_name()
                    .is_some_and(|name| name == AGENTS_INSTRUCTION_FILE)
            {
                continue;
            }
            collect_unloaded_instructions(
                &root,
                &target,
                sandbox,
                &self.rendering,
                ctx,
                &mut state,
                &mut reminders,
            )
            .await?;
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

fn model_path(path: &str) -> PathBuf {
    let mut path = path.trim();
    if let Some(start) = path.find("<path>") {
        let after = &path[start + "<path>".len()..];
        if let Some(end) = after.find("</path>") {
            path = &after[..end];
        }
    }
    PathBuf::from(
        path.trim()
            .trim_matches(|character| matches!(character, '`' | '"' | '\''))
            .trim_end_matches(':'),
    )
}

async fn collect_unloaded_instructions(
    root: &Path,
    target: &Path,
    sandbox: &SandboxHandle,
    rendering: &Arc<Mutex<HashSet<(String, PathBuf)>>>,
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
        if let Some(instruction_text) = read_instruction(sandbox, &instruction_path).await? {
            if instruction_path != target
                && !state
                    .loaded
                    .iter()
                    .any(|loaded| loaded.path == instruction_path)
                && let Some(_rendering) =
                    InstructionRenderGuard::enter(rendering, ctx.session_id, &instruction_path)
            {
                let content = render_agents_instruction(&instruction_path, &instruction_text);
                let base_dir = instruction_path
                    .parent()
                    .map_or_else(|| root, |parent| parent);
                let mut template_runtime = TemplateRuntime::new(
                    base_dir,
                    ctx.middleware_state,
                    TemplateRunContext {
                        middleware_chain: &ctx.middleware_chain,
                        permission_policy: &ctx.permission_policy,
                        cancellation_token: ctx.cancellation_token.clone(),
                        session_id: ctx.session_id,
                        turn: ctx.turn,
                        sandbox: Some(sandbox),
                    },
                    TemplateOrigin::Instruction {
                        path: &instruction_path,
                    },
                );
                let content = match template_runtime.render_template(&content).await? {
                    TemplateRender::Rendered(content) => content,
                    TemplateRender::Suspend(_) => content,
                };
                let tool_ids = template_runtime.rendered_tool_ids().to_vec();
                clear_sandbox_template_results(template_runtime.middleware_state, &tool_ids)?;
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

async fn read_instruction(sandbox: &SandboxHandle, path: &Path) -> Result<Option<String>> {
    match sandbox.read_file(&path.to_string_lossy()).await {
        Ok(data) => Ok(Some(data.content)),
        Err(error)
            if error.to_string().contains("No such file")
                || error.to_string().contains("not found") =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
