//! Middleware extension points for agent runs.
//!
//! TODO: Define the core middleware traits after the agent loop and provider
//! modules have stable seams.

pub mod chain;
pub mod context;
pub mod cost;
pub mod otel;
pub mod secrets;
