//! Model Context Protocol client integration.

mod config;
mod event;
mod name;
mod runtime;
mod server;
pub(crate) mod skills;

pub use config::{McpConfig, McpServerConfig, McpTransportConfig};
pub use mcp::McpLogLevel;
pub(crate) use runtime::McpClientRuntime;
