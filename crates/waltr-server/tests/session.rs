//! Server session integration tests.

mod support;

#[path = "session/create.rs"]
mod create;

#[path = "session/agents.rs"]
mod agents;

#[path = "session/list.rs"]
mod list;

#[path = "session/delete.rs"]
mod delete;

#[path = "session/watch.rs"]
mod watch;

#[path = "session/inference_settings.rs"]
mod inference_settings;

#[path = "session/timeline.rs"]
mod timeline;

#[path = "session/commands.rs"]
mod commands;

#[path = "session/template_search.rs"]
mod template_search;
