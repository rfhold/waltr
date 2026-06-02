//! Filesystem-backed configuration implementations.

mod pubsub;
mod store;
mod watcher;

pub use pubsub::{ConfigWatchEvent, FilesystemConfigPubSub};
pub use store::FilesystemConfigStore;

use watcher::spawn_config_watcher;
