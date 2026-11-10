use std::{collections::BTreeSet, fmt, time::SystemTime};

use mcp::McpPrincipalId;
use sha2::{Digest as _, Sha256};

use super::{
    AccessAuthStore, BrowserSessionTokenDigest,
    lease::{LeaseMonitor, PrincipalLease, RevocationSignal},
};

/// The mechanism that admitted Waltr access, including explicit anonymous mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationMethod {
    /// The standalone host explicitly disabled authentication; no identity was validated.
    AnonymousDisabled,
    /// A trusted embedding host validated its current process-generation bearer.
    LocalProcessBearer,
    /// An OIDC ID token established a browser login.
    OidcIdToken,
    /// A JWT bearer access token authenticated an API request.
    BearerAccessToken,
    /// A persisted Waltr browser session authenticated a request.
    BrowserSession,
}

/// Access evidence attached to an admitted Waltr request.
///
/// The legacy type name includes explicit `AnonymousDisabled` admission, which
/// authenticates nobody. Evidence is not an ownership or data-partitioning key.
#[derive(Clone)]
pub struct AuthenticatedPrincipal {
    issuer: String,
    subject: String,
    scopes: BTreeSet<String>,
    method: AuthenticationMethod,
    lease: PrincipalLease,
    hosted_mcp: Option<std::sync::Arc<mcp::server::McpTokenAuthorization>>,
    local_generation: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
}

impl fmt::Debug for AuthenticatedPrincipal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthenticatedPrincipal")
            .field("issuer", &self.issuer)
            .field("subject", &self.subject)
            .field("scopes", &self.scopes)
            .field("method", &self.method)
            .field("lease", &self.lease)
            .field("hosted_mcp", &self.hosted_mcp.is_some())
            .finish()
    }
}

impl PartialEq for AuthenticatedPrincipal {
    fn eq(&self, other: &Self) -> bool {
        self.issuer == other.issuer
            && self.subject == other.subject
            && self.scopes == other.scopes
            && self.method == other.method
    }
}

impl Eq for AuthenticatedPrincipal {}

impl AuthenticatedPrincipal {
    pub(crate) fn anonymous_disabled() -> Self {
        Self::new(
            "waltr:anonymous-disabled",
            "shared-dev-domain",
            [],
            AuthenticationMethod::AnonymousDisabled,
        )
    }

    pub(crate) fn same_access_lease(&self, other: &Self) -> bool {
        if self != other {
            return false;
        }
        match (&self.local_generation, &other.local_generation) {
            (Some(left), Some(right)) => return std::sync::Arc::ptr_eq(left, right),
            (None, None) => {}
            _ => return false,
        }
        match (&self.hosted_mcp, &other.hosted_mcp) {
            (Some(left), Some(right)) => return std::sync::Arc::ptr_eq(left, right),
            (None, None) => {}
            _ => return false,
        }
        match (&self.lease, &other.lease) {
            (PrincipalLease::Unbounded, PrincipalLease::Unbounded) => true,
            (
                PrincipalLease::Bearer { expires_at: left },
                PrincipalLease::Bearer { expires_at: right },
            ) => left == right,
            (
                PrincipalLease::Browser {
                    token_digest: left,
                    expires_at: left_expiry,
                    revocation_signal: left_signal,
                    ..
                },
                PrincipalLease::Browser {
                    token_digest: right,
                    expires_at: right_expiry,
                    revocation_signal: right_signal,
                    ..
                },
            ) => {
                left == right
                    && left_expiry == right_expiry
                    && std::sync::Arc::ptr_eq(left_signal, right_signal)
            }
            _ => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn apps_test_principal(subject: &str) -> Self {
        Self::new(
            "https://issuer.example",
            subject,
            [],
            AuthenticationMethod::BearerAccessToken,
        )
    }
    /// Create authentication evidence for a validated identity.
    #[must_use]
    pub(super) fn new(
        issuer: impl Into<String>,
        subject: impl Into<String>,
        scopes: impl IntoIterator<Item = String>,
        method: AuthenticationMethod,
    ) -> Self {
        Self {
            issuer: issuer.into(),
            subject: subject.into(),
            scopes: scopes.into_iter().collect(),
            method,
            lease: PrincipalLease::Unbounded,
            hosted_mcp: None,
            local_generation: None,
        }
    }

    pub(super) fn bearer(
        issuer: impl Into<String>,
        subject: impl Into<String>,
        scopes: impl IntoIterator<Item = String>,
        expires_at: SystemTime,
    ) -> Self {
        Self {
            issuer: issuer.into(),
            subject: subject.into(),
            scopes: scopes.into_iter().collect(),
            method: AuthenticationMethod::BearerAccessToken,
            lease: PrincipalLease::Bearer { expires_at },
            hosted_mcp: None,
            local_generation: None,
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "browser principals retain all lease validation inputs"
    )]
    pub(super) fn browser(
        issuer: String,
        client_id: String,
        subject: String,
        token_digest: BrowserSessionTokenDigest,
        expires_at: SystemTime,
        store: AccessAuthStore,
        revocation_signal: std::sync::Arc<RevocationSignal>,
    ) -> Self {
        Self {
            issuer: issuer.clone(),
            subject,
            scopes: BTreeSet::new(),
            method: AuthenticationMethod::BrowserSession,
            lease: PrincipalLease::Browser {
                token_digest,
                issuer,
                client_id,
                expires_at,
                store,
                revocation_signal,
            },
            hosted_mcp: None,
            local_generation: None,
        }
    }

    pub(super) fn lease_monitor(&self) -> Option<LeaseMonitor> {
        self.lease.monitor()
    }

    /// Observe access loss independently of the request or response body.
    pub(crate) fn access_ended(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        if let Some(generation) = self.local_generation.clone() {
            return Box::pin(async move { generation.cancelled().await });
        }
        if let Some(proof) = self.hosted_mcp.clone() {
            return Box::pin(async move { proof.ended().await });
        }
        let monitor = self.lease_monitor();
        Box::pin(async move {
            if let Some(mut monitor) = monitor {
                futures_util::future::poll_fn(|context| monitor.poll(context)).await;
            } else {
                std::future::pending::<()>().await;
            }
        })
    }

    /// Bind the exact hosted OAuth subject and its original validated credential.
    pub(crate) fn hosted_mcp(issuer: &str, proof: mcp::server::McpTokenAuthorization) -> Self {
        Self {
            issuer: issuer.to_owned(),
            subject: proof.principal_id.as_str().to_owned(),
            scopes: BTreeSet::new(),
            method: AuthenticationMethod::BearerAccessToken,
            lease: proof
                .expires_at
                .map_or(PrincipalLease::Unbounded, |expires_at| {
                    PrincipalLease::Bearer { expires_at }
                }),
            hosted_mcp: Some(std::sync::Arc::new(proof)),
            local_generation: None,
        }
    }

    pub(crate) fn local_process(origin: &crate::local_admission::ValidatedLocalOrigin) -> Self {
        let mut principal = Self::new(
            "waltr:disabled",
            "local",
            [],
            AuthenticationMethod::LocalProcessBearer,
        );
        principal.local_generation = Some(origin.0.clone());
        principal
    }

    /// Return the validating issuer, or the fixed anonymous access namespace.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Return the issuer-local subject, or the shared anonymous domain identifier.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Return the token scopes retained as authentication evidence.
    pub fn scopes(&self) -> impl Iterator<Item = &str> {
        self.scopes.iter().map(String::as_str)
    }

    /// Return how this request was admitted; anonymous mode validates no identity.
    #[must_use]
    pub const fn method(&self) -> AuthenticationMethod {
        self.method
    }

    /// Return whether the authentication evidence contains a scope.
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }

    /// Derive the stable, domain-separated owner used for MCP sessions.
    ///
    /// # Panics
    ///
    /// Panics only if a fixed-format SHA-256 identifier violates the MCP identifier contract.
    #[must_use]
    pub fn mcp_principal_id(&self) -> McpPrincipalId {
        use fmt::Write as _;

        let mut digest = Sha256::new();
        digest.update(b"waltr:mcp-principal:v1\0");
        update_length_prefixed(&mut digest, self.issuer.as_bytes());
        update_length_prefixed(&mut digest, self.subject.as_bytes());
        let mut principal_id = String::from("waltr-v1:");
        for byte in digest.finalize() {
            write!(&mut principal_id, "{byte:02x}").expect("writing to a String cannot fail");
        }
        McpPrincipalId::new(principal_id).expect("SHA-256 principal identifiers are always valid")
    }

    /// Return the validated bearer-token expiry, when this is bearer evidence.
    #[must_use]
    pub const fn bearer_expires_at(&self) -> Option<SystemTime> {
        self.lease.bearer_expires_at()
    }
}

fn update_length_prefixed(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

#[cfg(test)]
#[allow(
    clippy::duration_suboptimal_units,
    reason = "principal tests use short expiry fixtures"
)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn anonymous_disabled_is_shared_unbounded_and_not_authenticated() {
        use futures_util::FutureExt as _;
        let principal = AuthenticatedPrincipal::anonymous_disabled();
        assert_eq!(principal.method(), AuthenticationMethod::AnonymousDisabled);
        assert_eq!(principal.scopes().count(), 0);
        assert!(principal.bearer_expires_at().is_none());
        assert!(principal.hosted_mcp.is_none());
        assert!(principal.local_generation.is_none());
        assert!(principal.access_ended().now_or_never().is_none());
        assert!(principal.same_access_lease(&AuthenticatedPrincipal::anonymous_disabled()));
    }

    #[tokio::test(start_paused = true)]
    async fn access_ended_bearer_expiry_and_unbounded_drop_are_independent() {
        use futures_util::FutureExt as _;
        let principal = AuthenticatedPrincipal::bearer(
            "issuer",
            "subject",
            [],
            SystemTime::now() + std::time::Duration::from_secs(2),
        );
        let abandoned = principal.access_ended();
        drop(abandoned);
        let remaining = principal.access_ended();
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        remaining.await;
        let unbounded = AuthenticatedPrincipal::new(
            "issuer",
            "subject",
            [],
            AuthenticationMethod::BrowserSession,
        );
        assert!(unbounded.access_ended().now_or_never().is_none());
        assert!(unbounded.access_ended().now_or_never().is_none());
    }

    #[tokio::test]
    async fn access_ended_browser_logout_and_store_revalidation_survive_waiter_drop() {
        use super::super::{BrowserSession, lease::RevocationRegistry};
        use std::{sync::Arc, time::Duration};
        for immediate in [true, false] {
            let persistence = crate::persistence::Persistence::initialize(
                crate::persistence::DatabaseConfig::for_tests().unwrap(),
            )
            .await
            .unwrap();
            let store = AccessAuthStore::new(&persistence);
            let digest = BrowserSessionTokenDigest::from_token("owned-monitor");
            let now = SystemTime::now();
            let expires_at = now + Duration::from_secs(60);
            store
                .insert_browser_session(&BrowserSession {
                    token_digest: digest.clone(),
                    issuer: "issuer".into(),
                    client_id: "client".into(),
                    subject: "subject".into(),
                    created_at: now,
                    expires_at,
                    last_seen_at: now,
                })
                .await
                .unwrap();
            let registry = RevocationRegistry::default();
            let signal = registry.signal(&digest);
            let principal = AuthenticatedPrincipal::browser(
                "issuer".into(),
                "client".into(),
                "subject".into(),
                digest.clone(),
                expires_at,
                store.clone(),
                Arc::clone(&signal),
            );
            drop(principal.access_ended());
            assert!(!signal.is_cancelled());
            let waiting = principal.access_ended();
            if immediate {
                registry.cancel(&digest);
            } else {
                store
                    .revoke_browser_session(&digest, SystemTime::now())
                    .await
                    .unwrap();
            }
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap();
            assert_eq!(signal.is_cancelled(), immediate);
        }
    }

    #[test]
    fn principal_retains_evidence_without_an_authorization_variant() {
        let principal = AuthenticatedPrincipal::new(
            "https://issuer.example",
            "subject",
            ["write".to_owned(), "read".to_owned(), "read".to_owned()],
            AuthenticationMethod::BearerAccessToken,
        );

        assert_eq!(principal.issuer(), "https://issuer.example");
        assert_eq!(principal.subject(), "subject");
        assert_eq!(principal.scopes().collect::<Vec<_>>(), ["read", "write"]);
        assert!(principal.has_scope("read"));
        assert!(principal.lease_monitor().is_none());
    }

    #[test]
    fn mcp_principal_is_stable_and_unambiguous_for_issuer_and_subject() {
        let principal = AuthenticatedPrincipal::bearer(
            "https://issuer.example",
            "subject",
            ["mcp:use".to_owned()],
            SystemTime::UNIX_EPOCH,
        );
        let refreshed = AuthenticatedPrincipal::bearer(
            "https://issuer.example",
            "subject",
            ["step-up".to_owned()],
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(60),
        );
        let different_issuer = AuthenticatedPrincipal::bearer(
            "https://other.example",
            "subject",
            Vec::<String>::new(),
            SystemTime::UNIX_EPOCH,
        );
        let ambiguous_pair = AuthenticatedPrincipal::bearer(
            "https://issuer.example/subject",
            "",
            Vec::<String>::new(),
            SystemTime::UNIX_EPOCH,
        );

        assert_eq!(principal.mcp_principal_id(), refreshed.mcp_principal_id());
        assert_ne!(
            principal.mcp_principal_id(),
            different_issuer.mcp_principal_id()
        );
        assert_ne!(
            principal.mcp_principal_id(),
            ambiguous_pair.mcp_principal_id()
        );
        assert_eq!(principal.bearer_expires_at(), Some(SystemTime::UNIX_EPOCH));
    }

    #[tokio::test]
    async fn browser_lease_debug_omits_digest_store_and_signal() {
        let persistence = crate::persistence::Persistence::initialize(
            crate::persistence::DatabaseConfig::for_tests().unwrap(),
        )
        .await
        .unwrap();
        let debug = format!(
            "{:?}",
            PrincipalLease::Browser {
                token_digest: BrowserSessionTokenDigest::from_token("plaintext-secret"),
                issuer: "https://issuer.example".to_owned(),
                client_id: "client".to_owned(),
                expires_at: SystemTime::UNIX_EPOCH,
                store: AccessAuthStore::new(&persistence),
                revocation_signal: std::sync::Arc::new(RevocationSignal::new()),
            }
        );
        assert!(!debug.contains("plaintext-secret"));
        assert!(!debug.contains("token_digest"));
        assert!(!debug.contains("store"));
        assert!(!debug.contains("signal"));
    }
}
