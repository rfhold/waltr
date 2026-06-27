//! Reusable Waltr server components.

pub mod app;
pub mod auth;
pub mod catalog;
pub mod config;
pub mod config_service;
pub mod observability;
pub mod profiling;
pub mod session;
pub mod status_service;
#[cfg(any(debug_assertions, feature = "test"))]
#[doc(hidden)]
pub mod test_support;

pub use app::{
    build_app, build_app_with_catalog_loader, build_app_with_config_service,
    build_app_with_services,
};

/// Default address the server listens on when `WALTR_SERVER_ADDR` is unset.
pub const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:8080";
