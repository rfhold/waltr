//! Configuration backend abstractions and implementations.

pub mod defaults;
pub mod error;
pub mod filesystem;
pub mod handle;
pub(crate) mod headers;
pub mod traits;

pub use defaults::DefaultConfigBuilder;
pub(crate) use defaults::provider_templates as default_configuration;
pub use error::ConfigError;
pub use filesystem::{ConfigWatchEvent, FilesystemConfigPubSub, FilesystemConfigStore};
pub use handle::ConfigHandle;
pub use traits::{ConfigPatch, ConfigPubSub, ConfigStore};
