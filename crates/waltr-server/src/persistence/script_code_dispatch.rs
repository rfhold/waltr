//! Outer code-call association. An existing identity never authorizes another dispatch.
use super::PersistencePool;
use crate::routine::{
    code_mode::{CodeDispatch, CodeDispatchStore},
    ports::{PortError, PortFuture},
};

/// SQLite/PostgreSQL implementation, sharing the initialized Waltr pool.
#[derive(Clone)]
pub struct SqlCodeDispatchStore {
    pool: PersistencePool,
}
impl std::fmt::Debug for SqlCodeDispatchStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlCodeDispatchStore")
            .finish_non_exhaustive()
    }
}
impl SqlCodeDispatchStore {
    pub(super) fn new(pool: PersistencePool) -> Self {
        Self { pool }
    }
}
macro_rules! backend {
    ($pool:expr, |$p:ident, $s:ident| $body:block) => {
        match $pool {
            PersistencePool::Sqlite($p) => {
                let $s = "";
                $body
            }
            PersistencePool::PostgreSql($p) => {
                let $s = "waltr.";
                $body
            }
        }
    };
}
impl CodeDispatchStore for SqlCodeDispatchStore {
    fn associate(&self, dispatch: CodeDispatch) -> PortFuture<()> {
        let pool = self.pool.clone();
        Box::pin(async move {
            dispatch.validate()?;
            let d = dispatch;
            let rows = backend!(&pool, |p, s| {
                sqlx::query(&format!("INSERT INTO {s}script_code_dispatch (principal,session_id,incarnation,tool_call_id,source_revision,invocation_id) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING"))
                    .bind(&d.principal).bind(&d.session_id).bind(&d.incarnation).bind(&d.tool_call_id).bind(&d.source_revision).bind(&d.invocation_id)
                    .execute(p).await.map_err(|_| PortError::Unavailable)?.rows_affected()
            });
            if rows == 1 {
                Ok(())
            } else {
                Err(PortError::Conflict)
            }
        })
    }
    fn complete(&self, dispatch: CodeDispatch, outcome: serde_json::Value) -> PortFuture<()> {
        let pool = self.pool.clone();
        Box::pin(async move {
            dispatch.validate()?;
            let outcome = serde_json::to_string(&outcome).map_err(|_| PortError::Invalid)?;
            if outcome.len() > 65536 {
                return Err(PortError::Invalid);
            }
            let d = dispatch;
            let rows = backend!(&pool, |p, s| {
                sqlx::query(&format!("UPDATE {s}script_code_dispatch SET outcome=$7,state='terminal' WHERE principal=$1 AND session_id=$2 AND incarnation=$3 AND tool_call_id=$4 AND source_revision=$5 AND invocation_id=$6 AND state='associated' AND outcome IS NULL"))
                    .bind(&d.principal).bind(&d.session_id).bind(&d.incarnation).bind(&d.tool_call_id).bind(&d.source_revision).bind(&d.invocation_id).bind(outcome)
                    .execute(p).await.map_err(|_| PortError::Unavailable)?.rows_affected()
            });
            if rows == 1 {
                Ok(())
            } else {
                Err(PortError::Conflict)
            }
        })
    }
}

#[cfg(all(test, feature = "test"))]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a guarded fresh loopback code_s8_ephemeral PostgreSQL fixture"]
    async fn code_s8_postgres_thirteen_preserves_prior_rows_and_exact_dispatch_cas() {
        let raw = std::env::var("CODE_S8_POSTGRES_URL").expect("explicit fresh fixture URL");
        let mut url = reqwest::Url::parse(&raw).unwrap();
        assert!(matches!(url.scheme(), "postgres" | "postgresql"));
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert!(url.port().is_some());
        assert_eq!(url.username(), "code_s8");
        assert_eq!(url.path(), "/code_s8_ephemeral");
        assert!(url.query().is_none() && url.fragment().is_none());
        url.query_pairs_mut().append_pair("sslmode", "disable");
        let pool = sqlx::PgPool::connect(url.as_str()).await.unwrap();
        sqlx::query("CREATE SCHEMA waltr")
            .execute(&pool)
            .await
            .expect("fresh schema required; never reset a database");
        let mut prior = sqlx::migrate!("migrations/postgresql");
        prior.migrations = std::borrow::Cow::Owned(
            prior
                .iter()
                .filter(|migration| migration.version != 13)
                .cloned()
                .collect(),
        );
        prior.run(&pool).await.unwrap();
        use ::mcp::oauth::{
            OAuthAuthorizationStore, OAuthAuthorizationStoreConfig, OAuthClient,
            OAuthClientRegistrationMethod, PostgresOAuthAuthorizationStore,
        };
        use waltr_agent::{
            FilesystemSessionStore, SessionCoordinator, SessionCreateRequest, SessionId,
            SessionStore,
        };
        let history = super::super::script_invocation::SqlInvocationStore::new(
            PersistencePool::PostgreSql(pool.clone()),
        );
        let before = super::super::test_history::seed(
            PersistencePool::PostgreSql(pool.clone()),
            "s8-prior-pg",
        )
        .await;
        let mcp = PostgresOAuthAuthorizationStore::from_pool_with_config(
            pool.clone(),
            OAuthAuthorizationStoreConfig::default(),
        )
        .await
        .unwrap();
        let client = OAuthClient {
            client_id: "s8-prior-client".into(),
            registration_method: OAuthClientRegistrationMethod::Preconfigured,
            display_name: Some("Existing client 界".into()),
            client_secret_digest: None,
            redirect_uris: vec!["https://example.invalid/callback".into()],
            created_at_ms: 1,
            expires_at_ms: None,
            disabled_at_ms: None,
        };
        assert!(mcp.register_client(client.clone()).await.unwrap());
        let root = crate::test_support::temp_root();
        let filesystem = FilesystemSessionStore::new(&root);
        let sessions = SessionCoordinator::new(filesystem.clone());
        let created = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .unwrap();
        let id = created.id().clone();
        let original = serde_json::to_vec(&filesystem.load(&id).await.unwrap()).unwrap();
        sessions.shutdown().await;
        sqlx::migrate!("migrations/postgresql")
            .run(&pool)
            .await
            .unwrap();
        assert_eq!(
            before,
            super::super::test_history::snapshot(&history, "s8-prior-pg").await
        );
        assert_eq!(
            mcp.client(client.client_id.clone(), 2).await.unwrap(),
            Some(client)
        );
        assert_eq!(
            original,
            serde_json::to_vec(&FilesystemSessionStore::new(&root).load(&id).await.unwrap())
                .unwrap()
        );
        let store = SqlCodeDispatchStore::new(PersistencePool::PostgreSql(pool.clone()));
        let dispatch = CodeDispatch {
            principal: "actual-owner".into(),
            session_id: "actual-session".into(),
            incarnation: "durable-generation".into(),
            tool_call_id: "stable-call".into(),
            source_revision: "pinned-revision".into(),
            invocation_id: "reserved-invocation".into(),
        };
        store.associate(dispatch.clone()).await.unwrap();
        assert!(matches!(
            store.associate(dispatch.clone()).await,
            Err(PortError::Conflict)
        ));
        let mut changed = dispatch.clone();
        changed.source_revision = "different-revision".into();
        assert!(matches!(
            store
                .complete(changed, serde_json::json!({"status":"succeeded"}))
                .await,
            Err(PortError::Conflict)
        ));
        let outcome = serde_json::json!({"status":"uncertain","cleanupComplete":false});
        store
            .complete(dispatch.clone(), outcome.clone())
            .await
            .unwrap();
        assert!(matches!(
            store.complete(dispatch.clone(), outcome.clone()).await,
            Err(PortError::Conflict)
        ));
        assert!(matches!(
            store.associate(dispatch).await,
            Err(PortError::Conflict)
        ));
        let row: (String, String) =
            sqlx::query_as("SELECT state,outcome FROM waltr.script_code_dispatch")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "terminal");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&row.1).unwrap(),
            outcome
        );
        pool.close().await;
    }
}
