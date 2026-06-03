//! Agent Skills resources.

use std::{fs, path::PathBuf};

use crate::Result;

use super::definition::SkillDefinition;

const RESOURCE_DIRECTORIES: &[&str] = &["scripts", "references", "assets"];

pub(super) fn collect_resource_paths(skill: &SkillDefinition) -> Result<Vec<PathBuf>> {
    let mut resources = Vec::new();
    for directory in RESOURCE_DIRECTORIES {
        let resource_dir = skill.skill_dir.join(directory);
        if resource_dir.is_dir() {
            collect_files_relative(&skill.skill_dir, &resource_dir, &mut resources)?;
        }
    }
    resources.sort();
    Ok(resources)
}

fn collect_files_relative(
    root: &std::path::Path,
    directory: &std::path::Path,
    resources: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files_relative(root, &path, resources)?;
        } else if path.is_file() {
            resources.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
    Ok(())
}
