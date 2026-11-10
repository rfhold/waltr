//! Server-owned Model Context Protocol integration.

pub mod apps;
mod config;
pub mod elicitation;
mod error;
mod event;
mod name;
pub mod registry;
mod runtime;
mod server;
pub mod session_apps;
pub(crate) mod skills;

pub use config::{McpAppCredentialProvider, McpConfig, McpServerConfig, McpTransportConfig};
pub use mcp::McpLogLevel;
pub(crate) use runtime::McpClientRuntime;
