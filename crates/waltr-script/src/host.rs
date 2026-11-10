use crate::{Error, Limits, ScriptFuture, StructuredValue};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;
use uuid::Uuid;

/// Owned, cooperative host work. It must not block synchronously or detach tasks.
pub type HostFuture = Pin<Box<dyn Future<Output = Result<HostReply, Error>> + Send + 'static>>;
/// Owned asynchronous cleanup, called after every host future has been dropped.
pub type CleanupFuture = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

/// Random, invocation-local identity, not an authorization decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InvocationId(pub(crate) Uuid);

/// An unforgeable host-issued identity. Kind and generation are registry-checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostHandle {
    pub(crate) invocation: InvocationId,
    pub(crate) token: Uuid,
    pub(crate) generation: u32,
    pub(crate) kind: String,
}
impl HostHandle {
    /// Application-defined kind, validated again by the dispatcher.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }
    /// Owning invocation.
    #[must_use]
    pub const fn invocation(&self) -> InvocationId {
        self.invocation
    }
}

pub(crate) struct CallbackRequest {
    pub id: u32,
    pub value: StructuredValue,
    pub reply: oneshot::Sender<Result<StructuredValue, Error>>,
}

/// An invocation-local callback, valid only while its owning host call is pending.
#[derive(Clone)]
pub struct CallbackRef {
    context: HostContext,
    evaluate: Arc<CallbackEvaluator>,
}
type CallbackEvaluator =
    dyn Fn(StructuredValue) -> ScriptFuture<Result<StructuredValue, Error>> + Send + Sync;
impl std::fmt::Debug for CallbackRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackRef")
            .field("invocation", &self.context.invocation)
            .finish_non_exhaustive()
    }
}
impl CallbackRef {
    /// Evaluate through the owner pump; nested awaited host calls remain possible.
    ///
    /// # Errors
    /// Returns value limits, expired identity, cancellation, or callback failure.
    pub async fn invoke(&self, value: StructuredValue) -> Result<StructuredValue, Error> {
        value.encode(&self.context.limits)?;
        if self
            .context
            .registry
            .lock()
            .map_err(|_| Error::Cleanup)?
            .closed
        {
            return Err(Error::Identity);
        }
        let output = (self.evaluate)(value).await?;
        if self
            .context
            .registry
            .lock()
            .map_err(|_| Error::Cleanup)?
            .closed
        {
            return Err(Error::Identity);
        }
        output.encode(&self.context.limits)?;
        Ok(output)
    }
}

#[derive(Debug)]
pub(crate) struct Registry {
    pub closed: bool,
    pub generation: u32,
    pub handles: BTreeMap<u32, HostHandle>,
}

/// Invocation-scoped resource issuance. It grants no permission to perform effects.
#[derive(Clone, Debug)]
pub struct HostContext {
    pub(crate) invocation: InvocationId,
    pub(crate) limits: Limits,
    pub(crate) registry: Arc<Mutex<Registry>>,
}
impl HostContext {
    /// Create a fresh registry for an engine implementation or deterministic fake.
    ///
    /// # Errors
    /// Rejects invalid limits. This constructor grants no host effect authority.
    pub fn new(limits: Limits) -> Result<Self, Error> {
        limits.validate()?;
        Ok(Self {
            invocation: InvocationId(Uuid::new_v4()),
            limits,
            registry: Arc::new(Mutex::new(Registry {
                closed: false,
                generation: 0,
                handles: BTreeMap::new(),
            })),
        })
    }
    /// Invalidate all identities before host cleanup. Engine owners call this once.
    ///
    /// # Errors
    /// Returns `Cleanup` if a panic poisoned the registry.
    pub fn close(&self) -> Result<(), Error> {
        let mut registry = self.registry.lock().map_err(|_| Error::Cleanup)?;
        registry.closed = true;
        registry.handles.clear();
        drop(registry);
        Ok(())
    }
    /// Create an opaque callback transport for an engine implementation or fake.
    /// The owner enforces call lifetime, evaluation quotas, and cancellation.
    pub fn callback(
        &self,
        evaluate: impl Fn(StructuredValue) -> ScriptFuture<Result<StructuredValue, Error>>
        + Send
        + Sync
        + 'static,
    ) -> CallbackRef {
        CallbackRef {
            context: self.clone(),
            evaluate: Arc::new(evaluate),
        }
    }
    /// Owning invocation.
    #[must_use]
    pub const fn invocation(&self) -> InvocationId {
        self.invocation
    }
    /// Issue an opaque kind-tagged resource identity. Host owns the actual resource.
    ///
    /// # Errors
    /// Rejects malformed kinds, closed registries, quota or generation exhaustion.
    pub fn issue_handle(&self, kind: &str) -> Result<HostHandle, Error> {
        if kind.is_empty()
            || kind.len() > 128
            || !kind
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(Error::Invalid);
        }
        let mut registry = self.registry.lock().map_err(|_| Error::Cleanup)?;
        if registry.closed {
            return Err(Error::Identity);
        }
        if registry.handles.len() >= self.limits.handles {
            return Err(Error::Limit);
        }
        registry.generation = registry.generation.checked_add(1).ok_or(Error::Limit)?;
        let generation = registry.generation;
        let handle = HostHandle {
            invocation: self.invocation,
            token: Uuid::new_v4(),
            generation,
            kind: kind.into(),
        };
        registry.handles.insert(generation, handle.clone());
        drop(registry);
        Ok(handle)
    }
    /// Check identity, lifetime, invocation, and expected kind before using a resource.
    ///
    /// # Errors
    /// Returns `Identity` on any mismatch or revocation, or `Cleanup` on poisoning.
    pub fn validate_handle(&self, handle: &HostHandle, kind: &str) -> Result<(), Error> {
        let registry = self.registry.lock().map_err(|_| Error::Cleanup)?;
        if registry.closed
            || handle.invocation != self.invocation
            || handle.kind != kind
            || registry.handles.get(&handle.generation) != Some(handle)
        {
            return Err(Error::Identity);
        }
        drop(registry);
        Ok(())
    }
    /// Revoke an identity. The dispatcher must separately close its actual resource.
    ///
    /// # Errors
    /// Returns the same identity and registry errors as `validate_handle`.
    pub fn release_handle(&self, handle: &HostHandle) -> Result<(), Error> {
        self.validate_handle(handle, &handle.kind)?;
        self.registry
            .lock()
            .map_err(|_| Error::Cleanup)?
            .handles
            .remove(&handle.generation);
        Ok(())
    }
    pub(crate) fn lookup(&self, generation: u32) -> Result<HostHandle, Error> {
        let registry = self.registry.lock().map_err(|_| Error::Cleanup)?;
        if registry.closed {
            return Err(Error::Identity);
        }
        registry
            .handles
            .get(&generation)
            .cloned()
            .ok_or(Error::Identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> HostContext {
        HostContext {
            invocation: InvocationId(Uuid::new_v4()),
            limits: Limits::default(),
            registry: Arc::new(Mutex::new(Registry {
                closed: false,
                generation: 0,
                handles: BTreeMap::new(),
            })),
        }
    }
    #[test]
    fn forged_stale_wrong_kind_and_cross_invocation_are_denied() {
        let owner = context();
        let other = context();
        let handle = owner.issue_handle("reader").expect("issue");
        assert_eq!(
            owner.validate_handle(&handle, "writer"),
            Err(Error::Identity)
        );
        assert_eq!(
            other.validate_handle(&handle, "reader"),
            Err(Error::Identity)
        );
        let mut forged = handle.clone();
        forged.token = Uuid::new_v4();
        assert_eq!(
            owner.validate_handle(&forged, "reader"),
            Err(Error::Identity)
        );
        let mut forged = handle.clone();
        forged.generation += 1;
        assert_eq!(
            owner.validate_handle(&forged, "reader"),
            Err(Error::Identity)
        );
        owner.release_handle(&handle).expect("release");
        let next = owner.issue_handle("reader").expect("issue");
        assert_ne!(next, handle);
        assert_eq!(
            owner.validate_handle(&handle, "reader"),
            Err(Error::Identity)
        );
        owner.validate_handle(&next, "reader").expect("current");
    }
}

/// A validated bridge request. Operation names are untrusted; host policy is final.
#[derive(Debug)]
pub struct HostRequest {
    /// Application-defined operation, ASCII and at most 128 bytes.
    pub operation: String,
    /// Strict bounded structured arguments.
    pub arguments: StructuredValue,
    /// Separate bounded binary data, never JSON encoded.
    pub bytes: Option<Vec<u8>>,
    /// Optional rooted callback.
    pub callback: Option<CallbackRef>,
    /// Optional validated resource identity.
    pub handle: Option<HostHandle>,
}

/// One host reply; binary and resource values remain outside structured JSON.
#[derive(Debug)]
pub enum HostReply {
    /// Structured result.
    Value(StructuredValue),
    /// Binary result, exposed as an `ArrayBuffer`.
    Bytes(Vec<u8>),
    /// Host-issued identity, exposed as a frozen opaque object.
    Handle(HostHandle),
}

/// The authority boundary. No backend operation set or server integration exists here.
pub trait HostDispatcher: Send + Sync {
    /// Return an owned cooperative future; never perform blocking work here.
    fn dispatch(&self, request: HostRequest, context: HostContext) -> HostFuture;
    /// Release actual invocation resources after host futures have been dropped.
    fn cleanup(&self, invocation: InvocationId) -> CleanupFuture;
}
