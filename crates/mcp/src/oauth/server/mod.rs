use std::time::SystemTime;

use crate::Result;

#[cfg(feature = "oauth-authorization-server")]
mod authorization;
#[cfg(feature = "oauth-resource-owner-oidc")]
mod oidc;
#[cfg(feature = "oauth-authorization-server")]
mod registration;
#[cfg(feature = "oauth-authorization-server")]
mod signing;
#[cfg(feature = "oauth-authorization-server")]
mod store;

#[cfg(feature = "oauth-authorization-server")]
pub use authorization::*;
#[cfg(feature = "oauth-resource-owner-oidc")]
pub use oidc::*;
#[cfg(feature = "oauth-authorization-server")]
pub use registration::*;
#[cfg(feature = "oauth-authorization-server")]
pub use signing::*;
#[cfg(feature = "oauth-authorization-server")]
pub use store::*;

/// Deterministic clock seam for OAuth authorization-server state machines.
pub trait McpOAuthClock: Send + Sync {
    /// Return the current wall-clock time.
    fn now(&self) -> SystemTime;
}

/// System wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct McpSystemOAuthClock;

impl McpOAuthClock for McpSystemOAuthClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Deterministic entropy seam for OAuth secrets, codes, and identifiers.
pub trait McpOAuthEntropy: Send + Sync {
    /// Fill a caller-owned buffer with cryptographically secure random bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when entropy is unavailable.
    fn fill_bytes(&self, output: &mut [u8]) -> Result<()>;
}
