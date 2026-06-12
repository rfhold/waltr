//! Agent Skills middleware.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    Result, ToolDefinition, ToolResult,
    tools::{
        filesystem::{GlobInput, GrepInput, ReadInput, model_path, resolve_existing_path},
        skills::{
            SKILLS_DIRECTORY, SkillDefinition,
            definition::{canonicalize_or_clone, skills_error},
            discover_skills_dir,
            render::render_loaded_skill,
            render_skills_catalog, render_skills_reminder,
        },
    },
};

use super::{
    AgentMiddleware, HookOutcome,
    context::{
        MiddlewareStateAccess, RequestContext, SystemPromptParts, ToolCallContext, ToolDraft,
        ToolResultContext,
    },
};

const SKILLS_STATE_NAMESPACE: &str = "skills";

/// Provider-facing skill loading tool name.
const LOAD_SKILL_TOOL_NAME: &str = "load_skill";

/// Middleware that owns the skill catalog, the `load_skill` tool, and loaded-skill replay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsMiddleware {
    initial_catalog: Vec<SkillDefinition>,
    reveal_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SkillsState {
    #[serde(default)]
    catalog: Vec<SkillDefinition>,
    #[serde(default)]
    revealed_roots: Vec<PathBuf>,
    #[serde(default)]
    loaded: Vec<LoadedSkill>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LoadedSkill {
    name: String,
    path: PathBuf,
    content: String,
}

#[derive(Debug, Deserialize)]
struct LoadSkillInput {
    name: String,
    root: Option<String>,
}

impl SkillsMiddleware {
    /// Create middleware from an initial skill catalog and filesystem reveal roots.
    pub const fn new(initial_catalog: Vec<SkillDefinition>, reveal_roots: Vec<PathBuf>) -> Self {
        Self {
            initial_catalog,
            reveal_roots,
        }
    }
}

impl AgentMiddleware for SkillsMiddleware {
    async fn build_system_prompt(
        &self,
        ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<()> {
        let state = if let Some(state) = ctx.state_as::<SkillsState>(SKILLS_STATE_NAMESPACE)? {
            state
        } else {
            let mut revealed_roots = Vec::new();
            for skill in &self.initial_catalog {
                if !revealed_roots.contains(&skill.skills_dir) {
                    revealed_roots.push(skill.skills_dir.clone());
                }
            }
            let state = SkillsState {
                catalog: self.initial_catalog.clone(),
                revealed_roots,
                loaded: Vec::new(),
            };
            ctx.set_state_as(SKILLS_STATE_NAMESPACE, &state)?;
            state
        };

        if let Some(catalog) = render_skills_catalog(&state.catalog, "Available skills:") {
            prompt.push(catalog);
        }
        Ok(())
    }

    async fn list_tools(&self, _ctx: &mut RequestContext<'_>, tools: &mut ToolDraft) -> Result<()> {
        tools.push(load_skill_definition());
        Ok(())
    }

    async fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<HookOutcome> {
        if ctx.call.name != LOAD_SKILL_TOOL_NAME {
            return Ok(HookOutcome::Continue);
        }

        let input: LoadSkillInput = serde_json::from_value(ctx.call.arguments.clone())?;
        let mut state = ctx
            .state_as::<SkillsState>(SKILLS_STATE_NAMESPACE)?
            .unwrap_or_default();
        let skill = resolve_skill(&input, &state.catalog)?.clone();

        let content = if state
            .loaded
            .iter()
            .any(|loaded| loaded.path == skill.skill_path)
        {
            format!(
                "Skill '{}' from root {} is already loaded.",
                skill.name,
                skill.root.display()
            )
        } else {
            let content = render_loaded_skill(&skill)?;
            state.loaded.push(LoadedSkill {
                name: skill.name,
                path: skill.skill_path,
                content: content.clone(),
            });
            ctx.set_state_as(SKILLS_STATE_NAMESPACE, &state)?;
            content
        };

        Ok(HookOutcome::ReplaceToolResult(ToolResult::new(
            ctx.call.id.clone(),
            ctx.call.name.clone(),
            content,
        )))
    }

    async fn after_tool_call(&self, ctx: &mut ToolResultContext<'_>) -> Result<HookOutcome> {
        if self.reveal_roots.is_empty() {
            return Ok(HookOutcome::Continue);
        }
        let Some(call) = ctx.call else {
            return Ok(HookOutcome::Continue);
        };
        if ctx.result.content.starts_with("Tool error:") {
            return Ok(HookOutcome::Continue);
        }
        let (target_path, walk_children) = match call.name.as_str() {
            "read" => {
                let Ok(input) = serde_json::from_value::<ReadInput>(call.arguments.clone()) else {
                    return Ok(HookOutcome::Continue);
                };
                (input.file_path, false)
            }
            "glob" => {
                let Ok(input) = serde_json::from_value::<GlobInput>(call.arguments.clone()) else {
                    return Ok(HookOutcome::Continue);
                };
                (input.path.unwrap_or_else(|| ".".to_owned()), true)
            }
            "grep" => {
                let Ok(input) = serde_json::from_value::<GrepInput>(call.arguments.clone()) else {
                    return Ok(HookOutcome::Continue);
                };
                (input.path.unwrap_or_else(|| ".".to_owned()), true)
            }
            _ => return Ok(HookOutcome::Continue),
        };

        let mut state = ctx
            .state_as::<SkillsState>(SKILLS_STATE_NAMESPACE)?
            .unwrap_or_default();
        let initial_state = state.clone();
        let mut reminders = Vec::new();
        for root in &self.reveal_roots {
            let Ok(root) = root.canonicalize() else {
                continue;
            };
            let Some(target) = resolve_existing_path(&root, &model_path(&target_path)) else {
                continue;
            };
            if !target.starts_with(&root) {
                continue;
            }
            let start = if target.is_dir() {
                target.clone()
            } else {
                target.parent().map_or_else(PathBuf::new, Path::to_path_buf)
            };
            collect_reveals_for_ancestors(&root, &start, &mut state, &mut reminders);
            if walk_children && target.is_dir() {
                collect_reveals_for_tree(&target, &mut state, &mut reminders)?;
            }
        }

        if !reminders.is_empty() {
            ctx.result.content.push_str("\n\n<system-reminder>\n");
            ctx.result.content.push_str(&reminders.join("\n\n"));
            ctx.result.content.push_str("\n</system-reminder>");
        }
        if state != initial_state {
            ctx.set_state_as(SKILLS_STATE_NAMESPACE, &state)?;
        }
        Ok(HookOutcome::Continue)
    }
}

fn collect_reveals_for_ancestors(
    root: &Path,
    start: &Path,
    state: &mut SkillsState,
    reminders: &mut Vec<String>,
) {
    let mut directory = start.to_path_buf();
    loop {
        if !directory.starts_with(root) {
            break;
        }
        push_reveal_reminder(&directory, state, reminders);
        if directory == root || !directory.pop() {
            break;
        }
    }
}

fn collect_reveals_for_tree(
    directory: &Path,
    state: &mut SkillsState,
    reminders: &mut Vec<String>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().is_some_and(|name| name == ".agents") {
            push_reveal_reminder(directory, state, reminders);
            continue;
        }
        collect_reveals_for_tree(&path, state, reminders)?;
    }
    Ok(())
}

fn push_reveal_reminder(root: &Path, state: &mut SkillsState, reminders: &mut Vec<String>) {
    if let Some(added) = reveal_skills_root(state, root)
        && let Some(reminder) = render_skills_reminder(&added, root)
    {
        reminders.push(reminder);
    }
}

fn reveal_skills_root(state: &mut SkillsState, root: &Path) -> Option<Vec<SkillDefinition>> {
    let root = canonicalize_or_clone(root);
    let skills_dir = canonicalize_or_clone(&root.join(SKILLS_DIRECTORY));
    if !skills_dir.is_dir() || state.revealed_roots.contains(&skills_dir) {
        return None;
    }
    state.revealed_roots.push(skills_dir.clone());
    let mut added = Vec::new();
    for skill in discover_skills_dir(&root, &skills_dir) {
        if !state
            .catalog
            .iter()
            .any(|visible| visible.skill_path == skill.skill_path)
        {
            state.catalog.push(skill.clone());
            added.push(skill);
        }
    }
    if added.is_empty() { None } else { Some(added) }
}

fn load_skill_definition() -> ToolDefinition {
    ToolDefinition::new(
        LOAD_SKILL_TOOL_NAME,
        "Load full instructions for one visible Agent Skill. Use this before following a skill's instructions.",
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name from the available skills catalog"
                },
                "root": {
                    "type": "string",
                    "description": "Required when more than one visible skill has this name"
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    )
}

fn resolve_skill<'a>(
    input: &LoadSkillInput,
    skills: &'a [SkillDefinition],
) -> Result<&'a SkillDefinition> {
    let matches = skills
        .iter()
        .filter(|skill| skill.name == input.name)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(skills_error(format!(
            "skill '{}' is not visible; use only skills from the available skills catalog",
            input.name
        ))),
        [skill] if input.root.is_none() => Ok(skill),
        [_skill] => matches
            .into_iter()
            .find(|skill| {
                input
                    .root
                    .as_deref()
                    .is_some_and(|root| root_matches(root, &skill.root))
            })
            .ok_or_else(|| {
                skills_error(format!(
                    "skill '{}' is not visible from that root",
                    input.name
                ))
            }),
        _ => {
            let Some(root) = input.root.as_deref() else {
                let roots = matches
                    .iter()
                    .map(|skill| skill.root.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(skills_error(format!(
                    "skill '{}' is ambiguous; pass one of these roots: {roots}",
                    input.name
                )));
            };
            matches
                .into_iter()
                .find(|skill| root_matches(root, &skill.root))
                .ok_or_else(|| {
                    skills_error(format!(
                        "skill '{}' is not visible from root {root}",
                        input.name
                    ))
                })
        }
    }
}

fn root_matches(input: &str, root: &Path) -> bool {
    let input_path = PathBuf::from(input);
    canonicalize_or_clone(&input_path) == root
}
