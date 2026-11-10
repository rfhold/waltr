//! Agent Skills support.

pub mod config;
pub mod definition;
pub mod render;

pub use config::SkillsConfig;
pub use definition::{SKILL_FILE, SKILLS_DIRECTORY, SkillDefinition};
pub use render::{render_skills_catalog, render_skills_reminder};
