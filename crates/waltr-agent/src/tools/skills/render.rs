//! Agent Skills rendering.

use std::{collections::HashMap, fmt::Write as _, path::Path};

use super::definition::SkillDefinition;

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

fn ambiguous_names(skills: &[SkillDefinition]) -> std::collections::HashSet<String> {
    let mut counts = HashMap::<&str, usize>::new();
    for skill in skills {
        *counts.entry(&skill.name).or_default() += 1;
    }
    counts
        .into_iter()
        .filter_map(|(name, count)| (count > 1).then_some(name.to_owned()))
        .collect()
}
