//! Configuration backend abstractions and implementations.

pub mod defaults;
pub mod error;
pub mod filesystem;
pub mod handle;
pub mod traits;

pub use defaults::default_configuration;
pub use error::ConfigError;
pub use filesystem::{ConfigWatchEvent, FilesystemConfigPubSub, FilesystemConfigStore};
pub use handle::ConfigHandle;
pub use traits::{ConfigPatch, ConfigPubSub, ConfigStore};
