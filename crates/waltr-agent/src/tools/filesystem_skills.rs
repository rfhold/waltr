//! Agent Skills discovery and loading for filesystem roots.

use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::json;

use crate::{AgentTool, AgentToolContext, LlmError, Result, ToolCall, ToolDefinition};

/// Directory under a filesystem root that contains Agent Skills.
pub const SKILLS_DIRECTORY: &str = ".agents/skills";

/// Required skill instruction filename.
pub const SKILL_FILE: &str = "SKILL.md";

const RESOURCE_DIRECTORIES: &[&str] = &["scripts", "references", "assets"];

/// Agent Skills configuration for one filesystem root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsConfig {
    /// Filesystem root whose `.agents/skills` directory is initially visible.
    pub root: PathBuf,
}

impl SkillsConfig {
    /// Create skills configuration for a filesystem root.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Discover skills visible at this root's `.agents/skills` directory.
    #[must_use]
    pub fn discover(&self) -> Vec<SkillDefinition> {
        discover_skills_root(&canonicalize_or_clone(&self.root))
    }

    /// Render a system prompt catalog for initially visible skills.
    #[must_use]
    pub fn system_prompt(&self) -> Option<String> {
        render_skills_catalog(&self.discover(), "Available skills:")
    }

    /// Convert this configuration into the dedicated skill loading tool.
    #[must_use]
    pub fn into_agent_tool(self) -> AgentTool {
        AgentTool::with_context(load_skill_definition(), move |call, context| {
            load_skill(call, context)
        })
    }
}

/// Discovered Agent Skill metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Discover immediate child skill directories under `<root>/.agents/skills`.
#[must_use]
pub fn discover_skills_root(root: &Path) -> Vec<SkillDefinition> {
    let root = canonicalize_or_clone(root);
    let skills_dir = root.join(SKILLS_DIRECTORY);
    discover_skills_dir(&root, &skills_dir)
}

/// Discover immediate child skill directories under an explicit `.agents/skills` directory.
pub fn discover_skills_dir(root: &Path, skills_dir: &Path) -> Vec<SkillDefinition> {
    let root = canonicalize_or_clone(root);
    let skills_dir = canonicalize_or_clone(skills_dir);
    let Ok(entries) = fs::read_dir(&skills_dir) else {
        return Vec::new();
    };

    let mut skills = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() {
                return None;
            }
            read_skill_definition(&root, &skills_dir, &entry.path())
        })
        .collect::<Vec<_>>();
    skills.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.root.cmp(&right.root))
            .then_with(|| left.skill_dir.cmp(&right.skill_dir))
    });
    skills
}

/// Render a skills catalog, or `None` when no skills are visible.
#[must_use]
pub fn render_skills_catalog(skills: &[SkillDefinition], heading: &str) -> Option<String> {
    if skills.is_empty() {
        return None;
    }

    let ambiguous = ambiguous_names(skills);
    let mut output = String::new();
    writeln!(output, "{heading}\n").expect("write skills catalog");
    for skill in skills {
        writeln!(
            output,
            "- {}: {} Root: {}",
            skill.name,
            skill.description,
            skill.root.display()
        )
        .expect("write skills catalog");
    }
    output.push_str("\nUse the load_skill tool to load a skill before following its instructions.");
    if ambiguous.is_empty() {
        output.push_str(" If multiple skills share a name, pass the root shown in the catalog.");
    } else {
        let mut names = ambiguous.into_iter().collect::<Vec<_>>();
        names.sort();
        write!(
            output,
            " The following skill names require root when loading: {}.",
            names.join(", ")
        )
        .expect("write skills catalog");
    }
    Some(output)
}

/// Render a dynamic reminder for newly visible skills.
#[must_use]
pub fn render_skills_reminder(skills: &[SkillDefinition], root: &Path) -> Option<String> {
    render_skills_catalog(
        skills,
        &format!("New skills are available from {}:", root.display()),
    )
}

/// Discover a `.agents/skills` root and insert newly visible skills into context.
pub fn reveal_skills_root(
    context: &mut AgentToolContext,
    root: &Path,
) -> Option<Vec<SkillDefinition>> {
    if !context.skills_enabled {
        return None;
    }
    let root = canonicalize_or_clone(root);
    let skills_dir = root.join(SKILLS_DIRECTORY);
    let skills_dir = canonicalize_or_clone(&skills_dir);
    if !skills_dir.is_dir() || context.skills_revealed_roots.contains(&skills_dir) {
        return None;
    }
    context.skills_revealed_roots.insert(skills_dir.clone());
    let skills = discover_skills_dir(&root, &skills_dir);
    let mut added = Vec::new();
    for skill in skills {
        if !context
            .visible_skills
            .iter()
            .any(|visible| visible.skill_path == skill.skill_path)
        {
            context.visible_skills.push(skill.clone());
            added.push(skill);
        }
    }
    if added.is_empty() { None } else { Some(added) }
}

fn read_skill_definition(
    root: &Path,
    skills_directory: &Path,
    candidate_dir: &Path,
) -> Option<SkillDefinition> {
    let candidate_dir = canonicalize_or_clone(candidate_dir);
    let skill_path = candidate_dir.join(SKILL_FILE);
    if !skill_path.is_file() {
        return None;
    }
    let content = fs::read_to_string(&skill_path).ok()?;
    let metadata = parse_skill_frontmatter(&content)?;
    Some(SkillDefinition {
        name: metadata.name,
        description: metadata.description,
        root: root.to_path_buf(),
        skills_dir: skills_directory.to_path_buf(),
        skill_dir: candidate_dir,
        skill_path: canonicalize_or_clone(&skill_path),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillMetadata {
    name: String,
    description: String,
}

fn parse_skill_frontmatter(content: &str) -> Option<SkillMetadata> {
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

fn load_skill_definition() -> ToolDefinition {
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

fn load_skill(call: &ToolCall, context: &mut AgentToolContext) -> Result<String> {
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

fn render_loaded_skill(skill: &SkillDefinition) -> Result<String> {
    let content = fs::read_to_string(&skill.skill_path)?;
    let resources = collect_resource_paths(skill)?;
    let mut output = String::new();
    writeln!(output, "<skill_content name=\"{}\">", skill.name).expect("write skill content");
    writeln!(output, "Skill directory: {}\n", skill.skill_dir.display())
        .expect("write skill content");
    output.push_str(content.trim());
    output.push_str("\n\n<skill_resources>");
    if resources.is_empty() {
        output.push_str("\n</skill_resources>\n</skill_content>");
        return Ok(output);
    }
    for resource in resources {
        write!(output, "\n- {}", resource.display()).expect("write skill resources");
    }
    output.push_str("\n</skill_resources>\n</skill_content>");
    Ok(output)
}

fn collect_resource_paths(skill: &SkillDefinition) -> Result<Vec<PathBuf>> {
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
    root: &Path,
    directory: &Path,
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

fn ambiguous_names(skills: &[SkillDefinition]) -> HashSet<String> {
    let mut counts = HashMap::<&str, usize>::new();
    for skill in skills {
        *counts.entry(&skill.name).or_default() += 1;
    }
    counts
        .into_iter()
        .filter_map(|(name, count)| (count > 1).then_some(name.to_owned()))
        .collect()
}

fn canonicalize_or_clone(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn skills_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "skills",
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn discover_skills_scans_only_immediate_children() {
        let temp = TempRoot::new("waltr-skills-discover-test");
        temp.write(
            ".agents/skills/example/SKILL.md",
            "---\nname: example\ndescription: Use example.\n---\n# Example\n",
        );
        temp.write(
            ".agents/skills/group/nested/SKILL.md",
            "---\nname: nested\ndescription: Should not load.\n---\n# Nested\n",
        );

        let skills = SkillsConfig::new(temp.path()).discover();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "example");
    }

    #[test]
    fn malformed_skills_are_skipped() {
        let temp = TempRoot::new("waltr-skills-malformed-test");
        temp.write(
            ".agents/skills/good/SKILL.md",
            "---\nname: good\ndescription: Use good.\n---\n# Good\n",
        );
        temp.write(".agents/skills/bad/SKILL.md", "# Missing frontmatter\n");

        let skills = SkillsConfig::new(temp.path()).discover();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");
    }

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(prefix: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time after epoch")
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp root");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write(&self, path: &str, content: &str) {
            let path = self.path.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directory");
            }
            fs::write(path, content).expect("write temp file");
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.path));
        }
    }
}
