use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use mcp::oauth::*;

pub const fn config() -> OAuthAuthorizationStoreConfig {
    OAuthAuthorizationStoreConfig {
        max_clients: 4,
        max_redirect_uris_per_client: 2,
        max_authorization_transactions: 4,
        max_grants: 4,
        max_refresh_families: 4,
        max_access_tokens: 8,
    }
}

#[allow(clippy::too_many_lines)]
pub async fn assert_store_contract(
    first: Arc<dyn OAuthAuthorizationStore>,
    reopened: Arc<dyn OAuthAuthorizationStore>,
) {
    assert!(first.register_client(client("client")).await.unwrap());
    assert!(!first.register_client(client("client")).await.unwrap());
    assert_eq!(
        first.client("client".to_owned(), -1).await,
        Err(OAuthAuthorizationStoreError::InvalidData)
    );
    let mut invalid_client = client("invalid-client");
    invalid_client.redirect_uris = vec!["https://client.example/callback#fragment".to_owned()];
    assert_eq!(
        first.register_client(invalid_client).await,
        Err(OAuthAuthorizationStoreError::InvalidData)
    );
    let transaction = OAuthAuthorizationTransaction {
        transaction_id: "transaction".to_owned(),
        client_id: "client".to_owned(),
        redirect_uri: "https://client.example/callback".to_owned(),
        resource: "https://resource.example/mcp".to_owned(),
        scopes: vec!["write".to_owned(), "read".to_owned(), "read".to_owned()],
        pkce_challenge: "challenge".to_owned(),
        browser_correlation_digest: digest(1),
        state_reference: "state-reference".to_owned(),
        principal_id: None,
        decision: None,
        created_at_ms: 1_000,
        expires_at_ms: 10_000,
    };
    assert!(
        first
            .create_authorization_transaction(transaction)
            .await
            .unwrap()
    );
    assert!(
        reopened
            .authenticate_authorization_transaction(
                "transaction".to_owned(),
                McpPrincipalId::new("alice").unwrap(),
                2_000
            )
            .await
            .unwrap()
    );
    assert!(
        !first
            .authenticate_authorization_transaction(
                "transaction".to_owned(),
                McpPrincipalId::new("bob").unwrap(),
                2_000
            )
            .await
            .unwrap()
    );
    assert!(
        reopened
            .decide_authorization_transaction(
                "transaction".to_owned(),
                OAuthAuthorizationDecision::Approved,
                2_000
            )
            .await
            .unwrap()
    );
    assert!(
        !first
            .decide_authorization_transaction(
                "transaction".to_owned(),
                OAuthAuthorizationDecision::Denied,
                2_000
            )
            .await
            .unwrap()
    );
    assert!(
        first
            .issue_authorization_code(OAuthAuthorizationCode {
                code_digest: digest(2),
                transaction_id: "transaction".to_owned(),
                created_at_ms: 2_000,
                expires_at_ms: 8_000,
                consumed_at_ms: None
            })
            .await
            .unwrap()
    );

    let issue = issue_request();
    let (left, right) = tokio::join!(
        first.redeem_authorization_code(issue.clone()),
        reopened.redeem_authorization_code(issue)
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, OAuthIssueGrantResult::Issued(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, OAuthIssueGrantResult::InvalidGrant))
            .count(),
        1
    );

    assert!(
        reopened
            .validate_access_token(validation("access-1", vec!["read", "write"]))
            .await
            .unwrap()
    );
    assert!(
        !first
            .validate_access_token(validation("access-1", vec!["read"]))
            .await
            .unwrap()
    );
    assert!(
        !first
            .validate_access_token(OAuthValidateAccessTokenRequest {
                principal_id: McpPrincipalId::new("bob").unwrap(),
                ..validation("access-1", vec!["read", "write"])
            })
            .await
            .unwrap()
    );

    let rotation = OAuthRefreshRotationRequest {
        refresh_token_digest: digest(3),
        client_id: "client".to_owned(),
        resource: "https://resource.example/mcp".to_owned(),
        scopes: vec!["read".to_owned()],
        replacement_refresh_token_digest: digest(4),
        replacement_refresh_token_expires_at_ms: 8_500,
        access_token: token("access-2", 3_000),
        now_ms: 3_000,
    };
    let (left, right) = tokio::join!(
        first.rotate_refresh_token(rotation.clone()),
        reopened.rotate_refresh_token(rotation)
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, OAuthRefreshRotationResult::Rotated { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, OAuthRefreshRotationResult::ReplayDetected))
            .count(),
        1
    );
    assert!(
        !first
            .validate_access_token(validation("access-2", vec!["read"]))
            .await
            .unwrap()
    );

    first
        .revoke(OAuthRevocationTarget::Client("client".to_owned()), 4_000)
        .await
        .unwrap();
    first
        .revoke(OAuthRevocationTarget::Client("client".to_owned()), 4_000)
        .await
        .unwrap();
    assert!(
        !reopened
            .validate_access_token(validation("access-1", vec!["read", "write"]))
            .await
            .unwrap()
    );
    assert!(first.register_client(client("client-2")).await.unwrap());
    assert!(first.register_client(client("client-3")).await.unwrap());
    assert!(first.register_client(client("client-4")).await.unwrap());
    assert!(!first.register_client(client("client-5")).await.unwrap());

    assert_signing_contract(first, reopened).await;
}

#[allow(clippy::too_many_lines)]
async fn assert_signing_contract(
    first: Arc<dyn OAuthAuthorizationStore>,
    reopened: Arc<dyn OAuthAuthorizationStore>,
) {
    let entropy = CounterEntropy::default();
    let old_ring =
        VersionedOAuthWrappingKeyring::new("wrap-1", [("wrap-1".to_owned(), vec![1; 32])]).unwrap();
    assert!(
        VersionedOAuthWrappingKeyring::new(
            "wrap-1",
            [
                ("wrap-1".to_owned(), vec![1; 32]),
                ("wrap-1".to_owned(), vec![2; 32]),
            ],
        )
        .is_err()
    );
    let candidate1 = generate_es256_signing_key_candidate(
        "https://issuer.example",
        "key-1",
        1_000,
        &old_ring,
        &entropy,
    )
    .unwrap();
    let candidate2 = generate_es256_signing_key_candidate(
        "https://issuer.example",
        "key-2",
        1_001,
        &old_ring,
        &entropy,
    )
    .unwrap();
    assert_ne!(
        candidate1.encrypted_private_key.nonce,
        candidate2.encrypted_private_key.nonce
    );
    let alias_ring =
        VersionedOAuthWrappingKeyring::new("wrap-alias", [("wrap-alias".to_owned(), vec![1; 32])])
            .unwrap();
    let mut tampered = candidate1.encrypted_private_key.clone();
    tampered.wrapping_key_id = "wrap-alias".to_owned();
    assert!(alias_ring.decrypt("ES256", "key-1", &tampered).is_err());
    let (left, right) = tokio::join!(
        first.put_signing_key_candidate(candidate1, true),
        reopened.put_signing_key_candidate(candidate2, true)
    );
    let keys = [left.unwrap(), right.unwrap()];
    assert_eq!(
        keys.iter()
            .filter(|key| key.state == OAuthSigningKeyState::Active)
            .count(),
        1
    );
    let active = first
        .active_signing_key("https://issuer.example".to_owned())
        .await
        .unwrap()
        .unwrap();
    validate_es256_signing_key_readiness(&active, &old_ring).unwrap();
    let mut mismatched = active.clone();
    mismatched.public_jwk["kid"] = serde_json::json!("different-key");
    assert_eq!(
        validate_es256_signing_key_readiness(&mismatched, &old_ring),
        Err(OAuthAuthorizationStoreError::InvalidData)
    );
    let pending = keys.iter().find(|key| key.key_id != active.key_id).unwrap();
    assert!(
        reopened
            .activate_signing_key(
                "https://issuer.example".to_owned(),
                pending.key_id.clone(),
                2_000,
                5_000,
                4_000
            )
            .await
            .unwrap()
    );
    let published = first
        .verification_signing_keys("https://issuer.example".to_owned(), 3_000)
        .await
        .unwrap();
    assert_eq!(published.len(), 2);
    let active = reopened
        .active_signing_key("https://issuer.example".to_owned())
        .await
        .unwrap()
        .unwrap();
    let encrypted = active.encrypted_private_key.as_ref().unwrap();
    let plaintext = old_ring
        .decrypt("ES256", &active.key_id, encrypted)
        .unwrap();
    let new_ring = VersionedOAuthWrappingKeyring::new(
        "wrap-2",
        [
            ("wrap-1".to_owned(), vec![1; 32]),
            ("wrap-2".to_owned(), vec![2; 32]),
        ],
    )
    .unwrap();
    let rewrapped = new_ring
        .encrypt("ES256", &active.key_id, &plaintext, &entropy)
        .unwrap();
    assert!(
        first
            .rewrap_signing_key(active.issuer.clone(), active.key_id.clone(), rewrapped)
            .await
            .unwrap()
    );
    let active = reopened
        .active_signing_key(active.issuer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        active
            .encrypted_private_key
            .as_ref()
            .unwrap()
            .wrapping_key_id,
        "wrap-2"
    );
    assert!(
        old_ring
            .decrypt(
                "ES256",
                &active.key_id,
                active.encrypted_private_key.as_ref().unwrap()
            )
            .is_err()
    );
    assert!(!format!("{:?}", active.encrypted_private_key).contains("ciphertext"));
    let cleanup = first.cleanup(5_000, 10).await.unwrap();
    assert_eq!(cleanup.signing_keys_deleted, 1);
    assert_eq!(first.cleanup(5_000, 1).await.unwrap().records_deleted, 1);
    assert_eq!(first.cleanup(5_000, 1).await.unwrap().records_deleted, 1);
}

fn client(id: &str) -> OAuthClient {
    OAuthClient {
        client_id: id.to_owned(),
        display_name: Some("Client".to_owned()),
        client_secret_digest: Some(digest(31)),
        redirect_uris: vec!["https://client.example/callback".to_owned()],
        created_at_ms: 1_000,
        expires_at_ms: None,
        disabled_at_ms: None,
    }
}
fn issue_request() -> OAuthIssueGrantRequest {
    OAuthIssueGrantRequest {
        code_digest: digest(2),
        client_id: "client".to_owned(),
        redirect_uri: "https://client.example/callback".to_owned(),
        resource: "https://resource.example/mcp".to_owned(),
        pkce_challenge: "challenge".to_owned(),
        grant_id: "grant".to_owned(),
        refresh_family_id: "family".to_owned(),
        refresh_token_digest: digest(3),
        refresh_token_expires_at_ms: 8_000,
        refresh_family_expires_at_ms: 9_000,
        access_token: token("access-1", 2_500),
        now_ms: 2_500,
    }
}
fn token(jti: &str, now: i64) -> OAuthTokenMetadata {
    OAuthTokenMetadata {
        jti: jti.to_owned(),
        signing_key_id: "key-1".to_owned(),
        issued_at_ms: now,
        expires_at_ms: now + 2_000,
    }
}
fn validation(jti: &str, scopes: Vec<&str>) -> OAuthValidateAccessTokenRequest {
    OAuthValidateAccessTokenRequest {
        jti: jti.to_owned(),
        client_id: "client".to_owned(),
        grant_id: "grant".to_owned(),
        principal_id: McpPrincipalId::new("alice").unwrap(),
        resource: "https://resource.example/mcp".to_owned(),
        scopes: scopes.into_iter().map(str::to_owned).collect(),
        now_ms: 3_100,
    }
}
fn digest(value: u8) -> Vec<u8> {
    vec![value; 32]
}

#[derive(Default)]
struct CounterEntropy(AtomicU8);
impl McpOAuthEntropy for CounterEntropy {
    fn fill_bytes(&self, output: &mut [u8]) -> mcp::Result<()> {
        let value = self.0.fetch_add(1, Ordering::Relaxed).wrapping_add(7);
        output.fill(value);
        Ok(())
    }
}
