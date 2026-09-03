//! Process-generation local bearer admission and self-MCP credentials.

use axum::{
    body::Body,
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse as _, Response},
};
use waltr_agent::{
    AuthTarget, McpOAuthContext, McpOAuthEndpoints, McpOAuthRegistration, OAuthCredential,
    OAuthCredentialRecord, OAuthCredentialStore, OAuthCredentialStoreFuture, OAuthRefreshClaim,
    OAuthRefreshClaimResult, OAuthTokenSet,
};
use std::{fmt, sync::Arc};

/// Opaque bearer supplied by an embedding host for one process generation.
#[derive(Clone)]
pub struct LocalBearer(Arc<str>);

impl LocalBearer {
    /// Validate a bearer value accepted by the HTTP authorization metadata path.
    ///
    /// # Errors
    ///
    /// Returns an error without reproducing the supplied value when it is not an ASCII bearer
    /// token.
    pub fn new(value: impl Into<String>) -> Result<Self, LocalBearerError> {
        let value = value.into();
        let mut padding = false;
        if value.is_empty()
            || value.bytes().any(|byte| match byte {
                b'=' => {
                    padding = true;
                    false
                }
                b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'-'
                | b'.'
                | b'_'
                | b'~'
                | b'+'
                | b'/' => padding,
                _ => true,
            })
        {
            return Err(LocalBearerError);
        }
        Ok(Self(Arc::from(value)))
    }

    fn matches(&self, candidate: &[u8]) -> bool {
        self.0.as_bytes() == candidate
    }

    pub(crate) fn credential(&self, resource: &str) -> OAuthCredential {
        OAuthCredential::mcp(
            OAuthTokenSet::bearer(self.0.to_string(), None, None),
            McpOAuthContext {
                resource: resource.to_owned(),
                issuer: resource.to_owned(),
                registration: McpOAuthRegistration {
                    client_id: "embedded-waltr".to_owned(),
                    redirect_uri: resource.to_owned(),
                    token_endpoint_auth_method: "none".to_owned(),
                    ..McpOAuthRegistration::default()
                },
                endpoints: McpOAuthEndpoints {
                    authorization: resource.to_owned(),
                    token: resource.to_owned(),
                    revocation: None,
                },
            },
        )
    }
}

impl fmt::Debug for LocalBearer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LocalBearer(<redacted>)")
    }
}

/// Invalid local bearer configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalBearerError;

impl fmt::Display for LocalBearerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("local bearer must use the ASCII bearer token grammar")
    }
}

impl std::error::Error for LocalBearerError {}

pub(crate) async fn require_local_protocol_bearer(
    State(expected): State<LocalBearer>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let protected = request.uri().path() == "/mcp" || request.uri().path().starts_with("/waltr.");
    if !protected {
        return next.run(request).await;
    }
    let admitted = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "))
        .is_some_and(|candidate| expected.matches(candidate));
    if admitted {
        next.run(request).await
    } else {
        axum::http::StatusCode::UNAUTHORIZED.into_response()
    }
}

pub(crate) struct LocalCredentialStore<S> {
    durable: Arc<S>,
    target_key: String,
    credential: OAuthCredential,
    durable_callback_marker: Option<String>,
    current_callback_uri: Option<String>,
}

impl<S> LocalCredentialStore<S> {
    pub(crate) fn new(
        durable: Arc<S>,
        bearer: &LocalBearer,
        self_mcp_url: &str,
        durable_callback_marker: Option<String>,
    ) -> Self {
        Self {
            durable,
            target_key: AuthTarget::mcp_server("waltr").credential_key(),
            credential: bearer.credential(self_mcp_url),
            current_callback_uri: durable_callback_marker
                .as_ref()
                .and_then(|_| outbound_callback_for_self_mcp(self_mcp_url)),
            durable_callback_marker,
        }
    }
}

impl<S> OAuthCredentialStore for LocalCredentialStore<S>
where
    S: OAuthCredentialStore,
{
    fn read(
        &self,
        target_key: String,
    ) -> OAuthCredentialStoreFuture<'_, Option<OAuthCredentialRecord>> {
        if target_key == self.target_key {
            let credential = self.credential.clone();
            return Box::pin(async move {
                Ok(Some(OAuthCredentialRecord {
                    credential,
                    revision: 0,
                    reauthorization_required: false,
                }))
            });
        }
        let durable = Arc::clone(&self.durable);
        let marker = self.durable_callback_marker.clone();
        let callback = self.current_callback_uri.clone();
        Box::pin(async move {
            let record = durable.read(target_key).await?;
            Ok(record.and_then(|mut record| {
                rehydrate_durable_generation_credential(
                    &mut record.credential,
                    marker.as_deref(),
                    callback.as_deref(),
                )
                .then_some(record)
            }))
        })
    }

    fn write(
        &self,
        target_key: String,
        mut credential: OAuthCredential,
    ) -> OAuthCredentialStoreFuture<'_, ()> {
        if target_key == self.target_key {
            return Box::pin(async { Err("local credential is host-owned".to_owned()) });
        }
        prepare_transient_generation_credential_for_persistence(
            &mut credential,
            self.durable_callback_marker.as_deref(),
            self.current_callback_uri.as_deref(),
        );
        self.durable.write(target_key, credential)
    }

    fn delete(&self, target_key: String) -> OAuthCredentialStoreFuture<'_, ()> {
        if target_key == self.target_key {
            return Box::pin(async { Err("local credential is host-owned".to_owned()) });
        }
        self.durable.delete(target_key)
    }

    fn claim_refresh(
        &self,
        target_key: String,
        claim: OAuthRefreshClaim,
    ) -> OAuthCredentialStoreFuture<'_, OAuthRefreshClaimResult> {
        if target_key == self.target_key {
            let credential = self.credential.clone();
            return Box::pin(async move {
                Ok(OAuthRefreshClaimResult::Current(Box::new(
                    OAuthCredentialRecord {
                        credential,
                        revision: 0,
                        reauthorization_required: false,
                    },
                )))
            });
        }
        let durable = Arc::clone(&self.durable);
        let marker = self.durable_callback_marker.clone();
        let callback = self.current_callback_uri.clone();
        Box::pin(async move {
            match durable.claim_refresh(target_key, claim).await? {
                OAuthRefreshClaimResult::Current(mut record) => {
                    if rehydrate_durable_generation_credential(
                        &mut record.credential,
                        marker.as_deref(),
                        callback.as_deref(),
                    ) {
                        Ok(OAuthRefreshClaimResult::Current(record))
                    } else {
                        Ok(OAuthRefreshClaimResult::ReauthorizationRequired)
                    }
                }
                result => Ok(result),
            }
        })
    }

    fn commit_refresh(
        &self,
        target_key: String,
        claim: OAuthRefreshClaim,
        mut credential: OAuthCredential,
    ) -> OAuthCredentialStoreFuture<'_, bool> {
        if target_key == self.target_key {
            return Box::pin(async { Ok(false) });
        }
        prepare_transient_generation_credential_for_persistence(
            &mut credential,
            self.durable_callback_marker.as_deref(),
            self.current_callback_uri.as_deref(),
        );
        self.durable.commit_refresh(target_key, claim, credential)
    }

    fn abort_refresh(
        &self,
        target_key: String,
        claim: OAuthRefreshClaim,
        indeterminate: bool,
    ) -> OAuthCredentialStoreFuture<'_, ()> {
        if target_key == self.target_key {
            return Box::pin(async { Ok(()) });
        }
        self.durable.abort_refresh(target_key, claim, indeterminate)
    }
}

fn outbound_callback_for_self_mcp(self_mcp_url: &str) -> Option<String> {
    let mut callback = url::Url::parse(self_mcp_url).ok()?;
    callback.set_path("/outbound-oauth/callback");
    callback.set_query(None);
    callback.set_fragment(None);
    Some(callback.to_string())
}

fn rehydrate_durable_generation_credential(
    credential: &mut OAuthCredential,
    marker: Option<&str>,
    callback: Option<&str>,
) -> bool {
    let Some(context) = credential.mcp_context_mut() else {
        return true;
    };
    let Some(marker) = marker else {
        return true;
    };
    if context.registration.redirect_uri != marker {
        return false;
    }
    let Some(callback) = callback else {
        return false;
    };
    context.registration.redirect_uri = callback.to_owned();
    true
}

fn prepare_transient_generation_credential_for_persistence(
    credential: &mut OAuthCredential,
    marker: Option<&str>,
    callback: Option<&str>,
) {
    let (Some(context), Some(marker), Some(callback)) =
        (credential.mcp_context_mut(), marker, callback)
    else {
        return;
    };
    if context.registration.redirect_uri == callback {
        context.registration.redirect_uri = marker.to_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_comparison_requires_exact_value() {
        let bearer = LocalBearer::new("0123abcdef-._~+/==").unwrap();
        assert!(bearer.matches(b"0123abcdef-._~+/=="));
        assert!(!bearer.matches(b"0123abcdef-._~+/="));
        assert!(!bearer.matches(b"0123abcdef-._~+/===extra"));
        assert!(!bearer.matches(b""));
        assert_eq!(format!("{bearer:?}"), "LocalBearer(<redacted>)");
    }

    #[test]
    fn bearer_validation_rejects_non_token_input_without_debug_disclosure() {
        for invalid in [
            "",
            "has space",
            "has\ttab",
            "line\nbreak",
            "caf\u{00e9}",
            "token=tail",
        ] {
            assert_eq!(LocalBearer::new(invalid).unwrap_err(), LocalBearerError);
        }
        let secret = "secret-that-must-not-appear";
        let error = LocalBearer::new(format!("{secret} ")).unwrap_err();
        assert!(!format!("{error:?} {error}").contains(secret));
    }

    #[test]
    fn self_mcp_credential_uses_the_same_generation_bearer() {
        let bearer = LocalBearer::new("generation-secret").unwrap();
        let credential = bearer.credential("http://127.0.0.1:43123/mcp");
        assert_eq!(credential.token.access_token.secret(), "generation-secret");
        assert_eq!(
            credential.mcp_context().unwrap().resource,
            "http://127.0.0.1:43123/mcp"
        );
    }

    #[test]
    fn embedded_durable_mcp_credentials_require_the_exact_current_marker() {
        let bearer = LocalBearer::new("generation-secret").unwrap();
        let callback = "http://127.0.0.1:43123/outbound-oauth/callback";
        let marker = "urn:waltr:outbound-oauth-callback:current";
        let credential_with_redirect = |redirect_uri: &str| {
            let mut credential = bearer.credential("https://mcp.example/resource");
            credential
                .mcp_context_mut()
                .unwrap()
                .registration
                .redirect_uri = redirect_uri.to_owned();
            credential
        };

        for rejected in [
            callback,
            "https://waltr.example/pre-marker-callback",
            "urn:waltr:outbound-oauth-callback:old",
            "urn:waltr:outbound-oauth-callback:",
        ] {
            assert!(!rehydrate_durable_generation_credential(
                &mut credential_with_redirect(rejected),
                Some(marker),
                Some(callback),
            ));
        }

        let mut credential = credential_with_redirect(marker);
        assert!(rehydrate_durable_generation_credential(
            &mut credential,
            Some(marker),
            Some(callback),
        ));
        assert_eq!(
            credential.mcp_context().unwrap().registration.redirect_uri,
            callback
        );
        prepare_transient_generation_credential_for_persistence(
            &mut credential,
            Some(marker),
            Some(callback),
        );
        assert_eq!(
            credential.mcp_context().unwrap().registration.redirect_uri,
            marker
        );
    }

    #[test]
    fn standalone_durable_mcp_credentials_remain_unmarked() {
        let bearer = LocalBearer::new("standalone-secret").unwrap();
        let mut credential = bearer.credential("https://mcp.example/resource");
        let redirect_uri = "https://waltr.example/outbound-oauth/callback";
        credential
            .mcp_context_mut()
            .unwrap()
            .registration
            .redirect_uri = redirect_uri.to_owned();

        assert!(rehydrate_durable_generation_credential(
            &mut credential,
            None,
            None,
        ));
        assert_eq!(
            credential.mcp_context().unwrap().registration.redirect_uri,
            redirect_uri
        );
    }
}
