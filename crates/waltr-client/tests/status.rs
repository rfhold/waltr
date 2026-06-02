//! Status integration test aggregator.

mod support;

#[path = "status/readiness.rs"]
mod readiness;

#[path = "status/watch.rs"]
mod watch;
