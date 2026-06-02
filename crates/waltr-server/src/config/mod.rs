//! Configuration backend abstractions and implementations.

pub mod error;
pub mod filesystem;
pub mod traits;

pub use error::ConfigError;
pub use filesystem::{ConfigWatchEvent, FilesystemConfigPubSub, FilesystemConfigStore};
pub use traits::{ConfigPatch, ConfigPubSub, ConfigStore};
