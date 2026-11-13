//! Production Routine RPC transport composition over the shared invocation owner.
pub mod api;
pub mod attachments;
pub mod blobs;
pub mod catalog;
mod captures;
pub(crate) mod cursor;
pub mod service;
pub mod transport;
pub(crate) mod wire;

#[cfg(all(test, feature = "test"))]
mod tests;

use super::{
    definition::Definition,
    history::Caller,
    ports::{HostFactory, PortError, PortFuture, PreparedHost, Reservation, SelectedEntrypoint},
    service::{ChildAdmission, ChildInvocation, InvocationService},
};
use crate::access_auth::AuthenticatedPrincipal;
use sha2::{Digest as _, Sha256};
use std::{future::Future, pin::Pin, sync::Arc};

/// Routine-specific issuer/subject partition; neither MCP identity nor a guest field.
pub fn principal_id(principal: &AuthenticatedPrincipal) -> String {
    let mut hash = Sha256::new();
    hash.update(b"waltr:routine-principal:v1\0");
    for value in [principal.issuer(), principal.subject()] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    format!("routine-v1-{}", hex(hash.finalize().as_slice()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Independent auth lifetime waiter, owned by the transport rather than a response body.
pub fn access_ended(
    principal: &AuthenticatedPrincipal,
) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
    principal.access_ended()
}

/// Host-only admission policy input, not a grant from discovery metadata.
#[derive(Debug)]
pub struct StartRequest {
    /// Validated request evidence, including the original access lease.
    pub authenticated: AuthenticatedPrincipal,
    /// Request-local cold media association, never a client-supplied owner identity.
    pub transport: Option<Arc<transport::PendingTransport>>,
    /// Authenticated Routine partition.
    pub principal: String,
    /// Immutable current catalog definition.
    pub definition: Arc<Definition>,
    /// Exact declared descriptor.
    pub entrypoint: String,
    /// Requested source; the policy must authorize and resolve its actual eligibility.
    pub source_session: Option<String>,
}

/// Actual authorized composition returned by a trusted host policy.
pub struct AuthorizedStart {
    /// Fresh request-local factory over existing agent/media/context adapters.
    pub factory: Arc<dyn HostFactory>,
    /// Actual resolved source, not merely the requested selector or correlation.
    pub source_session: Option<String>,
}
impl std::fmt::Debug for AuthorizedStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizedStart").finish_non_exhaustive()
    }
}

/// Constructor-injected source/host policy, independent of catalog offers.
pub trait StartPolicy: Send + Sync {
    /// Authorize an exact source filter before querying; absent includes all principal-owned rows.
    fn source_access(&self, _principal: String, source: Option<String>) -> PortFuture<()> {
        Box::pin(async move {
            if source.is_some() {
                Err(PortError::Denied)
            } else {
                Ok(())
            }
        })
    }
    /// Validate source eligibility without effects or durable invocation history.
    fn authorize(&self, request: StartRequest) -> PortFuture<AuthorizedStart>;
}

struct SourceFactory {
    inner: Arc<dyn HostFactory>,
    source_session: Option<String>,
    public_definition: serde_json::Value,
    transport: Option<Arc<transport::PendingTransport>>,
}
struct SourceReservation {
    inner: Box<dyn Reservation>,
    source_session: Option<String>,
    public_definition: serde_json::Value,
}
impl Reservation for SourceReservation {
    fn source_session(&self) -> Option<String> {
        self.source_session.clone()
    }
    fn public_definition(&self) -> Option<serde_json::Value> {
        Some(self.public_definition.clone())
    }
    fn prepare(&self) -> PortFuture<()> {
        self.inner.prepare()
    }
    fn abort(&self) -> PortFuture<()> {
        self.inner.abort()
    }
    fn finalize(&self, interrupted: bool) -> PortFuture<()> {
        self.inner.finalize(interrupted)
    }
    fn validate_success(&self) -> Result<(), PortError> {
        self.inner.validate_success()
    }
}
impl HostFactory for SourceFactory {
    fn preflight(
        &self,
        binding: super::history::InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<PreparedHost> {
        self.preflight_input(
            binding,
            definition,
            selected,
            caller,
            serde_json::Value::Null,
        )
    }
    fn preflight_input(
        &self,
        binding: super::history::InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
        input: serde_json::Value,
    ) -> PortFuture<PreparedHost> {
        let inner = self.inner.clone();
        let source_session = self.source_session.clone();
        let public_definition = self.public_definition.clone();
        let transport = self.transport.clone();
        Box::pin(async move {
            let mut prepared = inner
                .preflight_input(binding.clone(), definition, selected, caller, input)
                .await?;
            if let Some(transport) = transport {
                let actual = transport.binding()?;
                if actual.id() != binding.id() || actual.principal() != binding.principal() {
                    return Err(PortError::Denied);
                }
            }
            prepared.reservation = Box::new(SourceReservation {
                inner: prepared.reservation,
                source_session,
                public_definition,
            });
            Ok(prepared)
        })
    }
}

/// Bind an authorized request to the SAME service gate, engine and admission transaction.
pub fn invoke_authorized(
    service: &InvocationService,
    request: ChildInvocation,
    authorized: AuthorizedStart,
    public_definition: serde_json::Value,
    transport: Option<Arc<transport::PendingTransport>>,
) -> Result<ChildAdmission, PortError> {
    service.invoke_bound(
        request,
        Arc::new(SourceFactory {
            inner: authorized.factory,
            source_session: authorized.source_session,
            public_definition,
            transport,
        }),
    )
}
