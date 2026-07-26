//! Reusable Waltr server components.

pub mod access_auth;
pub mod app;
pub mod auth;
pub mod catalog;
pub mod config;
pub mod config_service;
pub mod mcp_oauth;
pub mod mcp_service;
pub mod observability;
pub mod persistence;
pub mod profiling;
pub mod session;
pub mod status_service;
#[cfg(any(debug_assertions, feature = "test"))]
#[doc(hidden)]
pub mod test_support;
#[cfg(feature = "web")]
mod web_assets;

pub use app::{
    BuildAppError, ServerApplication, build_app, build_app_with_access_authenticator,
    build_app_with_access_authenticator_and_mcp_oauth, build_app_with_catalog_loader,
    build_app_with_config_service, build_app_with_services,
};

/// Default address the server listens on when `WALTR_SERVER_ADDR` is unset.
pub const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:14333";
