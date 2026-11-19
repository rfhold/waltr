use std::{sync::Weak, time::SystemTime};

use crate::access_auth::{AccessAuthenticator, RequestAuthFailure};

use super::*;

#[derive(Clone, Default)]
pub(crate) struct MachineRegistry(Arc<Registry>);

#[derive(Default)]
struct Registry {
    claims: Mutex<Claims>,
    stopped: CancellationToken,
}

#[derive(Default)]
struct Claims {
    live: HashMap<String, LiveClaim>,
    gateways: HashMap<(String, WorkspaceAccount), Weak<Inner>>,
    next_epoch: u64,
}

struct LiveClaim {
    epoch: u64,
    gateway: SandboxGateway,
}

impl MachineRegistry {
    fn live_handle(&self, machine: &str, account: &WorkspaceAccount) -> Option<SandboxHandle> {
        let claims = self.0.claims.lock().unwrap();
        let gateway = &claims.live.get(machine)?.gateway;
        if gateway.inner.account.as_ref() != Some(account) {
            return None;
        }
        let handle = gateway.handle();
        handle.canonical_root()?;
        Some(handle)
    }

    pub(crate) fn owns_live(&self, machine: &str, account: &WorkspaceAccount) -> bool {
        self.live_handle(machine, account).is_some()
    }

    pub(crate) async fn resolve(
        &self,
        machine: &str,
        directory: Option<&str>,
        account: &WorkspaceAccount,
    ) -> Result<SandboxHandle> {
        self.live_handle(machine, account)
            .ok_or_else(|| error("workspace machine offline"))?
            .bind_directory(directory.unwrap_or(""))
            .await
    }

    pub(crate) async fn restore(
        &self,
        scope: &WorkspaceScope,
        account: &WorkspaceAccount,
    ) -> Result<SandboxHandle> {
        if scope.account.as_ref() != Some(account) {
            return Err(error("workspace machine offline"));
        }
        let handle = self
            .live_handle(&scope.machine_id, account)
            .ok_or_else(|| error("workspace machine offline"))?
            .bind_at(
                &format!("./{}", scope.working_directory),
                scope.canonical_root.clone(),
            )
            .await?;
        if handle.scope() != Some(scope) {
            return Err(error("workspace export or directory changed"));
        }
        Ok(handle)
    }

    pub(crate) fn shutdown(&self) {
        let mut claims = self.0.claims.lock().unwrap();
        self.0.stopped.cancel();
        for inner in claims
            .gateways
            .drain()
            .filter_map(|(_, inner)| inner.upgrade())
        {
            SandboxGateway { inner }.shutdown();
        }
        claims.live.clear();
    }

    pub(crate) fn mount(&self, router: axum::Router, auth: AccessAuthenticator) -> axum::Router {
        if !auth.enabled() {
            return router;
        }
        router.route_service(
            "/waltr.sandbox.v1.SandboxService/Connect",
            SandboxServiceServer::new(HostedConnect {
                machines: self.clone(),
                auth,
            }),
        )
    }

    fn claim(
        &self,
        hello: proto::Hello,
        account: WorkspaceAccount,
        expires: Instant,
    ) -> std::result::Result<AdmittedConnection, Status> {
        let machine = hello.workspace_id.clone();
        if machine == "local"
            || machine.is_empty()
            || machine.len() > 128
            || !machine
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(Status::invalid_argument("invalid workspace machine ID"));
        }
        let mut claims = self.0.claims.lock().unwrap();
        if self.0.stopped.is_cancelled() {
            return Err(Status::unavailable("gateway shut down"));
        }
        if Instant::now() >= expires {
            return Err(expired_bearer());
        }
        if let Some(claim) = claims.live.get(&machine) {
            // Retire the gateway epoch even if its stream driver has not dropped its guard yet.
            if !claim.gateway.inner.retire_inactive_connection() {
                return Err(Status::already_exists("worker already connected"));
            }
            claims.live.remove(&machine);
        }
        // Preserve outstanding replies on same-account reconnects; never share a
        // gateway between owners, even if a replacement claims the same machine ID.
        claims.gateways.retain(|_, inner| inner.strong_count() > 0);
        let key = (machine.clone(), account.clone());
        let gateway = claims
            .gateways
            .get(&key)
            .and_then(Weak::upgrade)
            .map_or_else(
                || SandboxGateway::for_account(machine.clone(), Some(account)),
                |inner| SandboxGateway { inner },
            );
        let (mut guard, outbound, receiver) = gateway.admit(hello, Some(expires))?;
        claims.gateways.insert(key, Arc::downgrade(&gateway.inner));
        claims.next_epoch += 1;
        let epoch = claims.next_epoch;
        claims
            .live
            .insert(machine.clone(), LiveClaim { epoch, gateway });
        let registry = Arc::downgrade(&self.0);
        guard.on_disconnect = Some(Box::new(move || {
            if let Some(registry) = registry.upgrade() {
                registry.release(&machine, epoch);
            }
        }));
        Ok((guard, outbound, receiver))
    }
}

impl Registry {
    fn release(&self, machine: &str, epoch: u64) {
        let mut claims = self.claims.lock().unwrap();
        if claims
            .live
            .get(machine)
            .is_some_and(|claim| claim.epoch == epoch)
        {
            claims.live.remove(machine);
        }
    }
}

struct HostedConnect {
    machines: MachineRegistry,
    auth: AccessAuthenticator,
}

#[tonic::async_trait]
impl SandboxService for HostedConnect {
    type ConnectStream = ConnectStream;

    async fn connect(
        &self,
        request: Request<tonic::Streaming<proto::ConnectRequest>>,
    ) -> std::result::Result<Response<Self::ConnectStream>, Status> {
        let (metadata, _, mut incoming) = request.into_parts();
        let principal = self
            .auth
            .authenticate_bearer_headers(&metadata.into_headers(), self.auth.api_required_scopes())
            .await
            .map_err(|failure| match failure {
                RequestAuthFailure::Internal => {
                    Status::unavailable("Waltr access authentication unavailable")
                }
                RequestAuthFailure::Unauthenticated | RequestAuthFailure::InsufficientScope => {
                    Status::unauthenticated("Waltr access token required")
                }
            })?;
        let account = WorkspaceAccount::from_principal(&principal)
            .ok_or_else(|| Status::unauthenticated("native Waltr access token required"))?;
        let now = Instant::now();
        let expiry = principal
            .bearer_expires_at()
            .and_then(|expiry| expiry.duration_since(SystemTime::now()).ok())
            .and_then(|remaining| now.checked_add(remaining))
            .ok_or_else(expired_bearer)?;
        let hello = read_hello(&mut incoming, &self.machines.0.stopped, Some(expiry)).await?;
        let (guard, outbound, receiver) = self.machines.claim(hello, account, expiry)?;
        Ok(Response::new(SandboxGateway::drive_connection(
            incoming,
            guard,
            outbound,
            receiver,
            Some(expiry),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt as _;

    fn account(issuer: &str, subject: &str) -> WorkspaceAccount {
        WorkspaceAccount {
            issuer: issuer.into(),
            subject: subject.into(),
        }
    }

    fn hello() -> proto::Hello {
        proto::Hello {
            workspace_id: "laptop".into(),
            worker_id: "worker".into(),
            canonical_root: "/export".into(),
            tools: vec!["read".into()],
            ..Default::default()
        }
    }

    #[test]
    fn workspace_owner_requires_verified_bearer_evidence() {
        assert!(
            WorkspaceAccount::from_principal(&AuthenticatedPrincipal::anonymous_disabled())
                .is_none()
        );
        assert!(
            WorkspaceAccount::from_principal(&AuthenticatedPrincipal::apps_test_principal("owner"))
                .is_none()
        );
    }

    async fn answer_directory(guard: &ConnectionGuard, stream: &mut OutboundStream, path: &str) {
        let request = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Some(HostBody::WorkspaceRequest(request)) = request.body else {
            panic!("expected directory request");
        };
        assert_eq!(request.operation, "resolve_working_directory");
        assert_eq!(request.path, path);
        guard
            .inner
            .dispatch(
                guard.epoch,
                WorkerBody::WorkspaceResponse(proto::WorkspaceResponse {
                    request_id: request.request_id,
                    canonical_root: "/export".into(),
                    working_directory: Some("repo".into()),
                    ..Default::default()
                }),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn live_claim_owns_directory_and_never_rebinds_another_accounts_handle() {
        let registry = MachineRegistry::default();
        let owner = account("issuer", "subject");
        let different_issuer = account("other-issuer", "subject");
        let different_subject = account("issuer", "other-subject");
        let expires = Instant::now() + Duration::from_secs(30);
        let (guard, _, mut stream) = registry.claim(hello(), owner.clone(), expires).unwrap();
        for other in [&different_issuer, &different_subject] {
            assert!(!registry.owns_live("laptop", other));
            assert!(registry.resolve("laptop", None, other).await.is_err());
            assert_eq!(
                registry
                    .claim(hello(), other.clone(), expires)
                    .err()
                    .unwrap()
                    .code(),
                tonic::Code::AlreadyExists
            );
        }
        assert!(registry.owns_live("laptop", &owner));

        let (handle, ()) = tokio::join!(
            registry.resolve("laptop", Some("repo"), &owner),
            answer_directory(&guard, &mut stream, "repo"),
        );
        let handle = handle.unwrap();
        let scope = handle.scope().unwrap();
        assert_eq!(scope.account.as_ref(), Some(&owner));
        assert!(registry.restore(scope, &different_subject).await.is_err());
        let mut ownerless = serde_json::to_value(scope).unwrap();
        ownerless.as_object_mut().unwrap().remove("account");
        let ownerless = serde_json::from_value::<WorkspaceScope>(ownerless).unwrap();
        assert!(ownerless.account.is_none());
        assert!(registry.restore(&ownerless, &owner).await.is_err());

        drop(guard);
        assert!(!registry.owns_live("laptop", &owner));
        let (foreign, _, mut foreign_stream) = registry
            .claim(hello(), different_subject.clone(), expires)
            .unwrap();
        assert!(registry.owns_live("laptop", &different_subject));
        assert!(registry.restore(scope, &owner).await.is_err());
        assert!(handle.read_file("sentinel").await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), foreign_stream.next())
                .await
                .is_err()
        );
        drop(foreign);

        let (restored_guard, _, mut replacement) =
            registry.claim(hello(), owner.clone(), expires).unwrap();
        registry.0.release("laptop", 1);
        assert!(registry.owns_live("laptop", &owner));
        let (restored, ()) = tokio::join!(
            registry.restore(scope, &owner),
            answer_directory(&restored_guard, &mut replacement, "./repo"),
        );
        let restored = restored.unwrap();
        assert_eq!(restored.scope(), Some(scope));
        assert!(Arc::ptr_eq(&restored.inner, &handle.inner));
        assert!(
            restored
                .prepare("write", &serde_json::json!({}))
                .await
                .is_err()
        );
        drop(restored_guard);

        let (changed_root, _, _) = registry
            .claim(
                proto::Hello {
                    canonical_root: "/another-export".into(),
                    ..hello()
                },
                owner.clone(),
                expires,
            )
            .unwrap();
        assert!(registry.owns_live("laptop", &owner));
        let error = registry.restore(scope, &owner).await.unwrap_err();
        assert!(error.to_string().contains("export root changed"));
        drop(changed_root);
    }

    #[tokio::test(start_paused = true)]
    async fn disconnected_execute_is_not_replayed_to_a_replacement() {
        let registry = MachineRegistry::default();
        let owner = account("issuer", "subject");
        let expires = Instant::now() + Duration::from_secs(30);
        let (guard, _, mut stream) = registry.claim(hello(), owner.clone(), expires).unwrap();
        let (handle, ()) = tokio::join!(
            registry.resolve("laptop", Some("repo"), &owner),
            answer_directory(&guard, &mut stream, "repo"),
        );
        let handle = handle.unwrap();
        let pending = tokio::spawn(async move {
            let mut ticket = handle
                .prepare("read", &serde_json::json!({}))
                .await
                .unwrap();
            ticket.execution_timeout = Duration::from_secs(1);
            handle
                .execute(ticket, CancellationToken::new(), ToolProgressSink::noop())
                .await
        });
        let Some(HostBody::Prepare(prepare)) = stream.next().await.unwrap().unwrap().body else {
            panic!("expected prepare");
        };
        guard
            .inner
            .dispatch(
                guard.epoch,
                WorkerBody::Prepared(proto::Prepared {
                    invocation_id: prepare.invocation_id,
                    ticket: "ticket".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        assert!(matches!(
            stream.next().await.unwrap().unwrap().body,
            Some(HostBody::Execute(_))
        ));
        drop(guard);

        let (_replacement, _, mut stream) = registry
            .claim(
                proto::Hello {
                    worker_id: "new-worker".into(),
                    ..hello()
                },
                owner,
                expires,
            )
            .unwrap();
        let error = tokio::select! {
            result = pending => result.unwrap().unwrap_err(),
            body = stream.next() => panic!("unexpected redispatch: {body:?}"),
        };
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test(start_paused = true)]
    async fn expired_claim_is_not_live_even_before_driver_cleanup() {
        let registry = MachineRegistry::default();
        let owner = account("issuer", "subject");
        let expiry = Instant::now() + Duration::from_secs(1);
        let (guard, _, _) = registry.claim(hello(), owner.clone(), expiry).unwrap();
        assert!(registry.owns_live("laptop", &owner));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!registry.owns_live("laptop", &owner));
        assert!(registry.resolve("laptop", None, &owner).await.is_err());
        drop(guard);
        let error = registry.claim(hello(), owner, expiry).err().unwrap();
        assert_eq!(error.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_claim_can_be_replaced_before_its_guard_drops() {
        let registry = MachineRegistry::default();
        let owner = account("issuer", "subject");
        let expiry = Instant::now() + Duration::from_secs(1);
        let (old, _, _) = registry.claim(hello(), owner.clone(), expiry).unwrap();
        let renewed_expiry = expiry + Duration::from_secs(1);
        let duplicate = registry
            .claim(hello(), owner.clone(), renewed_expiry)
            .err()
            .unwrap();
        assert_eq!(duplicate.code(), tonic::Code::AlreadyExists);

        tokio::time::advance(Duration::from_secs(1)).await;
        let (replacement, _, _) = registry
            .claim(hello(), owner.clone(), renewed_expiry)
            .unwrap();
        assert!(Arc::ptr_eq(&old.inner, &replacement.inner));
        assert_ne!(old.epoch, replacement.epoch);
        assert_eq!(
            old.inner
                .dispatch(old.epoch, WorkerBody::Hello(hello()))
                .unwrap_err()
                .code(),
            tonic::Code::Unavailable
        );
        drop(old);
        assert!(registry.owns_live("laptop", &owner));
        drop(replacement);
        assert!(!registry.owns_live("laptop", &owner));
    }
}
