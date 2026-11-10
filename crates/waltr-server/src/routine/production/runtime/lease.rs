use super::*;
use crate::access_auth::AuthenticatedPrincipal;
use crate::routine::ports::{OperationPolicy, Reservation};
use futures_util::FutureExt as _;
use serde_json::Value;
use waltr_script::{HostContext, HostDispatcher, HostReply, HostRequest};

pub(super) fn bind(
    factory: Arc<dyn HostFactory>,
    principal: AuthenticatedPrincipal,
    role: Option<CancellationToken>,
) -> Arc<dyn HostFactory> {
    Arc::new(LeaseFactory {
        factory,
        principal,
        role,
    })
}
struct LeaseFactory {
    factory: Arc<dyn HostFactory>,
    principal: AuthenticatedPrincipal,
    role: Option<CancellationToken>,
}
impl HostFactory for LeaseFactory {
    fn preflight_child(
        &self,
        binding: InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
        scope: crate::routine::children::ChildScope,
    ) -> PortFuture<PreparedHost> {
        let principal = self.principal.clone();
        let role = self.role.clone().unwrap_or_default();
        let factory = self.factory.clone();
        Box::pin(async move {
            check(&principal, &role)?;
            let prepared = factory
                .preflight_child(binding, definition, selected, caller, scope)
                .await?;
            Ok(wrap(prepared, principal, role))
        })
    }
    fn preflight(
        &self,
        binding: InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<PreparedHost> {
        self.preflight_input(binding, definition, selected, caller, Value::Null)
    }
    fn preflight_input(
        &self,
        binding: InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
        input: Value,
    ) -> PortFuture<PreparedHost> {
        let principal = self.principal.clone();
        let role = self.role.clone().unwrap_or_default();
        let factory = self.factory.clone();
        Box::pin(async move {
            check(&principal, &role)?;
            let prepared = factory
                .preflight_input(binding, definition, selected, caller, input)
                .await?;
            Ok(wrap(prepared, principal, role))
        })
    }
}
fn wrap(
    prepared: PreparedHost,
    principal: AuthenticatedPrincipal,
    role: CancellationToken,
) -> PreparedHost {
    PreparedHost {
        dispatcher: Arc::new(Dispatcher {
            inner: prepared.dispatcher,
            principal: principal.clone(),
            role: role.clone(),
        }),
        policy: Arc::new(Policy {
            inner: prepared.policy,
            principal: principal.clone(),
            role: role.clone(),
        }),
        reservation: Box::new(LeaseReservation {
            inner: prepared.reservation,
            principal,
            role,
        }),
        snapshot: prepared.snapshot,
    }
}
fn check(principal: &AuthenticatedPrincipal, role: &CancellationToken) -> Result<(), PortError> {
    if role.is_cancelled() || principal.access_ended().now_or_never().is_some() {
        Err(PortError::Denied)
    } else {
        Ok(())
    }
}
struct Dispatcher {
    inner: Arc<dyn HostDispatcher>,
    principal: AuthenticatedPrincipal,
    role: CancellationToken,
}
impl HostDispatcher for Dispatcher {
    fn dispatch(&self, request: HostRequest, context: HostContext) -> waltr_script::HostFuture {
        let inner = self.inner.clone();
        let principal = self.principal.clone();
        let role = self.role.clone();
        Box::pin(async move {
            check(&principal, &role).map_err(|_| waltr_script::Error::Denied)?;
            tokio::select! { biased;
                () = principal.access_ended() => Err(waltr_script::Error::Denied),
                () = role.cancelled() => Err(waltr_script::Error::Denied),
                result = inner.dispatch(request, context) => result,
            }
        })
    }
    fn cleanup(&self, id: waltr_script::InvocationId) -> waltr_script::CleanupFuture {
        self.inner.cleanup(id)
    }
}
struct Policy {
    inner: Arc<dyn OperationPolicy>,
    principal: AuthenticatedPrincipal,
    role: CancellationToken,
}
impl OperationPolicy for Policy {
    fn before(&self, request: &HostRequest, context: &HostContext) -> PortFuture<Value> {
        let checked = check(&self.principal, &self.role);
        let pending = self.inner.before(request, context);
        Box::pin(async move {
            checked?;
            pending.await
        })
    }
    fn project_result(&self, name: &str, reply: &HostReply) -> Result<Option<Value>, PortError> {
        self.inner.project_result(name, reply)
    }
}
struct LeaseReservation {
    inner: Box<dyn Reservation>,
    principal: AuthenticatedPrincipal,
    role: CancellationToken,
}
impl Reservation for LeaseReservation {
    fn source_session(&self) -> Option<String> {
        self.inner.source_session()
    }
    fn public_definition(&self) -> Option<Value> {
        self.inner.public_definition()
    }
    fn prepare(&self) -> PortFuture<()> {
        let checked = check(&self.principal, &self.role);
        let pending = self.inner.prepare();
        Box::pin(async move {
            checked?;
            pending.await
        })
    }
    fn abort(&self) -> PortFuture<()> {
        self.inner.abort()
    }
    fn finalize(&self, interrupted: bool) -> PortFuture<()> {
        self.inner.finalize(interrupted)
    }
}
