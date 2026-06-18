//! Session integration test aggregator.

mod support;

#[path = "session/create.rs"]
mod create;

#[path = "session/watch.rs"]
mod watch;
