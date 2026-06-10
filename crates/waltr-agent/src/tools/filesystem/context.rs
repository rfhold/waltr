//! Filesystem dynamic context helpers.

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    AgentToolContext, Result,
    prompt::{AGENTS_INSTRUCTION_FILE, canonicalize_existing, render_agents_instruction},
    tools::skills::{render_skills_reminder, reveal_skills_root},
};

use super::FilesystemConfig;

impl FilesystemConfig {
    // TODO(agent-runtime): Replace tool-local AGENTS reads with read-hook middleware state.
    pub(super) fn append_agents_read_context(
        &self,
        target: &Path,
        context: &mut AgentToolContext,
        output: &mut String,
    ) -> Result<()> {
        let root = self.root.canonicalize()?;
        if !target.starts_with(&root)
            || target
                .file_name()
                .is_some_and(|name| name == AGENTS_INSTRUCTION_FILE)
        {
            return Ok(());
        }

        let Some(mut directory) = target.parent().map(Path::to_path_buf) else {
            return Ok(());
        };
        let mut reminders = Vec::new();
        loop {
            if !directory.starts_with(&root) {
                break;
            }
            let instruction_path = directory.join(AGENTS_INSTRUCTION_FILE);
            if instruction_path.is_file() {
                let instruction_path = canonicalize_existing(&instruction_path);
                if instruction_path != target
                    && !context.agents_system_paths.contains(&instruction_path)
                    && !context.agents_loaded_paths.contains(&instruction_path)
                {
                    let instruction_text = fs::read_to_string(&instruction_path)?;
                    reminders.push(render_agents_instruction(
                        &instruction_path,
                        &instruction_text,
                    ));
                    context.agents_loaded_paths.insert(instruction_path);
                }
            }
            if directory == root || !directory.pop() {
                break;
            }
        }

        if !reminders.is_empty() {
            output.push_str("\n\n<system-reminder>\n");
            output.push_str(&reminders.join("\n\n"));
            output.push_str("\n</system-reminder>");
        }
        Ok(())
    }

    pub(super) fn append_skills_context(
        &self,
        target: &Path,
        walk_children: bool,
        context: &mut AgentToolContext,
        output: &mut String,
    ) -> Result<()> {
        // TODO(agent-runtime): Replace tool-result skill reminders with middleware state updates.
        if !context.skills_enabled {
            return Ok(());
        }
        let root = self.root.canonicalize()?;
        if !target.starts_with(&root) {
            return Ok(());
        }

        let mut reminders = Vec::new();
        let start = if target.is_dir() {
            target.to_path_buf()
        } else {
            target.parent().map_or_else(PathBuf::new, Path::to_path_buf)
        };
        Self::collect_skills_context_for_ancestors(&root, &start, context, &mut reminders);
        if walk_children && target.is_dir() {
            Self::collect_skills_context_for_tree(target, context, &mut reminders)?;
        }

        if !reminders.is_empty() {
            output.push_str("\n\n<system-reminder>\n");
            output.push_str(&reminders.join("\n\n"));
            output.push_str("\n</system-reminder>");
        }
        Ok(())
    }

    fn collect_skills_context_for_ancestors(
        root: &Path,
        start: &Path,
        context: &mut AgentToolContext,
        reminders: &mut Vec<String>,
    ) {
        let mut directory = start.to_path_buf();
        loop {
            if !directory.starts_with(root) {
                break;
            }
            Self::push_skills_reminder(&directory, context, reminders);
            if directory == root || !directory.pop() {
                break;
            }
        }
    }

    fn collect_skills_context_for_tree(
        directory: &Path,
        context: &mut AgentToolContext,
        reminders: &mut Vec<String>,
    ) -> Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.file_name().is_some_and(|name| name == ".agents") {
                if let Some(skills) = reveal_skills_root(context, directory)
                    && let Some(reminder) = render_skills_reminder(&skills, directory)
                {
                    reminders.push(reminder);
                }
                continue;
            }
            Self::collect_skills_context_for_tree(&path, context, reminders)?;
        }
        Ok(())
    }

    fn push_skills_reminder(
        root: &Path,
        context: &mut AgentToolContext,
        reminders: &mut Vec<String>,
    ) {
        if let Some(skills) = reveal_skills_root(context, root)
            && let Some(reminder) = render_skills_reminder(&skills, root)
        {
            reminders.push(reminder);
        }
    }
}
