//! Agent Skills load tool.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use crate::{AgentToolContext, Result, ToolCall, ToolDefinition};

use super::{
    definition::{SkillDefinition, canonicalize_or_clone, skills_error},
    render::render_loaded_skill,
};

pub(super) fn load_skill_definition() -> ToolDefinition {
    ToolDefinition::new(
        "load_skill",
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

#[derive(Debug, Deserialize)]
struct LoadSkillInput {
    name: String,
    root: Option<String>,
}

pub(super) fn load_skill(call: &ToolCall, context: &mut AgentToolContext) -> Result<String> {
    let input: LoadSkillInput = serde_json::from_value(call.arguments.clone())?;
    let skill = resolve_skill(&input, &context.visible_skills)?.clone();
    if !context.skills_loaded_paths.insert(skill.skill_path.clone()) {
        return Ok(format!(
            "Skill '{}' from root {} is already loaded.",
            skill.name,
            skill.root.display()
        ));
    }
    render_loaded_skill(&skill)
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
