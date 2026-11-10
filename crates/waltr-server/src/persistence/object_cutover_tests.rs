use super::*;

#[tokio::test]
async fn fresh_schema_has_only_payload_references() {
    let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap()).await.unwrap();
    let pool = persistence.sqlite_pool().unwrap();
    for (table, removed, required) in [("script_program_revisions", "envelope", "object_sha256"), ("script_text_results", "content", "object_sha256")] {
        let columns: Vec<(i64, String, String, i64, Option<String>, i64)> = sqlx::query_as(&format!("PRAGMA table_info({table})")).fetch_all(pool).await.unwrap();
        assert!(!columns.iter().any(|r| r.1 == removed));
        assert!(columns.iter().any(|r| r.1 == required));
    }
}

#[tokio::test]
async fn populated_payload_preflight_preserves_payload_history_and_unrelated_sentinels() {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    let mut prior = sqlx::migrate!("migrations/sqlite");
    prior.migrations = std::borrow::Cow::Owned(prior.iter().filter(|m| m.version < 16).cloned().collect());
    prior.run(&pool).await.unwrap();
    sqlx::raw_sql("CREATE TABLE unrelated_object_cutover_sentinel (value TEXT); INSERT INTO unrelated_object_cutover_sentinel VALUES ('credentials-access-sessions-external-receipts'); INSERT INTO script_program_revisions(id,api_version,engine,envelope,name) VALUES('old','v1','engine','retained exact source','test');").execute(&pool).await.unwrap();
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations").fetch_one(&pool).await.unwrap();
    assert_eq!(migrations::sqlite(&pool).await, Err(PersistenceError::IncompatibleRoutineState));
    let body: String = sqlx::query_scalar("SELECT envelope FROM script_program_revisions").fetch_one(&pool).await.unwrap();
    assert_eq!(body, "retained exact source");
    let sentinel: String = sqlx::query_scalar("SELECT value FROM unrelated_object_cutover_sentinel").fetch_one(&pool).await.unwrap();
    assert_eq!(sentinel, "credentials-access-sessions-external-receipts");
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations").fetch_one(&pool).await.unwrap();
    assert_eq!(before, after);
}
