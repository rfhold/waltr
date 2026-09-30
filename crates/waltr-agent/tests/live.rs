//! Ignored live-provider integration tests.

#[path = "support/agent_definitions.rs"]
mod agent_definitions;
#[path = "support/live_provider.rs"]
mod live_provider;
#[path = "support/sandbox.rs"]
mod sandbox;
#[path = "support/scripted_provider.rs"]
mod scripted_provider;
#[path = "agent/session.rs"]
mod session;

#[path = "live/agent/coding_agent.rs"]
mod coding_agent;
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
#[path = "live/agent/session_providers.rs"]
mod session_providers;
#[path = "live/agent/session_storage.rs"]
mod session_storage;
#[cfg(feature = "shell")]
#[path = "live/agent/shell.rs"]
mod shell;
#[path = "live/agent/task.rs"]
mod task;
#[path = "live/agent/timeline.rs"]
mod timeline;

#[path = "live/prompt_cache.rs"]
mod prompt_cache;
#[path = "live/streaming.rs"]
mod streaming;
#[path = "live/thinking.rs"]
mod thinking;
#[path = "live/tools.rs"]
mod tools;
