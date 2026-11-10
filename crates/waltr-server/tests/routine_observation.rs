//! Durable observation partitions and snapshot/change continuity.
use sha2::{Digest as _, Sha256};
use waltr_server::{
    persistence::{DatabaseConfig, Persistence},
    routine::{
        definition::API_VERSION,
        history::{
            Admission, Caller, Operation, OperationStatus, Origin, Ownership, Revision, Status,
        },
        observation::{Observation, ObservationScope, ObservationStore},
        ports::InvocationStore,
        text_results::{InvocationTextScope, TextResultStore, TextSource, TextSourceFuture},
    },
};
fn revision_id(engine: &str, envelope: &str) -> String {
    Sha256::digest(format!("{API_VERSION}\0{engine}\0{envelope}"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn admission(id: &str, principal: &str) -> Admission {
    let engine = "test-js;test-engine;abi=1";
    let envelope = r#"{"apiVersion":"waltr.routine/v1","kind":"Routine","name":"test","spec":{"source":"export default async function(input) { return input; }"}}"#;
    Admission {
        id: id.into(),
        revision: Revision {
            id: revision_id(engine, envelope),
            api_version: API_VERSION.into(),
            engine: engine.into(),
            envelope: envelope.into(),
            name: "test".into(),
        },
        caller: Caller {
            principal: principal.into(),
            origin: Origin::Manual,
            ownership: Ownership::Detached,
            correlation: "not-source".into(),
        },
        entrypoint: "run".into(),
        snapshot: None,
        admitted_ms: 100,
    }
}
fn scope(principal: &str, source: Option<&str>) -> ObservationScope {
    ObservationScope {
        principal: principal.into(),
        source_session: source.map(str::to_owned),
    }
}
struct Source(bool);
impl TextSource for Source {
    fn read<'a>(&'a mut self, destination: &'a mut [u8]) -> TextSourceFuture<'a> {
        Box::pin(async move {
            if self.0 {
                Ok(0)
            } else {
                self.0 = true;
                destination[..4].copy_from_slice(b"text");
                Ok(4)
            }
        })
    }
}

#[tokio::test]
async fn routine_observation_authorizes_before_limit_and_resumes_fixed_snapshot() {
    let persistence = Persistence::initialize(
        DatabaseConfig::from_urls("sqlite::memory:", "sqlite::memory:").unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.script_invocation_store();
    store
        .admit_with_source(admission("foreign", "other"), Some("source".into()))
        .await
        .unwrap();
    store
        .admit_with_source(admission("first", "owner"), Some("source".into()))
        .await
        .unwrap();
    store
        .admit_with_source(
            admission("different-source", "owner"),
            Some("elsewhere".into()),
        )
        .await
        .unwrap();
    store
        .admit_with_source(admission("second", "owner"), Some("source".into()))
        .await
        .unwrap();
    let scoped = scope("owner", Some("source"));
    let first = ObservationStore::list(&store, scoped.clone(), 0, 1)
        .await
        .unwrap();
    assert_eq!(first[0].invocation.id, "first");
    assert_eq!(
        ObservationStore::list(&store, scoped.clone(), first[0].invocation.cursor, 1)
            .await
            .unwrap()[0]
            .invocation
            .id,
        "second"
    );
    assert!(
        ObservationStore::get(&store, scope("other", None), "first".into())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        ObservationStore::operations(&store, scope("other", None), "first".into(), 0, 1)
            .await
            .is_err()
    );
    let initial = store.page(scoped.clone(), None, 0, true, 1).await.unwrap();
    assert_eq!(initial.rows.len(), 1);
    assert!(!initial.complete);
    let high = initial.high_water;
    store.running("first".into(), 100).await.unwrap();
    store
        .dispatch_intent(Operation {
            version: 1,
            cursor: 0,
            id: "operation".into(),
            invocation: "first".into(),
            name: "safe".into(),
            status: OperationStatus::Dispatched,
            arguments: serde_json::json!({"redacted":true}),
            result: None,
            updated_ms: 100,
        })
        .await
        .unwrap();
    store
        .operation_done("operation".into(), OperationStatus::Completed, None, 100)
        .await
        .unwrap();
    let descriptor = persistence
        .script_text_result_store()
        .publish(
            InvocationTextScope::new("owner".into(), "first".into()).unwrap(),
            "selected".into(),
            None,
            Box::new(Source(false)),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let latest = ObservationStore::get(&store, scoped.clone(), "first".into())
        .await
        .unwrap()
        .unwrap();
    assert!(latest.observation_revision > high);
    assert_eq!(latest.text_results, [descriptor]);
    let next = store
        .page(scoped.clone(), Some(high), initial.position, true, 1)
        .await
        .unwrap();
    assert!(next.complete);
    assert_eq!(next.position, high);
    assert!(
        matches!(&next.rows[0],Observation::Invocation(v) if v.invocation.id=="second" && v.invocation.status==Status::Admitted)
    );
    let mut position = high;
    let mut revisions = Vec::new();
    loop {
        let page = store
            .page(scoped.clone(), Some(high), position, false, 1)
            .await
            .unwrap();
        for row in page.rows {
            revisions.push(match row {
                Observation::Invocation(v) => v.observation_revision,
                Observation::Operation(v) => v.observation_revision,
            });
        }
        position = page.position;
        if page.complete {
            break;
        }
    }
    assert_eq!(revisions.len(), 4);
    assert!(revisions.windows(2).all(|r| r[0] < r[1]));
    let before = latest.observation_revision;
    store
        .terminal("first".into(), Status::Failed, None, 100)
        .await
        .unwrap();
    let terminal = ObservationStore::get(&store, scoped, "first".into())
        .await
        .unwrap()
        .unwrap();
    assert!(terminal.observation_revision > before);
    assert_eq!(terminal.text_results.len(), 1);
    assert_eq!(terminal.revision.name, "test");
}

#[tokio::test]
async fn routine_sqlite_backfill_preserves_rows_and_restart_cursor_key() {
    use sqlx::Executor as _;
    let path = std::env::temp_dir().join("opencode").join(format!(
        "routine-observation-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    let url = format!("sqlite:{}?mode=rwc", path.display());
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let migrations = sqlx::migrate!("./migrations/sqlite");
    let prior = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            migrations
                .iter()
                .filter(|m| m.version <= 10)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    prior.run(&pool).await.unwrap();
    let seed = admission("before-eleven", "owner");
    let revision = &seed.revision;
    sqlx::query("INSERT INTO script_program_revisions(id,api_version,engine,envelope,name) VALUES($1,$2,$3,$4,$5)").bind(&revision.id).bind(&revision.api_version).bind(&revision.engine).bind(&revision.envelope).bind(&revision.name).execute(&pool).await.unwrap();
    let record = serde_json::to_string(&waltr_server::routine::history::Invocation {
        version: 1,
        cursor: 0,
        id: seed.id.clone(),
        revision: revision.id.clone(),
        caller: seed.caller,
        entrypoint: seed.entrypoint,
        snapshot: None,
        status: Status::Admitted,
        admitted_ms: 100,
        updated_ms: 100,
        result: None,
    })
    .unwrap();
    sqlx::query("INSERT INTO script_invocations(id,revision,record,status,admitted_ms,updated_ms,result) VALUES($1,$2,$3,'admitted',100,100,NULL)").bind(&seed.id).bind(&revision.id).bind(&record).execute(&pool).await.unwrap();
    pool.close().await;
    let persistence =
        Persistence::initialize(DatabaseConfig::from_urls(&url, "sqlite::memory:").unwrap())
            .await
            .unwrap();
    let store = persistence.script_invocation_store();
    let owner = scope("owner", None);
    let key = store.cursor_key().await.unwrap();
    let row = ObservationStore::get(&store, owner.clone(), seed.id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.invocation.id, seed.id);
    assert!(row.observation_revision > 0);
    assert!(row.public_definition.is_none());
    let page = store.page(owner.clone(), None, 0, true, 100).await.unwrap();
    let position = page.position;
    store.running(seed.id.clone(), 100).await.unwrap();
    let reopened =
        Persistence::initialize(DatabaseConfig::from_urls(&url, "sqlite::memory:").unwrap())
            .await
            .unwrap();
    let second = reopened.script_invocation_store();
    assert_eq!(key, second.cursor_key().await.unwrap());
    let changes = second
        .page(owner, Some(position), position, false, 100)
        .await
        .unwrap();
    assert_eq!(changes.rows.len(), 1);
    assert!(
        matches!(&changes.rows[0],Observation::Invocation(v) if v.invocation.status==Status::Running&&v.observation_revision>position)
    );
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT record FROM script_invocations WHERE id='before-eleven'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        record
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT envelope FROM script_program_revisions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        revision.envelope
    );
    assert!(
        pool.execute(
            "UPDATE script_invocations SET source_session='invented' WHERE id='before-eleven'"
        )
        .await
        .is_err()
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires fresh private loopback ROUTINE_OBSERVATION_POSTGRES_URL/routine_observation_ephemeral database"]
async fn routine_postgres_guarded_fresh_observation_contract() {
    use sqlx::Executor as _;
    let raw =
        std::env::var("ROUTINE_OBSERVATION_POSTGRES_URL").expect("explicit fresh fixture URL");
    let fixture_url =
        guarded_postgres_fixture_url(&raw).expect("strict fresh loopback fixture URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&fixture_url)
        .await
        .unwrap();
    pool.execute("CREATE SCHEMA waltr")
        .await
        .expect("must fail if schema exists; never reset a target");
    assert!(pool.execute("CREATE SCHEMA waltr").await.is_err());
    pool.close().await;
    let persistence = Persistence::initialize(
        DatabaseConfig::from_urls(&fixture_url, "sqlite::memory:").unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.script_invocation_store();
    store
        .admit_with_source(admission("foreign", "other"), Some("source".into()))
        .await
        .unwrap();
    store
        .admit_with_source(admission("owner", "owner"), Some("source".into()))
        .await
        .unwrap();
    let scoped = scope("owner", Some("source"));
    assert_eq!(
        ObservationStore::list(&store, scoped.clone(), 0, 1)
            .await
            .unwrap()[0]
            .invocation
            .id,
        "owner"
    );
    let snapshot = store.page(scoped.clone(), None, 0, true, 1).await.unwrap();
    store.running("owner".into(), 100).await.unwrap();
    store
        .terminal("owner".into(), Status::Interrupted, None, 100)
        .await
        .unwrap();
    let changes = store
        .page(
            scoped,
            Some(snapshot.high_water),
            snapshot.high_water,
            false,
            100,
        )
        .await
        .unwrap();
    assert_eq!(changes.rows.len(), 2);
    assert!(changes.rows.windows(2).all(|pair|matches!((&pair[0],&pair[1]),(Observation::Invocation(a),Observation::Invocation(b)) if a.observation_revision<b.observation_revision)));
}

fn guarded_postgres_fixture_url(raw: &str) -> Option<String> {
    let mut url = url::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "postgres" | "postgresql")
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || url.username() != "routine_observation"
        || url.path() != "/routine_observation_ephemeral"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    url.query_pairs_mut().append_pair("sslmode", "disable");
    Some(url.into())
}

#[test]
fn routine_postgres_fixture_guard_derives_only_loopback_tls_override() {
    let valid = "postgres://routine_observation:routine_observation_fixture@127.0.0.1:49377/routine_observation_ephemeral";
    assert!(
        guarded_postgres_fixture_url(valid)
            .unwrap()
            .ends_with("?sslmode=disable")
    );
    for invalid in [
        valid.replace("127.0.0.1", "192.0.2.1"),
        valid.replace("127.0.0.1", "localhost"),
        valid.replace("/routine_observation_ephemeral", "/existing"),
        valid.replace("routine_observation:", "other:"),
        format!("{valid}?sslmode=require"),
        format!("{valid}?options=unsafe"),
        format!("{valid}#override"),
    ] {
        assert!(guarded_postgres_fixture_url(&invalid).is_none());
    }
}
