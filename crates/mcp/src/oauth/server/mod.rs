use crate::Result;

#[cfg(feature = "oauth-authorization-server")]
mod signing;
#[cfg(feature = "oauth-authorization-server")]
mod store;

#[cfg(feature = "oauth-authorization-server")]
pub use signing::*;
#[cfg(feature = "oauth-authorization-server")]
pub use store::*;

/// Deterministic entropy seam for OAuth secrets, codes, and identifiers.
pub trait McpOAuthEntropy: Send + Sync {
    /// Fill a caller-owned buffer with cryptographically secure random bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when entropy is unavailable.
    fn fill_bytes(&self, output: &mut [u8]) -> Result<()>;
}
