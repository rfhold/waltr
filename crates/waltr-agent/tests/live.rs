//! Ignored live-provider integration tests.

#[path = "support/live_provider.rs"]
mod live_provider;
#[path = "support/scripted_provider.rs"]
mod scripted_provider;
#[path = "agent/session.rs"]
mod session;

#[path = "live/agent/loop.rs"]
mod r#loop;
#[path = "live/agent/session_storage.rs"]
mod session_storage;
#[path = "live/agent/timeline.rs"]
mod timeline;

#[path = "live/prompt_cache.rs"]
mod prompt_cache;
#[path = "live/streaming.rs"]
mod streaming;
#[path = "live/thinking.rs"]
mod thinking;
#[path = "live/tools.rs"]
mod tools;
