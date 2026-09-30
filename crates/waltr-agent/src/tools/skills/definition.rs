//! Agent Skill definition and frontmatter parsing.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Directory under a filesystem root that contains Agent Skills.
pub const SKILLS_DIRECTORY: &str = ".agents/skills";

/// Required skill instruction filename.
pub const SKILL_FILE: &str = "SKILL.md";

/// Discovered Agent Skill metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDefinition {
    /// Skill name from frontmatter.
    pub name: String,
    /// Skill activation description from frontmatter.
    pub description: String,
    /// Filesystem root that revealed this skill.
    pub root: PathBuf,
    /// `.agents/skills` directory containing this skill.
    pub skills_dir: PathBuf,
    /// Skill directory containing `SKILL.md`.
    pub skill_dir: PathBuf,
    /// Full path to `SKILL.md`.
    pub skill_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillMetadata {
    pub(crate) name: String,
    pub(crate) description: String,
}

pub(crate) fn parse_skill_frontmatter(content: &str) -> Option<SkillMetadata> {
    let mut lines = content.lines();
    if lines.next()? != "---" {
        return None;
    }

    let mut name = None;
    let mut description = None;
    for line in lines {
        if line == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote_yaml_scalar(value.trim()).trim().to_owned();
        match key.trim() {
            "name" if !value.is_empty() && valid_skill_name(&value) => name = Some(value),
            "description" if !value.is_empty() => description = Some(value),
            _ => {}
        }
    }

    Some(SkillMetadata {
        name: name?,
        description: description?,
    })
}

fn unquote_yaml_scalar(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|stripped| stripped.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|stripped| stripped.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}
