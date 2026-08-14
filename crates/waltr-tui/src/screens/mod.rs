//! TUI screen rendering.

mod access;
mod config;
mod inference_selector;
mod landing;
mod operation;
mod session_data;
mod sessions;
mod splash;

pub use access::draw_access;
pub(crate) use config::{ConfigAuthState, ConfigResetState, targets_match};
pub use config::{
    ConfigTab, InferenceDefaultsSection, McpForm, McpFormMode, McpSection, ModelForm,
    ModelFormMode, ProviderForm, ProviderFormMode, ProviderSection, draw_config,
};
pub use landing::draw_landing;
pub use operation::{OperationResult, OperationTracker, SectionAction};
pub(crate) use session_data::{SessionDataAction, SessionDataState, draw_session_data};
pub(crate) use sessions::register_timeline_watch_command;
pub use sessions::{SessionsSection, draw_sessions};
pub use splash::draw_splash;
