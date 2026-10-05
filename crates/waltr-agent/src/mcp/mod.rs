//! Model Context Protocol client integration.

pub mod apps;
mod config;
mod event;
mod name;
mod runtime;
mod server;
pub(crate) mod skills;

pub use config::{McpAppCredentialProvider, McpConfig, McpServerConfig, McpTransportConfig};
pub use mcp::McpLogLevel;
pub(crate) use runtime::McpClientRuntime;
