//! Deterministic agent integration tests.

#[path = "support/scripted_provider.rs"]
mod scripted_provider;

#[path = "agent/completion.rs"]
mod completion;
#[path = "agent/context_semantics.rs"]
mod context_semantics;
#[path = "agent/metadata_midstream.rs"]
mod metadata_midstream;
#[path = "agent/loop.rs"]
mod r#loop;
#[path = "agent/session.rs"]
mod session;
#[path = "agent/session_storage.rs"]
mod session_storage;
