use std::{fmt, str::FromStr as _};

use sqlx::{
    Connection as _, Row as _, Sqlite, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use super::*;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/sqlite");

/// SQLite-backed atomic OAuth authorization persistence.
#[derive(Clone)]
pub struct SqliteOAuthAuthorizationStore {
    pool: SqlitePool,
    config: OAuthAuthorizationStoreConfig,
}

impl fmt::Debug for SqliteOAuthAuthorizationStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SqliteOAuthAuthorizationStore")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl SqliteOAuthAuthorizationStore {
    /// Connect, run the complete MCP migrator, and use default policy.
    ///
    /// # Errors
    ///
    /// Returns an error when connection or migration fails.
    pub async fn connect(database_url: &str) -> OAuthAuthorizationStoreResult<Self> {
        Self::connect_with_config(database_url, OAuthAuthorizationStoreConfig::default()).await
    }

    /// Connect, run the complete MCP migrator, and use explicit policy.
    ///
    /// # Errors
    ///
    /// Returns an error when connection or migration fails.
    pub async fn connect_with_config(
        database_url: &str,
        config: OAuthAuthorizationStoreConfig,
    ) -> OAuthAuthorizationStoreResult<Self> {
        let memory = database_url.contains(":memory:") || database_url.contains("mode=memory");
        let options = SqliteConnectOptions::from_str(database_url)
            .map_err(|_| OAuthAuthorizationStoreError::Unavailable)?
            .create_if_missing(true)
            .foreign_keys(true);
        let mut pools = SqlitePoolOptions::new();
        if memory {
            pools = pools
                .max_connections(1)
                .min_connections(1)
                .max_lifetime(None)
                .idle_timeout(None);
        }
        let pool = pools.connect_with(options).await.map_err(unavailable)?;
        Self::from_pool_with_config(pool, config).await
    }

    /// Run the complete MCP migrator on an existing pool.
    ///
    /// # Errors
    ///
    /// Returns an error when migration fails.
    pub async fn from_pool(pool: SqlitePool) -> OAuthAuthorizationStoreResult<Self> {
        Self::from_pool_with_config(pool, OAuthAuthorizationStoreConfig::default()).await
    }

    /// Run the complete MCP migrator on an existing pool with explicit policy.
    ///
    /// # Errors
    ///
    /// Returns an error when migration fails.
    pub async fn from_pool_with_config(
        pool: SqlitePool,
        config: OAuthAuthorizationStoreConfig,
    ) -> OAuthAuthorizationStoreResult<Self> {
        MIGRATOR
            .run(&pool)
            .await
            .map_err(|error| migration_error(&error))?;
        Ok(Self { pool, config })
    }

    async fn update<T>(
        &self,
        operation: impl FnOnce(
            &mut OAuthState,
            &OAuthAuthorizationStoreConfig,
        ) -> OAuthAuthorizationStoreResult<T>,
    ) -> OAuthAuthorizationStoreResult<T> {
        let mut connection = self.pool.acquire().await.map_err(unavailable)?;
        let mut transaction = connection
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(unavailable)?;
        let mut state = load_state(&mut transaction).await?;
        let previous = state.clone();
        let output = operation(&mut state, &self.config)?;
        persist_changes(&mut transaction, &previous, &state).await?;
        transaction.commit().await.map_err(unavailable)?;
        Ok(output)
    }
}

macro_rules! sqlite_method {
    ($name:ident($($argument:ident: $type:ty),*) -> $output:ty, $body:expr) => {
        fn $name(&self, $($argument: $type),*) -> BoxFuture<OAuthAuthorizationStoreResult<$output>> {
            let store = self.clone();
            Box::pin(async move { store.update(|state, config| ($body)(state, config, $($argument),*)).await })
        }
    };
}

impl OAuthAuthorizationStore for SqliteOAuthAuthorizationStore {
    fn config(&self) -> OAuthAuthorizationStoreConfig {
        self.config.clone()
    }
    sqlite_method!(register_client(client: OAuthClient) -> bool, |s: &mut OAuthState, c: &OAuthAuthorizationStoreConfig, v| s.register_client(c, v));
    sqlite_method!(register_dynamic_client(request: OAuthDynamicClientRegistrationRequest) -> OAuthDynamicClientRegistrationResult, |s: &mut OAuthState, c: &OAuthAuthorizationStoreConfig, v| s.register_dynamic(c, v));
    sqlite_method!(cache_client_metadata(metadata: OAuthCachedClientMetadata) -> (), |s: &mut OAuthState, c: &OAuthAuthorizationStoreConfig, v| s.cache_metadata(c, v));
    sqlite_method!(client_metadata(client_id: String, now_ms: i64) -> Option<OAuthCachedClientMetadata>, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, id: String, n| { validate_time(n)?; Ok(s.current_metadata(&id, n)) });
    sqlite_method!(create_authorization_transaction(transaction: OAuthAuthorizationTransaction) -> bool, |s: &mut OAuthState, c: &OAuthAuthorizationStoreConfig, v| s.create_transaction(c, v));
    sqlite_method!(client(client_id: String, now_ms: i64) -> Option<OAuthClient>, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, id: String, n| { validate_time(n)?; Ok(s.client(&id, n)) });
    sqlite_method!(authorization_context(transaction_id: String, now_ms: i64) -> Option<OAuthAuthorizationContext>, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, id: String, n| { validate_time(n)?; Ok(s.authorization_context(&id, n)) });
    sqlite_method!(consume_browser_correlation(transaction_id: String, correlation_digest: Vec<u8>, now_ms: i64) -> bool, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, id: String, d: Vec<u8>, n| s.consume_correlation(&id, &d, n));
    sqlite_method!(authenticate_authorization_transaction(transaction_id: String, principal_id: McpPrincipalId, now_ms: i64) -> bool, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, id: String, p, n| { validate_time(n)?; Ok(s.authenticate(&id, &p, n)) });
    sqlite_method!(authenticate_and_promote_client(request: OAuthDynamicClientPromotionRequest) -> OAuthClientPromotionResult, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, v: OAuthDynamicClientPromotionRequest| { validate_time(v.now_ms)?; Ok(s.authenticate_and_promote(&v)) });
    sqlite_method!(decide_authorization_transaction(transaction_id: String, decision: OAuthAuthorizationDecision, now_ms: i64) -> bool, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, id: String, d, n| { validate_time(n)?; Ok(s.decide(&id, d, n)) });
    sqlite_method!(remember_authorization_consent(transaction_id: String, now_ms: i64) -> bool, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, id: String, n| { validate_time(n)?; Ok(s.remember_consent(&id, n)) });
    sqlite_method!(issue_authorization_code(code: OAuthAuthorizationCode) -> bool, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, v| s.issue_code(v));
    sqlite_method!(authorization_code_context(code_digest: Vec<u8>, now_ms: i64) -> Option<OAuthAuthorizationCodeContext>, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, d: Vec<u8>, n| s.code_context(&d, n));
    sqlite_method!(redeem_authorization_code(request: OAuthIssueGrantRequest) -> OAuthIssueGrantResult, |s: &mut OAuthState, c: &OAuthAuthorizationStoreConfig, v| s.redeem(c, v));
    sqlite_method!(rotate_refresh_token(request: OAuthRefreshRotationRequest) -> OAuthRefreshRotationResult, |s: &mut OAuthState, c: &OAuthAuthorizationStoreConfig, v| s.rotate(c, v));
    sqlite_method!(refresh_token_context(refresh_token_digest: Vec<u8>, now_ms: i64) -> Option<OAuthRefreshTokenContext>, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, d: Vec<u8>, n| s.refresh_context(&d, n));
    sqlite_method!(revoke(target: OAuthRevocationTarget, now_ms: i64) -> (), |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, t, n| s.revoke(t, n));
    fn validate_access_token(
        &self,
        mut request: OAuthValidateAccessTokenRequest,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<bool>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            validate_time(request.now_ms)?;
            canonical_strings(&mut request.scopes)?;
            let scopes = serde_json::to_string(&request.scopes)
                .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
            let valid: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mcp_oauth_access_tokens a JOIN mcp_oauth_grants g ON g.grant_id = a.grant_id LEFT JOIN mcp_oauth_refresh_families f ON f.family_id = a.refresh_family_id LEFT JOIN mcp_oauth_clients c ON c.client_id = a.client_id LEFT JOIN mcp_oauth_revoked_clients r ON r.client_id = a.client_id WHERE a.jti = ? AND a.client_id = ? AND a.grant_id = ? AND a.principal_id = ? AND a.resource = ? AND a.scopes = ? AND a.revoked_at_ms IS NULL AND a.issued_at_ms <= ? AND a.expires_at_ms > ? AND g.created_at_ms <= ? AND g.revoked_at_ms IS NULL AND (a.refresh_family_id IS NULL OR (f.created_at_ms <= ? AND f.revoked_at_ms IS NULL AND f.expires_at_ms > ?)) AND r.client_id IS NULL AND (c.client_id IS NULL OR (c.created_at_ms <= ? AND c.disabled_at_ms IS NULL AND (c.expires_at_ms IS NULL OR c.expires_at_ms > ?))))")
                .bind(&request.jti).bind(&request.client_id).bind(&request.grant_id).bind(request.principal_id.as_str()).bind(&request.resource).bind(scopes).bind(request.now_ms).bind(request.now_ms).bind(request.now_ms).bind(request.now_ms).bind(request.now_ms).bind(request.now_ms).bind(request.now_ms).fetch_one(&pool).await.map_err(unavailable)?;
            Ok(valid != 0)
        })
    }
    sqlite_method!(put_signing_key_candidate(candidate: OAuthSigningKeyCandidate, bootstrap: bool) -> OAuthSigningKey, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, v, b| s.put_key(v, b));
    sqlite_method!(activate_signing_key(issuer: String, key_id: String, now_ms: i64, verify_until_ms: i64, private_until_ms: i64) -> bool, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, i: String, k: String, n, v, p| s.activate_key(&i, &k, n, v, p));
    sqlite_method!(active_signing_key(issuer: String) -> Option<OAuthSigningKey>, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, i| Ok(s.signing_keys.values().find(|k| k.issuer == i && k.state == OAuthSigningKeyState::Active).cloned()));
    sqlite_method!(verification_signing_keys(issuer: String, now_ms: i64) -> Vec<OAuthSigningKey>, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, i, n| { validate_time(n)?; Ok(s.signing_keys.values().filter(|k| k.issuer == i && (k.state == OAuthSigningKeyState::Active || k.state == OAuthSigningKeyState::Retired && k.verify_until_ms.is_some_and(|d| d > n))).cloned().collect()) });
    sqlite_method!(rewrap_signing_key(issuer: String, key_id: String, encrypted: OAuthEncryptedPrivateKey) -> bool, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, i: String, k: String, e| { validate_encrypted_private_key(&e)?; let Some(key) = s.signing_keys.get_mut(&signing_key_map_key(&i, &k)).filter(|key| key.encrypted_private_key.is_some()) else { return Ok(false); }; key.encrypted_private_key = Some(e); Ok(true) });
    sqlite_method!(cleanup(now_ms: i64, limit: usize) -> OAuthCleanupResult, |s: &mut OAuthState, _c: &OAuthAuthorizationStoreConfig, n, l| { validate_time(n)?; Ok(s.cleanup(n, l)) });
}

#[allow(clippy::too_many_lines)]
async fn load_state(tx: &mut Transaction<'_, Sqlite>) -> OAuthAuthorizationStoreResult<OAuthState> {
    let mut state = OAuthState::default();
    for row in sqlx::query("SELECT client_id, registration_method, display_metadata, client_secret_digest, created_at_ms, expires_at_ms, disabled_at_ms FROM mcp_oauth_clients").fetch_all(&mut **tx).await.map_err(unavailable)? {
        let client_id: String = row.try_get("client_id").map_err(unavailable)?;
        let redirects = sqlx::query_scalar("SELECT redirect_uri FROM mcp_oauth_client_redirect_uris WHERE client_id = ? ORDER BY redirect_uri").bind(&client_id).fetch_all(&mut **tx).await.map_err(unavailable)?;
        state.clients.insert(client_id.clone(), OAuthClient { client_id, registration_method: registration_method(&row.try_get::<String, _>("registration_method").map_err(unavailable)?)?, display_name: row.try_get("display_metadata").map_err(unavailable)?, client_secret_digest: row.try_get("client_secret_digest").map_err(unavailable)?, redirect_uris: redirects, created_at_ms: row.try_get("created_at_ms").map_err(unavailable)?, expires_at_ms: row.try_get("expires_at_ms").map_err(unavailable)?, disabled_at_ms: row.try_get("disabled_at_ms").map_err(unavailable)? });
    }
    for row in sqlx::query("SELECT * FROM mcp_oauth_dynamic_clients")
        .fetch_all(&mut **tx)
        .await
        .map_err(unavailable)?
    {
        state.dynamic_clients.insert(
            row.try_get("client_id").map_err(unavailable)?,
            DynamicClientLifecycle {
                provisional_expires_at_ms: row
                    .try_get("provisional_expires_at_ms")
                    .map_err(unavailable)?,
                inactivity_lifetime_ms: row
                    .try_get("inactivity_lifetime_ms")
                    .map_err(unavailable)?,
                promoted_principal_id: row.try_get("promoted_principal_id").map_err(unavailable)?,
                promoted_at_ms: row.try_get("promoted_at_ms").map_err(unavailable)?,
                last_used_at_ms: row.try_get("last_used_at_ms").map_err(unavailable)?,
            },
        );
    }
    for row in sqlx::query("SELECT * FROM mcp_oauth_client_metadata_cache")
        .fetch_all(&mut **tx)
        .await
        .map_err(unavailable)?
    {
        let document: String = row.try_get("normalized_document").map_err(unavailable)?;
        let value = OAuthCachedClientMetadata {
            snapshot_id: row.try_get("snapshot_id").map_err(unavailable)?,
            client_id: row.try_get("client_id").map_err(unavailable)?,
            document: serde_json::from_str(&document)
                .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
            document_digest: row.try_get("document_digest").map_err(unavailable)?,
            retrieved_at_ms: row.try_get("retrieved_at_ms").map_err(unavailable)?,
            expires_at_ms: row.try_get("expires_at_ms").map_err(unavailable)?,
            retain_until_ms: row.try_get("retain_until_ms").map_err(unavailable)?,
        };
        state.metadata.insert(value.snapshot_id.clone(), value);
    }
    for row in sqlx::query("SELECT * FROM mcp_oauth_authorization_transactions")
        .fetch_all(&mut **tx)
        .await
        .map_err(unavailable)?
    {
        let scopes: String = row.try_get("scopes").map_err(unavailable)?;
        let decision: Option<String> = row.try_get("decision").map_err(unavailable)?;
        let value = OAuthAuthorizationTransaction {
            transaction_id: row.try_get("transaction_id").map_err(unavailable)?,
            client_id: row.try_get("client_id").map_err(unavailable)?,
            client_metadata_snapshot_id: row
                .try_get("client_metadata_snapshot_id")
                .map_err(unavailable)?,
            redirect_uri: row.try_get("redirect_uri").map_err(unavailable)?,
            resource: row.try_get("resource").map_err(unavailable)?,
            scopes: serde_json::from_str(&scopes)
                .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
            pkce_challenge: row.try_get("pkce_challenge").map_err(unavailable)?,
            browser_correlation_digest: row
                .try_get("browser_correlation_digest")
                .map_err(unavailable)?,
            state_reference: row.try_get("state_reference").map_err(unavailable)?,
            principal_id: row.try_get("principal_id").map_err(unavailable)?,
            decision: decision
                .map(|value| authorization_decision(&value))
                .transpose()?,
            created_at_ms: row.try_get("created_at_ms").map_err(unavailable)?,
            expires_at_ms: row.try_get("expires_at_ms").map_err(unavailable)?,
        };
        if row
            .try_get::<Option<i64>, _>("browser_correlation_consumed_at_ms")
            .map_err(unavailable)?
            .is_some()
        {
            state
                .consumed_browser_correlations
                .insert(value.transaction_id.clone());
        }
        state
            .transactions
            .insert(value.transaction_id.clone(), value);
    }
    for row in sqlx::query("SELECT principal_id, client_id, resource, scopes FROM mcp_oauth_consents WHERE revoked_at_ms IS NULL").fetch_all(&mut **tx).await.map_err(unavailable)? {
        let scopes: String = row.try_get("scopes").map_err(unavailable)?;
        let scopes: Vec<String> = serde_json::from_str(&scopes).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
        state.consents.insert(consent_key(row.try_get::<String, _>("principal_id").map_err(unavailable)?.as_str(), row.try_get::<String, _>("client_id").map_err(unavailable)?.as_str(), row.try_get::<String, _>("resource").map_err(unavailable)?.as_str(), &scopes));
    }
    for row in sqlx::query("SELECT * FROM mcp_oauth_authorization_codes")
        .fetch_all(&mut **tx)
        .await
        .map_err(unavailable)?
    {
        let value = OAuthAuthorizationCode {
            code_digest: row.try_get("code_digest").map_err(unavailable)?,
            transaction_id: row.try_get("transaction_id").map_err(unavailable)?,
            created_at_ms: row.try_get("created_at_ms").map_err(unavailable)?,
            expires_at_ms: row.try_get("expires_at_ms").map_err(unavailable)?,
            consumed_at_ms: row.try_get("consumed_at_ms").map_err(unavailable)?,
        };
        state.codes.insert(digest_key(&value.code_digest)?, value);
    }
    for row in sqlx::query("SELECT * FROM mcp_oauth_grants")
        .fetch_all(&mut **tx)
        .await
        .map_err(unavailable)?
    {
        let scopes: String = row.try_get("scopes").map_err(unavailable)?;
        let value = OAuthGrant {
            grant_id: row.try_get("grant_id").map_err(unavailable)?,
            principal_id: row.try_get("principal_id").map_err(unavailable)?,
            client_id: row.try_get("client_id").map_err(unavailable)?,
            resource: row.try_get("resource").map_err(unavailable)?,
            scopes: serde_json::from_str(&scopes)
                .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
            created_at_ms: row.try_get("created_at_ms").map_err(unavailable)?,
            revoked_at_ms: row.try_get("revoked_at_ms").map_err(unavailable)?,
        };
        state.grants.insert(value.grant_id.clone(), value);
    }
    for row in sqlx::query("SELECT * FROM mcp_oauth_refresh_families")
        .fetch_all(&mut **tx)
        .await
        .map_err(unavailable)?
    {
        let value = OAuthRefreshFamily {
            family_id: row.try_get("family_id").map_err(unavailable)?,
            grant_id: row.try_get("grant_id").map_err(unavailable)?,
            created_at_ms: row.try_get("created_at_ms").map_err(unavailable)?,
            expires_at_ms: row.try_get("expires_at_ms").map_err(unavailable)?,
            revoked_at_ms: row.try_get("revoked_at_ms").map_err(unavailable)?,
            revocation_reason: row.try_get("revocation_reason").map_err(unavailable)?,
        };
        state.families.insert(value.family_id.clone(), value);
    }
    for row in sqlx::query("SELECT * FROM mcp_oauth_refresh_tokens")
        .fetch_all(&mut **tx)
        .await
        .map_err(unavailable)?
    {
        let generation: i64 = row.try_get("generation").map_err(unavailable)?;
        let replacement: Option<i64> =
            row.try_get("replacement_generation").map_err(unavailable)?;
        let value = RefreshTokenRecord {
            digest: row.try_get("token_digest").map_err(unavailable)?,
            family_id: row.try_get("family_id").map_err(unavailable)?,
            generation: generation
                .try_into()
                .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
            issued_at_ms: row.try_get("issued_at_ms").map_err(unavailable)?,
            expires_at_ms: row.try_get("expires_at_ms").map_err(unavailable)?,
            consumed_at_ms: row.try_get("consumed_at_ms").map_err(unavailable)?,
            replacement_generation: replacement
                .map(u64::try_from)
                .transpose()
                .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
        };
        state
            .refresh_tokens
            .insert(digest_key(&value.digest)?, value);
    }
    for row in sqlx::query("SELECT * FROM mcp_oauth_access_tokens")
        .fetch_all(&mut **tx)
        .await
        .map_err(unavailable)?
    {
        let scopes: String = row.try_get("scopes").map_err(unavailable)?;
        let value = OAuthAccessTokenRecord {
            jti: row.try_get("jti").map_err(unavailable)?,
            signing_key_id: row.try_get("signing_key_id").map_err(unavailable)?,
            refresh_family_id: row.try_get("refresh_family_id").map_err(unavailable)?,
            grant_id: row.try_get("grant_id").map_err(unavailable)?,
            client_id: row.try_get("client_id").map_err(unavailable)?,
            principal_id: row.try_get("principal_id").map_err(unavailable)?,
            resource: row.try_get("resource").map_err(unavailable)?,
            scopes: serde_json::from_str(&scopes)
                .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
            issued_at_ms: row.try_get("issued_at_ms").map_err(unavailable)?,
            expires_at_ms: row.try_get("expires_at_ms").map_err(unavailable)?,
            revoked_at_ms: row.try_get("revoked_at_ms").map_err(unavailable)?,
        };
        state.access_tokens.insert(value.jti.clone(), value);
    }
    for row in sqlx::query("SELECT * FROM mcp_oauth_signing_keys")
        .fetch_all(&mut **tx)
        .await
        .map_err(unavailable)?
    {
        let public_jwk: String = row.try_get("public_jwk").map_err(unavailable)?;
        let ciphertext: Option<Vec<u8>> = row
            .try_get("encrypted_private_material")
            .map_err(unavailable)?;
        let encrypted_private_key = ciphertext
            .map(|ciphertext| {
                Ok(OAuthEncryptedPrivateKey {
                    wrapping_key_id: row.try_get("wrapping_key_id").map_err(unavailable)?,
                    nonce: row.try_get("encryption_nonce").map_err(unavailable)?,
                    ciphertext,
                })
            })
            .transpose()?;
        let value = OAuthSigningKey {
            issuer: row.try_get("issuer").map_err(unavailable)?,
            key_id: row.try_get("key_id").map_err(unavailable)?,
            algorithm: row.try_get("algorithm").map_err(unavailable)?,
            public_jwk: serde_json::from_str(&public_jwk)
                .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
            encrypted_private_key,
            state: signing_state(&row.try_get::<String, _>("state").map_err(unavailable)?)?,
            created_at_ms: row.try_get("created_at_ms").map_err(unavailable)?,
            activated_at_ms: row.try_get("activated_at_ms").map_err(unavailable)?,
            retired_at_ms: row.try_get("retired_at_ms").map_err(unavailable)?,
            verify_until_ms: row.try_get("verify_until_ms").map_err(unavailable)?,
            private_until_ms: row.try_get("private_until_ms").map_err(unavailable)?,
        };
        validate_signing_key_record(&value)?;
        state
            .signing_keys
            .insert(signing_key_map_key(&value.issuer, &value.key_id), value);
    }
    for row in sqlx::query("SELECT * FROM mcp_oauth_registration_rates")
        .fetch_all(&mut **tx)
        .await
        .map_err(unavailable)?
    {
        let count: i64 = row.try_get("registration_count").map_err(unavailable)?;
        state.registration_rates.insert(
            row.try_get("rate_key").map_err(unavailable)?,
            RegistrationRate {
                window_started_at_ms: row.try_get("window_started_at_ms").map_err(unavailable)?,
                window_expires_at_ms: row.try_get("window_expires_at_ms").map_err(unavailable)?,
                count: count
                    .try_into()
                    .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
            },
        );
    }
    for row in sqlx::query("SELECT client_id, revoked_at_ms FROM mcp_oauth_revoked_clients")
        .fetch_all(&mut **tx)
        .await
        .map_err(unavailable)?
    {
        state.revoked_clients.insert(
            row.try_get("client_id").map_err(unavailable)?,
            row.try_get("revoked_at_ms").map_err(unavailable)?,
        );
    }
    Ok(state)
}

// Each changed logical record is upserted or deleted independently inside the caller's transaction.
async fn persist_changes(
    tx: &mut Transaction<'_, Sqlite>,
    old: &OAuthState,
    new: &OAuthState,
) -> OAuthAuthorizationStoreResult<()> {
    for key in old
        .clients
        .keys()
        .filter(|key| !new.clients.contains_key(*key))
    {
        sqlx::query("DELETE FROM mcp_oauth_clients WHERE client_id = ?")
            .bind(key)
            .execute(&mut **tx)
            .await
            .map_err(unavailable)?;
    }
    for (key, value) in new
        .clients
        .iter()
        .filter(|(key, value)| old.clients.get(*key) != Some(*value))
    {
        sqlx::query("INSERT INTO mcp_oauth_clients (client_id, registration_method, display_metadata, client_secret_digest, created_at_ms, expires_at_ms, disabled_at_ms, redirect_limit) VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(client_id) DO UPDATE SET registration_method=excluded.registration_method, display_metadata=excluded.display_metadata, client_secret_digest=excluded.client_secret_digest, created_at_ms=excluded.created_at_ms, expires_at_ms=excluded.expires_at_ms, disabled_at_ms=excluded.disabled_at_ms, redirect_limit=excluded.redirect_limit").bind(key).bind(registration_method_name(value.registration_method)).bind(&value.display_name).bind(&value.client_secret_digest).bind(value.created_at_ms).bind(value.expires_at_ms).bind(value.disabled_at_ms).bind(i64::try_from(value.redirect_uris.len()).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?).execute(&mut **tx).await.map_err(unavailable)?;
        sqlx::query("DELETE FROM mcp_oauth_client_redirect_uris WHERE client_id = ?")
            .bind(key)
            .execute(&mut **tx)
            .await
            .map_err(unavailable)?;
        for uri in &value.redirect_uris {
            sqlx::query("INSERT INTO mcp_oauth_client_redirect_uris (client_id, redirect_uri) VALUES (?, ?)").bind(key).bind(uri).execute(&mut **tx).await.map_err(unavailable)?;
        }
    }
    persist_simple_maps(tx, old, new).await
}

#[allow(clippy::too_many_lines)]
async fn persist_simple_maps(
    tx: &mut Transaction<'_, Sqlite>,
    old: &OAuthState,
    new: &OAuthState,
) -> OAuthAuthorizationStoreResult<()> {
    macro_rules! sync_map {
        ($old:expr, $new:expr, $delete:literal, $upsert:ident) => {{
            for key in $old.keys().filter(|key| !$new.contains_key(*key)) {
                sqlx::query($delete)
                    .bind(key)
                    .execute(&mut **tx)
                    .await
                    .map_err(unavailable)?;
            }
            for (key, value) in $new
                .iter()
                .filter(|(key, value)| $old.get(*key) != Some(*value))
            {
                $upsert(tx, key, value).await?;
            }
        }};
    }
    sync_map!(
        old.dynamic_clients,
        new.dynamic_clients,
        "DELETE FROM mcp_oauth_dynamic_clients WHERE client_id = ?",
        upsert_dynamic
    );
    for (key, value) in new
        .metadata
        .iter()
        .filter(|(key, value)| old.metadata.get(*key) != Some(*value))
    {
        upsert_metadata(tx, key, value).await?;
    }
    for (key, value) in new
        .transactions
        .iter()
        .filter(|(key, value)| old.transactions.get(*key) != Some(*value))
    {
        upsert_transaction(
            tx,
            key,
            value,
            new.consumed_browser_correlations.contains(key),
        )
        .await?;
    }
    // Correlation consumption can change without changing the transaction value.
    for key in new
        .consumed_browser_correlations
        .difference(&old.consumed_browser_correlations)
    {
        sqlx::query("UPDATE mcp_oauth_authorization_transactions SET browser_correlation_consumed_at_ms = created_at_ms WHERE transaction_id = ? AND browser_correlation_consumed_at_ms IS NULL").bind(key).execute(&mut **tx).await.map_err(unavailable)?;
    }
    for (key, value) in &old.codes {
        if !new.codes.contains_key(key) {
            sqlx::query("DELETE FROM mcp_oauth_authorization_codes WHERE code_digest = ?")
                .bind(&value.code_digest)
                .execute(&mut **tx)
                .await
                .map_err(unavailable)?;
        }
    }
    for (key, value) in new
        .codes
        .iter()
        .filter(|(key, value)| old.codes.get(*key) != Some(*value))
    {
        upsert_code(tx, key, value).await?;
    }
    for key in old
        .transactions
        .keys()
        .filter(|key| !new.transactions.contains_key(*key))
    {
        sqlx::query("DELETE FROM mcp_oauth_authorization_transactions WHERE transaction_id = ?")
            .bind(key)
            .execute(&mut **tx)
            .await
            .map_err(unavailable)?;
    }
    for key in old
        .metadata
        .keys()
        .filter(|key| !new.metadata.contains_key(*key))
    {
        sqlx::query("DELETE FROM mcp_oauth_client_metadata_cache WHERE snapshot_id = ?")
            .bind(key)
            .execute(&mut **tx)
            .await
            .map_err(unavailable)?;
    }
    sync_map!(
        old.grants,
        new.grants,
        "DELETE FROM mcp_oauth_grants WHERE grant_id = ?",
        upsert_grant
    );
    for (key, value) in new
        .families
        .iter()
        .filter(|(key, value)| old.families.get(*key) != Some(*value))
    {
        upsert_family(tx, key, value).await?;
    }
    for (key, value) in &old.refresh_tokens {
        if !new.refresh_tokens.contains_key(key) {
            sqlx::query("DELETE FROM mcp_oauth_refresh_tokens WHERE token_digest = ?")
                .bind(&value.digest)
                .execute(&mut **tx)
                .await
                .map_err(unavailable)?;
        }
    }
    for (key, value) in new
        .refresh_tokens
        .iter()
        .filter(|(key, value)| old.refresh_tokens.get(*key) != Some(*value))
    {
        upsert_refresh(tx, key, value).await?;
    }
    sync_map!(
        old.access_tokens,
        new.access_tokens,
        "DELETE FROM mcp_oauth_access_tokens WHERE jti = ?",
        upsert_access
    );
    for key in old
        .families
        .keys()
        .filter(|key| !new.families.contains_key(*key))
    {
        sqlx::query("DELETE FROM mcp_oauth_refresh_families WHERE family_id = ?")
            .bind(key)
            .execute(&mut **tx)
            .await
            .map_err(unavailable)?;
    }
    for (map_key, value) in &old.signing_keys {
        if !new.signing_keys.contains_key(map_key) {
            sqlx::query("DELETE FROM mcp_oauth_signing_keys WHERE issuer = ? AND key_id = ?")
                .bind(&value.issuer)
                .bind(&value.key_id)
                .execute(&mut **tx)
                .await
                .map_err(unavailable)?;
        }
    }
    for active in [false, true] {
        for (key, value) in new.signing_keys.iter().filter(|(key, value)| {
            old.signing_keys.get(*key) != Some(*value)
                && (value.state == OAuthSigningKeyState::Active) == active
        }) {
            upsert_signing(tx, key, value).await?;
        }
    }
    sync_map!(
        old.registration_rates,
        new.registration_rates,
        "DELETE FROM mcp_oauth_registration_rates WHERE rate_key = ?",
        upsert_rate
    );
    for (key, value) in new
        .revoked_clients
        .iter()
        .filter(|(key, value)| old.revoked_clients.get(*key) != Some(*value))
    {
        sqlx::query(
            "INSERT INTO mcp_oauth_revoked_clients VALUES (?, ?) ON CONFLICT(client_id) DO NOTHING",
        )
        .bind(key)
        .bind(value)
        .execute(&mut **tx)
        .await
        .map_err(unavailable)?;
    }
    for key in new.consents.difference(&old.consents) {
        let (principal, client, resource, scopes): (String, String, String, Vec<String>) =
            serde_json::from_str(key).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
        sqlx::query("INSERT INTO mcp_oauth_consents VALUES (?, ?, ?, ?, 0, 0, NULL) ON CONFLICT(principal_id,client_id,resource,scopes) DO UPDATE SET revoked_at_ms=NULL").bind(principal).bind(client).bind(resource).bind(serde_json::to_string(&scopes).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?).execute(&mut **tx).await.map_err(unavailable)?;
    }
    Ok(())
}

async fn upsert_dynamic(
    tx: &mut Transaction<'_, Sqlite>,
    key: &String,
    v: &DynamicClientLifecycle,
) -> OAuthAuthorizationStoreResult<()> {
    sqlx::query("INSERT INTO mcp_oauth_dynamic_clients VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(client_id) DO UPDATE SET provisional_expires_at_ms=excluded.provisional_expires_at_ms, inactivity_lifetime_ms=excluded.inactivity_lifetime_ms, promoted_principal_id=excluded.promoted_principal_id, promoted_at_ms=excluded.promoted_at_ms, last_used_at_ms=excluded.last_used_at_ms").bind(key).bind(v.provisional_expires_at_ms).bind(v.inactivity_lifetime_ms).bind(&v.promoted_principal_id).bind(v.promoted_at_ms).bind(v.last_used_at_ms).execute(&mut **tx).await.map_err(unavailable)?;
    Ok(())
}
async fn upsert_metadata(
    tx: &mut Transaction<'_, Sqlite>,
    _: &String,
    v: &OAuthCachedClientMetadata,
) -> OAuthAuthorizationStoreResult<()> {
    sqlx::query("INSERT INTO mcp_oauth_client_metadata_cache VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(snapshot_id) DO UPDATE SET client_id=excluded.client_id, normalized_document=excluded.normalized_document, document_digest=excluded.document_digest, retrieved_at_ms=excluded.retrieved_at_ms, expires_at_ms=excluded.expires_at_ms, retain_until_ms=excluded.retain_until_ms").bind(&v.snapshot_id).bind(&v.client_id).bind(serde_json::to_string(&v.document).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?).bind(&v.document_digest).bind(v.retrieved_at_ms).bind(v.expires_at_ms).bind(v.retain_until_ms).execute(&mut **tx).await.map_err(unavailable)?;
    Ok(())
}
async fn upsert_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    _: &String,
    v: &OAuthAuthorizationTransaction,
    consumed: bool,
) -> OAuthAuthorizationStoreResult<()> {
    sqlx::query("INSERT INTO mcp_oauth_authorization_transactions (transaction_id,client_id,redirect_uri,resource,scopes,pkce_challenge,browser_correlation_digest,browser_correlation_consumed_at_ms,state_reference,principal_id,decision,created_at_ms,expires_at_ms,client_metadata_snapshot_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(transaction_id) DO UPDATE SET principal_id=excluded.principal_id, decision=excluded.decision, browser_correlation_consumed_at_ms=excluded.browser_correlation_consumed_at_ms").bind(&v.transaction_id).bind(&v.client_id).bind(&v.redirect_uri).bind(&v.resource).bind(serde_json::to_string(&v.scopes).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?).bind(&v.pkce_challenge).bind(&v.browser_correlation_digest).bind(consumed.then_some(v.created_at_ms)).bind(&v.state_reference).bind(&v.principal_id).bind(v.decision.map(authorization_decision_name)).bind(v.created_at_ms).bind(v.expires_at_ms).bind(&v.client_metadata_snapshot_id).execute(&mut **tx).await.map_err(unavailable)?;
    Ok(())
}
async fn upsert_code(
    tx: &mut Transaction<'_, Sqlite>,
    _: &String,
    v: &OAuthAuthorizationCode,
) -> OAuthAuthorizationStoreResult<()> {
    sqlx::query("INSERT INTO mcp_oauth_authorization_codes VALUES (?, ?, ?, ?, ?) ON CONFLICT(code_digest) DO UPDATE SET consumed_at_ms=excluded.consumed_at_ms").bind(&v.code_digest).bind(&v.transaction_id).bind(v.created_at_ms).bind(v.expires_at_ms).bind(v.consumed_at_ms).execute(&mut **tx).await.map_err(unavailable)?;
    Ok(())
}
async fn upsert_grant(
    tx: &mut Transaction<'_, Sqlite>,
    _: &String,
    v: &OAuthGrant,
) -> OAuthAuthorizationStoreResult<()> {
    sqlx::query("INSERT INTO mcp_oauth_grants VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(grant_id) DO UPDATE SET revoked_at_ms=excluded.revoked_at_ms").bind(&v.grant_id).bind(&v.principal_id).bind(&v.client_id).bind(&v.resource).bind(serde_json::to_string(&v.scopes).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?).bind(v.created_at_ms).bind(v.revoked_at_ms).execute(&mut **tx).await.map_err(unavailable)?;
    Ok(())
}
async fn upsert_family(
    tx: &mut Transaction<'_, Sqlite>,
    _: &String,
    v: &OAuthRefreshFamily,
) -> OAuthAuthorizationStoreResult<()> {
    sqlx::query("INSERT INTO mcp_oauth_refresh_families VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(family_id) DO UPDATE SET revoked_at_ms=excluded.revoked_at_ms, revocation_reason=excluded.revocation_reason").bind(&v.family_id).bind(&v.grant_id).bind(v.created_at_ms).bind(v.expires_at_ms).bind(v.revoked_at_ms).bind(&v.revocation_reason).execute(&mut **tx).await.map_err(unavailable)?;
    Ok(())
}
async fn upsert_refresh(
    tx: &mut Transaction<'_, Sqlite>,
    _: &String,
    v: &RefreshTokenRecord,
) -> OAuthAuthorizationStoreResult<()> {
    sqlx::query("INSERT INTO mcp_oauth_refresh_tokens VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(token_digest) DO UPDATE SET consumed_at_ms=excluded.consumed_at_ms, replacement_generation=excluded.replacement_generation").bind(&v.digest).bind(&v.family_id).bind(i64::try_from(v.generation).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?).bind(v.issued_at_ms).bind(v.expires_at_ms).bind(v.consumed_at_ms).bind(v.replacement_generation.map(i64::try_from).transpose().map_err(|_| OAuthAuthorizationStoreError::InvalidData)?).execute(&mut **tx).await.map_err(unavailable)?;
    Ok(())
}
async fn upsert_access(
    tx: &mut Transaction<'_, Sqlite>,
    _: &String,
    v: &OAuthAccessTokenRecord,
) -> OAuthAuthorizationStoreResult<()> {
    sqlx::query("INSERT INTO mcp_oauth_access_tokens VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(jti) DO UPDATE SET revoked_at_ms=excluded.revoked_at_ms").bind(&v.jti).bind("").bind(&v.signing_key_id).bind(&v.refresh_family_id).bind(&v.grant_id).bind(&v.client_id).bind(&v.principal_id).bind(&v.resource).bind(serde_json::to_string(&v.scopes).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?).bind(v.issued_at_ms).bind(v.expires_at_ms).bind(v.revoked_at_ms).execute(&mut **tx).await.map_err(unavailable)?;
    Ok(())
}
async fn upsert_signing(
    tx: &mut Transaction<'_, Sqlite>,
    _: &String,
    v: &OAuthSigningKey,
) -> OAuthAuthorizationStoreResult<()> {
    let encrypted = v.encrypted_private_key.as_ref();
    sqlx::query("INSERT INTO mcp_oauth_signing_keys VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(issuer,key_id) DO UPDATE SET encrypted_private_material=excluded.encrypted_private_material, wrapping_key_id=excluded.wrapping_key_id, encryption_nonce=excluded.encryption_nonce, state=excluded.state, activated_at_ms=excluded.activated_at_ms, retired_at_ms=excluded.retired_at_ms, verify_until_ms=excluded.verify_until_ms, private_until_ms=excluded.private_until_ms").bind(&v.issuer).bind(&v.key_id).bind(&v.algorithm).bind(serde_json::to_string(&v.public_jwk).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?).bind(encrypted.map(|e| &e.ciphertext)).bind(encrypted.map(|e| &e.wrapping_key_id)).bind(encrypted.map(|e| &e.nonce)).bind(signing_state_name(v.state)).bind(v.created_at_ms).bind(v.activated_at_ms).bind(v.retired_at_ms).bind(v.verify_until_ms).bind(v.private_until_ms).execute(&mut **tx).await.map_err(unavailable)?;
    Ok(())
}
async fn upsert_rate(
    tx: &mut Transaction<'_, Sqlite>,
    key: &String,
    v: &RegistrationRate,
) -> OAuthAuthorizationStoreResult<()> {
    sqlx::query("INSERT INTO mcp_oauth_registration_rates VALUES (?, ?, ?, ?) ON CONFLICT(rate_key) DO UPDATE SET window_started_at_ms=excluded.window_started_at_ms, window_expires_at_ms=excluded.window_expires_at_ms, registration_count=excluded.registration_count").bind(key).bind(v.window_started_at_ms).bind(v.window_expires_at_ms).bind(i64::try_from(v.count).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?).execute(&mut **tx).await.map_err(unavailable)?;
    Ok(())
}

fn registration_method(
    value: &str,
) -> OAuthAuthorizationStoreResult<OAuthClientRegistrationMethod> {
    match value {
        "preconfigured" => Ok(OAuthClientRegistrationMethod::Preconfigured),
        "dynamic" => Ok(OAuthClientRegistrationMethod::Dynamic),
        "client_metadata_document" => Ok(OAuthClientRegistrationMethod::ClientMetadataDocument),
        _ => Err(OAuthAuthorizationStoreError::InvalidData),
    }
}
const fn registration_method_name(value: OAuthClientRegistrationMethod) -> &'static str {
    match value {
        OAuthClientRegistrationMethod::Preconfigured => "preconfigured",
        OAuthClientRegistrationMethod::Dynamic => "dynamic",
        OAuthClientRegistrationMethod::ClientMetadataDocument => "client_metadata_document",
    }
}
fn authorization_decision(
    value: &str,
) -> OAuthAuthorizationStoreResult<OAuthAuthorizationDecision> {
    match value {
        "approved" => Ok(OAuthAuthorizationDecision::Approved),
        "denied" => Ok(OAuthAuthorizationDecision::Denied),
        _ => Err(OAuthAuthorizationStoreError::InvalidData),
    }
}
const fn authorization_decision_name(value: OAuthAuthorizationDecision) -> &'static str {
    match value {
        OAuthAuthorizationDecision::Approved => "approved",
        OAuthAuthorizationDecision::Denied => "denied",
    }
}
fn signing_state(value: &str) -> OAuthAuthorizationStoreResult<OAuthSigningKeyState> {
    match value {
        "pending" => Ok(OAuthSigningKeyState::Pending),
        "active" => Ok(OAuthSigningKeyState::Active),
        "retired" => Ok(OAuthSigningKeyState::Retired),
        _ => Err(OAuthAuthorizationStoreError::InvalidData),
    }
}
const fn signing_state_name(value: OAuthSigningKeyState) -> &'static str {
    match value {
        OAuthSigningKeyState::Pending => "pending",
        OAuthSigningKeyState::Active => "active",
        OAuthSigningKeyState::Retired => "retired",
    }
}

fn unavailable<E>(_: E) -> OAuthAuthorizationStoreError {
    OAuthAuthorizationStoreError::Unavailable
}
const fn migration_error(error: &sqlx::migrate::MigrateError) -> OAuthAuthorizationStoreError {
    use sqlx::migrate::MigrateError;
    match error {
        MigrateError::VersionMissing(_)
        | MigrateError::VersionMismatch(_)
        | MigrateError::VersionNotPresent(_)
        | MigrateError::VersionTooOld(_, _)
        | MigrateError::VersionTooNew(_, _)
        | MigrateError::Dirty(_) => OAuthAuthorizationStoreError::InvalidData,
        _ => OAuthAuthorizationStoreError::Unavailable,
    }
}
