//! TODO: Move MCP transports here.
//!
//! This module should own the transport trait or enum and re-export stdio,
//! streamable HTTP, and SSE response parsing support.

mod http;
mod sse;
mod stdio;
