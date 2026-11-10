//! Typed media streams and speech adapters, independent of Routine graph execution.
//!
//! No JS, RPC, helper, or catalog registration is implied. Queue credits follow
//! packets into consumers, including provider sends. Caller allocations, allocator
//! metadata, opaque prost/tonic/provider allocations are excluded. These charged
//! payload budgets are not a process RSS guarantee. Hosts must share pools across
//! invocations, retain producer handles, and join them before destroying the runtime.
pub mod snapshot;
pub mod speech;
pub mod stream;

use std::{future::Future, pin::Pin};

/// Send future returned by object-safe media boundaries.
pub type MediaFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

/// Terminal contract, resource, cancellation, or upstream failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Malformed format, operation, or protocol transition.
    Invalid,
    /// A stream key belongs to a different resource or invocation.
    Identity,
    /// An explicit admission or retention budget was exhausted.
    Limit,
    /// The invocation was cancelled.
    Cancelled,
    /// Queue, idle, or absolute invocation time expired.
    Deadline,
    /// The producer disappeared without explicit successful close.
    BrokenProducer,
    /// A required consumer disappeared.
    ConsumerDropped,
    /// A required text segment was never committed or was changed after commitment.
    Uncommitted,
    /// The speech provider rejected or failed the operation.
    Provider,
    /// An owned task panicked or could not be joined.
    Worker,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// Owner must retain and join this handle; Drop only requests cancellation.
#[derive(Debug)]
pub struct Producer {
    cancel: tokio_util::sync::CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<(), Error>>>,
}
impl Producer {
    pub(crate) fn spawn(
        cancel: tokio_util::sync::CancellationToken,
        work: impl Future<Output = Result<(), Error>> + Send + 'static,
    ) -> Self {
        Self {
            cancel,
            task: Some(tokio::spawn(work)),
        }
    }
    /// Request invocation cancellation without claiming completion.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
    /// Await the owned task; the caller must keep the runtime alive throughout.
    pub async fn join(mut self) -> Result<(), Error> {
        let result = self
            .task
            .as_mut()
            .ok_or(Error::Worker)?
            .await
            .map_err(|_| Error::Worker)?;
        self.task.take();
        result
    }
    /// Request cancellation and await actual task completion.
    pub async fn cancel_and_join(self) -> Result<(), Error> {
        self.cancel();
        self.join().await
    }
}
impl Drop for Producer {
    fn drop(&mut self) {
        if self.task.is_some() {
            self.cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests;
