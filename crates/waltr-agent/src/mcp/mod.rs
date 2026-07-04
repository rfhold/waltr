//! Model Context Protocol client integration.

mod capabilities;
mod config;
mod connection;
mod event;
mod name;
mod prompt;
mod protocol;
mod runtime;
mod server;
mod tool;
mod transport;

pub use config::{McpConfig, McpServerConfig, McpTransportConfig};
pub(crate) use runtime::McpClientRuntime;
