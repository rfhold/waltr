//! Cancellation and identity fence shared by native interaction consumers.
use crate::host::Error;
use std::{future::Future, pin::Pin};
use tokio_util::sync::CancellationToken;

/// Local-capable owned async work; implementations must not detach helper tasks.
pub type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Synchronous intent fence for native effects.
pub struct Guard<'a> {
    /// Operation cancellation.
    pub cancel: &'a CancellationToken,
    /// Pinned access/profile generation.
    pub generation: &'a CancellationToken,
    /// Synchronous identity and credential revision validation.
    pub valid: &'a dyn Fn() -> bool,
}
impl std::fmt::Debug for Guard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guard").finish_non_exhaustive()
    }
}
impl Guard<'_> {
    /// Check immediately before every external effect, including after any await.
    /// # Errors
    /// Returns Cancelled if any host fence rejects the effect.
    pub fn check(&self) -> Result<(), Error> {
        if self.cancel.is_cancelled() || self.generation.is_cancelled() || !(self.valid)() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}
