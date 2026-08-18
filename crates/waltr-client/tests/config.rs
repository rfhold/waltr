//! Config integration test aggregator.

mod support;

#[path = "config/read.rs"]
mod read;

#[path = "config/watch.rs"]
mod watch;

#[path = "config/providers.rs"]
mod providers;

#[path = "config/stale_etag.rs"]
mod stale_etag;

#[path = "config/source.rs"]
mod source;

#[path = "config/reset.rs"]
mod reset;
