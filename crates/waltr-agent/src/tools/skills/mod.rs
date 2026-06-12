//! Agent Skills support.

pub mod config;
pub mod definition;
pub mod discovery;
pub mod render;
pub mod resources;

pub use config::SkillsConfig;
pub use definition::{SKILL_FILE, SKILLS_DIRECTORY, SkillDefinition};
pub use discovery::{discover_skills_dir, discover_skills_root};
pub use render::{render_skills_catalog, render_skills_reminder};
