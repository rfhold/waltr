//! TUI screen rendering.

mod access;
mod config;
mod landing;
mod operation;
mod runtime_selector;
mod sessions;
mod splash;

pub use access::draw_access;
pub(crate) use config::{ConfigAuthState, ConfigResetState, targets_match};
pub use config::{
    ConfigTab, McpForm, McpFormMode, McpSection, ModelForm, ModelFormMode, ProviderForm,
    ProviderFormMode, ProviderSection, RuntimeDefaultsSection, draw_config,
};
pub use landing::draw_landing;
pub use operation::{OperationResult, OperationTracker, SectionAction};
pub(crate) use sessions::register_timeline_watch_command;
pub use sessions::{SessionsSection, draw_sessions};
pub use splash::draw_splash;
