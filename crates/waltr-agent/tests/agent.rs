//! Deterministic agent integration tests.

#[path = "support/agent_definitions.rs"]
mod agent_definitions;
#[path = "support/scripted_provider.rs"]
mod scripted_provider;

#[path = "agent/loop.rs"]
mod r#loop;
#[path = "agent/mcp.rs"]
mod mcp;
#[path = "agent/permissions.rs"]
mod permissions;
#[path = "agent/question.rs"]
mod question;
#[path = "agent/session.rs"]
mod session;
#[path = "agent/session_storage.rs"]
mod session_storage;
#[path = "agent/task.rs"]
mod task;
