//! Session integration test aggregator.

mod support;

#[path = "session/create.rs"]
mod create;

#[path = "session/delete.rs"]
mod delete;

#[path = "session/watch.rs"]
mod watch;
