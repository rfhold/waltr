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
