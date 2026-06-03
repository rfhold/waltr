//! Agent Skills discovery.

use std::{fs, path::Path};

use crate::AgentToolContext;

use super::definition::{
    SKILL_FILE, SKILLS_DIRECTORY, SkillDefinition, canonicalize_or_clone, parse_skill_frontmatter,
};

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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::super::SkillsConfig;

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
