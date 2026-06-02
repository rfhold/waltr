//! TUI screen rendering.

mod config;
mod landing;
mod splash;

pub use config::{
    ConfigTab, McpForm, McpFormMode, McpSection, ModelForm, ModelFormMode, ProviderForm,
    ProviderFormMode, ProviderSection, SectionAction, draw_config,
};
pub use landing::draw_landing;
pub use splash::draw_splash;
