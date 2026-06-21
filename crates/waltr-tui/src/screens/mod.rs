//! TUI screen rendering.

mod config;
mod landing;
mod operation;
mod sessions;
mod splash;

pub use config::{
    ConfigTab, McpForm, McpFormMode, McpSection, ModelForm, ModelFormMode, ProviderForm,
    ProviderFormMode, ProviderSection, SectionAction, draw_config,
};
pub use landing::draw_landing;
pub use operation::{OperationResult, OperationTracker};
pub use sessions::{SessionsSection, draw_sessions};
pub use splash::draw_splash;
