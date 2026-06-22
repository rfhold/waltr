//! Shared library code for Waltr.

#[allow(
    missing_debug_implementations,
    missing_docs,
    clippy::all,
    clippy::cargo,
    clippy::nursery,
    clippy::pedantic
)]
#[cfg(feature = "grpc")]
pub mod config;

#[allow(
    missing_debug_implementations,
    missing_docs,
    clippy::all,
    clippy::cargo,
    clippy::nursery,
    clippy::pedantic
)]
#[cfg(feature = "grpc")]
pub mod grpc;

/// Returns the package name for this crate.
#[must_use]
pub const fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}
