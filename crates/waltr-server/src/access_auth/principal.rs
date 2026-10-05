use std::{collections::BTreeSet, fmt, time::SystemTime};

use mcp::McpPrincipalId;
use sha2::{Digest as _, Sha256};

use super::{
    AccessAuthStore, BrowserSessionTokenDigest,
    lease::{LeaseMonitor, PrincipalLease, RevocationSignal},
};

/// The mechanism that established a Waltr access identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationMethod {
    /// An OIDC ID token established a browser login.
    OidcIdToken,
    /// A JWT bearer access token authenticated an API request.
    BearerAccessToken,
    /// A persisted Waltr browser session authenticated a request.
    BrowserSession,
}

/// Authentication evidence attached to an authorized Waltr request.
///
/// Every instance grants the same effective Waltr access. The evidence is not
/// an ownership or data-partitioning key.
#[derive(Clone)]
pub struct AuthenticatedPrincipal {
    issuer: String,
    subject: String,
    scopes: BTreeSet<String>,
    method: AuthenticationMethod,
    lease: PrincipalLease,
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
        }
    }

    pub(super) fn lease_monitor(&self) -> Option<LeaseMonitor> {
        self.lease.monitor()
    }

    /// Return the exact issuer that authenticated this identity.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Return the issuer-local subject identifier.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Return the token scopes retained as authentication evidence.
    pub fn scopes(&self) -> impl Iterator<Item = &str> {
        self.scopes.iter().map(String::as_str)
    }

    /// Return how this request was authenticated.
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
