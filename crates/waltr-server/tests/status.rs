//! Server status integration tests.

mod support;

#[path = "status/health.rs"]
mod health;

#[path = "status/watch.rs"]
mod watch;
