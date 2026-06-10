//! Run-time agent context state.

use std::{collections::HashSet, path::PathBuf};

use crate::SkillDefinition;

/// Mutable context shared across tool executions in one agent run.
// TODO(agent-runtime): Persist this as AgentSession context across suspends/resumes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentToolContext {
    /// `AGENTS.md` files already loaded into the static system prompt.
    pub agents_system_paths: HashSet<PathBuf>,
    /// `AGENTS.md` files dynamically loaded by tools in this run.
    pub agents_loaded_paths: HashSet<PathBuf>,
    /// Agent Skills visible to the skill loading tool in this run.
    // TODO(agent-runtime): Track visible/loaded skills durably with session discovery state.
    pub visible_skills: Vec<SkillDefinition>,
    /// Whether Agent Skills support is enabled for this run.
    pub skills_enabled: bool,
    /// `.agents/skills` roots already revealed by tools in this run.
    pub skills_revealed_roots: HashSet<PathBuf>,
    /// `SKILL.md` files already loaded by the skill loading tool in this run.
    pub skills_loaded_paths: HashSet<PathBuf>,
}
