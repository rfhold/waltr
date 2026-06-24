//! Session integration test aggregator.

mod support;

#[path = "session/create.rs"]
mod create;

#[path = "session/delete.rs"]
mod delete;

#[path = "session/timeline.rs"]
mod timeline;

#[path = "session/watch.rs"]
mod watch;
