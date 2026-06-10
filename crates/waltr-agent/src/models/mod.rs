//! Model registry, pricing, and usage metadata.
//!
//! Static `ModelRef` compatibility remains in `model.rs` while this module grows
//! the dynamic catalog and cost-tracking support.

pub mod catalog;
pub mod models_dev;
pub mod pricing;
pub mod usage;
