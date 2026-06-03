//! Ignored live-provider integration tests.

#[path = "support/live_provider.rs"]
mod live_provider;
#[path = "agent/session.rs"]
mod session;

#[path = "live/agent/context.rs"]
mod context;
#[path = "live/agent/filesystem.rs"]
mod filesystem;
#[path = "live/agent/loop.rs"]
mod r#loop;
#[path = "live/agent/mcp.rs"]
mod mcp;
#[path = "live/agent/permissions.rs"]
mod permissions;
#[path = "live/agent/question.rs"]
mod question;
#[cfg(feature = "shell")]
#[path = "live/agent/shell.rs"]
mod shell;

#[path = "live/streaming.rs"]
mod streaming;
#[path = "live/thinking.rs"]
mod thinking;
#[path = "live/tools.rs"]
mod tools;
