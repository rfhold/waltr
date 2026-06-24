//! Server session integration tests.

mod support;

#[path = "session/create.rs"]
mod create;

#[path = "session/list.rs"]
mod list;

#[path = "session/delete.rs"]
mod delete;

#[path = "session/watch.rs"]
mod watch;

#[path = "session/runtime_config.rs"]
mod runtime_config;

#[path = "session/timeline.rs"]
mod timeline;

#[path = "session/commands.rs"]
mod commands;
