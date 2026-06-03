//! TODO: Define the agent session storage interface here.
//!
//! This module should own storage traits, session record types, backend
//! capability descriptions, and shared storage errors.

mod filesystem;
mod opendal;
mod s3;
mod scylla;
