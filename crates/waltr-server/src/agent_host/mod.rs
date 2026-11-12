//! Waltr capability ownership and host runtime assembly.

pub use crate::permission::*;
use waltr_agent::*;

mod builder;
mod capability;
mod context;
pub mod definition;
pub mod dispatcher;
pub mod external_journal;
#[cfg(test)]
mod external_journal_tests;
pub(crate) mod middleware;
pub mod prompt;
pub mod resource;
pub mod runtime_factory;
pub(crate) mod session_roles;
pub mod sandbox_gateway;
pub(crate) mod template;
pub mod tools;

pub use builder::HostRuntimeBuilder;
pub use capability::Capability;
pub use context::ServerHostContext;
pub use definition::*;
#[cfg(feature = "shell")]
pub use prompt::ShellPromptConfig;
pub use prompt::{
    AgentsInstruction, AgentsPromptConfig, FilesystemPromptConfig, SystemPromptBuilder,
};
#[cfg(feature = "shell")]
pub use tools::shell;
pub use tools::skills::{SkillDefinition, SkillsConfig};
pub use tools::{filesystem, question, skills, task};

pub mod authorized_tools;
pub mod mcp;

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    use std::fmt::Write;
    let mut value = String::with_capacity(64);
    for byte in sha2::Sha256::digest(bytes) {
        write!(value, "{byte:02x}").expect("string formatting");
    }
    value
}
