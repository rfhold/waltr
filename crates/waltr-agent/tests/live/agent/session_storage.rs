//! Ignored durable session storage resume tests.

use std::{
    collections::BTreeMap,
    fs,
    net::TcpListener,
    sync::{Arc, Once},
};

use futures_util::StreamExt;
use waltr_agent::{
    AgentRuntime, CHANNEL_ASSISTANT_TEXT, CHANNEL_EXTERNAL_REQUEST, CHANNEL_TOOL_CALL,
    CHANNEL_TOOL_RESULT, ExternalRequestId, ExternalResponse, FilesystemSessionStore,
    MetadataSetCondition, OpenDalSessionStore, PendingTimelineEvent, PendingTimelineItemId,
    ReplacementUserContent, ResumeInput, Revision, S3SessionStore, ScyllaSessionStore,
    SessionCoordinator, SessionCreateRequest, SessionId, SessionJournalEvent, SessionMaskAddition,
    SessionMaskError, SessionMaskMutation, SessionStore, SessionStoreError, ThroughCursor,
    TimelineChannel, TimelineContent, TimelineCursor, TimelineEventKind, TimelineQuery,
    TimelineStatus, opendal_postgresql_session_store,
    tools::question::{QUESTION_TOOL_NAME, QuestionConfig},
};
use opendal::{Operator, services};
use scylla::{client::session::Session, client::session_builder::SessionBuilder};
use serde_json::json;
use tempfile::TempDir;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, Mount, WaitFor},
    runners::AsyncRunner,
};
use tokio::time::{Duration, sleep};

use crate::live_provider;

#[tokio::test]
#[ignore = "requires container/live storage integration opt-in"]
async fn filesystem_session_store_resumes_from_temp_dir() -> waltr_agent::Result<()> {
    install_rustls_crypto_provider();
    let temp = TempDir::new()?;
    let store = FilesystemSessionStore::new(temp.path().join("sessions"));
    assert_resume_round_trip(store, "filesystem").await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s3_session_store_resumes_from_garage_container() -> waltr_agent::Result<()> {
    install_rustls_crypto_provider();
    let garage = GarageContainer::start().await?;
    let store = S3SessionStore::new(garage.s3_client().await, garage.bucket.clone(), "sessions");
    assert_mask_store_contract(store.clone(), "s3-garage").await?;
    assert_resume_round_trip(store, "s3-garage").await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn scylla_session_store_resumes_from_container() -> waltr_agent::Result<()> {
    install_rustls_crypto_provider();
    let scylla = Box::pin(ScyllaContainer::start()).await?;
    let store =
        ScyllaSessionStore::new(scylla.session.clone(), scylla.keyspace.clone(), "sessions");
    store.create_schema().await.map_err(store_error)?;
    assert_mask_store_contract(store.clone(), "scylla").await?;
    assert_resume_round_trip(store, "scylla").await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn scylla_conditional_metadata_is_atomic_and_preserves_record_state() -> waltr_agent::Result<()>
{
    install_rustls_crypto_provider();
    let scylla = Box::pin(ScyllaContainer::start()).await?;
    let first =
        ScyllaSessionStore::new(scylla.session.clone(), scylla.keyspace.clone(), "metadata");
    let second =
        ScyllaSessionStore::new(scylla.session.clone(), scylla.keyspace.clone(), "metadata");
    first.create_schema().await.map_err(store_error)?;
    let id = SessionId::new("conditional-metadata");
    first
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .map_err(store_error)?;
    first
        .append_timeline(
            &id,
            Revision::ZERO,
            vec![PendingTimelineEvent::assistant_text_delta(
                id.clone(),
                0,
                PendingTimelineItemId::New,
                "preserved",
            )],
        )
        .await
        .map_err(store_error)?;
    first
        .set_metadata_value(&id, "unrelated".to_owned(), json!("preserved"))
        .await
        .map_err(store_error)?;
    first
        .set_metadata_value(&id, "null".to_owned(), serde_json::Value::Null)
        .await
        .map_err(store_error)?;

    let null_outcome = second
        .set_metadata_value_conditionally(
            &id,
            "null".to_owned(),
            json!("replacement"),
            MetadataSetCondition::Absent,
        )
        .await
        .map_err(store_error)?;
    assert!(!null_outcome.changed);
    assert_eq!(
        null_outcome.record.metadata_value("null"),
        Some(&serde_json::Value::Null)
    );

    let (left, right) = tokio::join!(
        first.set_metadata_value_conditionally(
            &id,
            "winner".to_owned(),
            json!("left"),
            MetadataSetCondition::Absent,
        ),
        second.set_metadata_value_conditionally(
            &id,
            "winner".to_owned(),
            json!("right"),
            MetadataSetCondition::Absent,
        ),
    );
    let changed = usize::from(left.map_err(store_error)?.changed)
        + usize::from(right.map_err(store_error)?.changed);
    assert_eq!(changed, 1);

    let record = first.load(&id).await.map_err(store_error)?;
    assert_eq!(
        record.metadata_value("unrelated"),
        Some(&json!("preserved"))
    );
    assert_eq!(
        record.metadata_value("null"),
        Some(&serde_json::Value::Null)
    );
    assert!(
        matches!(record.metadata_value("winner"), Some(value) if value == "left" || value == "right")
    );
    assert_eq!(record.timeline_cursor_high_water().get(), 1);
    assert_eq!(record.timeline_events().count(), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires container/live storage integration opt-in"]
async fn opendal_filesystem_session_store_resumes_from_temp_dir() -> waltr_agent::Result<()> {
    install_rustls_crypto_provider();
    let temp = TempDir::new()?;
    let builder = services::Fs::default().root(temp.path().to_str().expect("utf-8 temp path"));
    let operator = Operator::new(builder).map_err(store_error)?.finish();
    let store = OpenDalSessionStore::new(operator, "sessions");
    assert_resume_round_trip(store, "opendal-fs").await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn opendal_s3_session_store_resumes_from_garage_container() -> waltr_agent::Result<()> {
    install_rustls_crypto_provider();
    let garage = GarageContainer::start().await?;
    let builder = services::S3::default()
        .root("/")
        .bucket(&garage.bucket)
        .endpoint(&garage.endpoint)
        .region("garage")
        .access_key_id(&garage.access_key)
        .secret_access_key(&garage.secret_key);
    let operator = Operator::new(builder).map_err(store_error)?.finish();
    let store = OpenDalSessionStore::new(operator, "sessions");
    assert_resume_round_trip(store, "opendal-s3-garage").await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn opendal_postgresql_session_store_resumes_from_container() -> waltr_agent::Result<()> {
    install_rustls_crypto_provider();
    let postgres = PostgresContainer::start().await?;
    let store = opendal_postgresql_session_store(&postgres.connection_string, "sessions")
        .await
        .map_err(store_error)?;
    assert_mask_store_contract(store.clone(), "opendal-postgresql").await?;
    assert_resume_round_trip(store, "opendal-postgresql").await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn opendal_postgresql_compare_and_swap_preserves_concurrent_writes() -> waltr_agent::Result<()>
{
    let postgres = PostgresContainer::start().await?;
    let first = opendal_postgresql_session_store(&postgres.connection_string, "cas")
        .await
        .map_err(store_error)?;
    let second = opendal_postgresql_session_store(&postgres.connection_string, "cas")
        .await
        .map_err(store_error)?;
    let id = SessionId::new("concurrent-journal-metadata");
    first
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .map_err(store_error)?;

    let (journal, metadata) = tokio::join!(
        first.append_journal(
            &id,
            Revision::ZERO,
            vec![SessionJournalEvent::PromptAppended {
                prompt: "preserved".to_owned(),
            }],
        ),
        second.set_metadata_value_conditionally(
            &id,
            "owner".to_owned(),
            json!("metadata"),
            MetadataSetCondition::Absent,
        ),
    );
    journal.map_err(store_error)?;
    assert!(metadata.map_err(store_error)?.changed);
    let record = first.load(&id).await.map_err(store_error)?;
    assert_eq!(record.revision, Revision::new(1));
    assert_eq!(record.snapshot().prompts, ["preserved"]);
    assert_eq!(record.metadata_value("owner"), Some(&json!("metadata")));

    let absent_id = SessionId::new("concurrent-absent");
    first
        .create(SessionCreateRequest::new(absent_id.clone()))
        .await
        .map_err(store_error)?;
    let (left, right) = tokio::join!(
        first.set_metadata_value_conditionally(
            &absent_id,
            "winner".to_owned(),
            json!("left"),
            MetadataSetCondition::Absent,
        ),
        second.set_metadata_value_conditionally(
            &absent_id,
            "winner".to_owned(),
            json!("right"),
            MetadataSetCondition::Absent,
        ),
    );
    let changed = usize::from(left.map_err(store_error)?.changed)
        + usize::from(right.map_err(store_error)?.changed);
    assert_eq!(changed, 1);
    Ok(())
}

async fn assert_resume_round_trip<S>(store: S, label: &str) -> waltr_agent::Result<()>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    let Some(live) = live_provider::skip_if_missing_config(live_provider::selected_from_env(
        "WALTR_AGENT_LIVE_PROVIDER",
        "openai_responses_websocket",
    ))?
    else {
        return Ok(());
    };
    let provider = std::sync::Arc::new(live.provider);
    let agent = AgentRuntime::builder(live.model, std::sync::Arc::clone(&provider))
        .with_system_prompt(
            "You are validating durable session storage. Follow the user instructions exactly.",
        )
        .with_question_tool(QuestionConfig::new())
        .build();
    let sessions = SessionCoordinator::new(store.clone());
    let session = sessions
        .create(waltr_agent::SessionCreateRequest::new(SessionId::generate()))
        .await?;
    let session_id = session.id().clone();

    let first = collect(
        sessions
            .prompt(&agent, session_id.clone(),
                "Call the question tool exactly once to ask: 'Confirm durable storage resume?'. Do not answer directly before the tool result. After the user response is provided on resume, reply with exactly: storage resume complete",
            )
            .stream(),
    )
    .await?;
    let request = first
        .external_request_ids
        .first()
        .unwrap_or_else(|| panic!("{label}: expected external question request"))
        .clone();

    let resumed = collect(
        sessions
            .resume(
                &agent,
                session_id.clone(),
                ResumeInput::ExternalResponse {
                    request_id: request,
                    response: ExternalResponse::new(json!("resume confirmed")),
                },
            )
            .stream(),
    )
    .await?;
    assert!(resumed.finished, "{label}: resumed session did not finish");
    assert!(
        resumed.text.contains("storage resume complete"),
        "{label}: unexpected resumed text: {}",
        resumed.text
    );

    let stored = store.load(&session_id).await.map_err(store_error)?;
    let snapshot = stored.snapshot();
    assert!(
        snapshot
            .tool_results()
            .any(|result| result.name == QUESTION_TOOL_NAME
                && result.content.contains("resume confirmed")),
        "{label}: stored session did not persist the resumed question result: {stored:?}"
    );

    assert_timeline_surface(&store, &session_id, label).await?;
    store.delete(&session_id).await.map_err(store_error)?;
    store.delete(&session_id).await.map_err(store_error)?;
    let error = store.load(&session_id).await.unwrap_err();
    assert!(
        matches!(error, SessionStoreError::NotFound { .. }),
        "{label}: deleted session should not load: {error:?}"
    );
    Ok(())
}

async fn assert_mask_store_contract<S>(store: S, label: &str) -> waltr_agent::Result<()>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    let id = SessionId::new(format!("mask-contract-{label}"));
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .map_err(store_error)?;
    store
        .append_timeline(
            &id,
            Revision::ZERO,
            vec![PendingTimelineEvent::assistant_text_snapshot(
                id.clone(),
                0,
                PendingTimelineItemId::New,
                "answer",
            )],
        )
        .await
        .map_err(store_error)?;
    store
        .append_journal(
            &id,
            Revision::new(1),
            vec![SessionJournalEvent::TurnFinished { turn: 0 }],
        )
        .await
        .map_err(store_error)?;
    let mut add = SessionMaskMutation::new(Some(TimelineCursor::new(1)));
    add.add.push(SessionMaskAddition::new(
        "summary",
        ThroughCursor::new(TimelineCursor::new(1)),
        ReplacementUserContent::new("compacted"),
    ));
    let outcome = store.mutate_masks(&id, add).await.map_err(store_error)?;
    let persisted = store.load(&id).await.map_err(store_error)?;
    assert_eq!(outcome.revision, persisted.revision, "{label}");
    assert_eq!(
        outcome.cursor,
        persisted.timeline_cursor_high_water(),
        "{label}"
    );
    assert_eq!(persisted.active_masks(), outcome.added_masks, "{label}");

    let unchanged = persisted.clone();
    let error = store
        .mutate_masks(&id, SessionMaskMutation::new(None))
        .await
        .expect_err("stale mask boundary must fail");
    assert!(matches!(
        error,
        SessionStoreError::MaskMutation {
            error: SessionMaskError::BoundaryMismatch { .. }
        }
    ));
    assert_eq!(
        store.load(&id).await.map_err(store_error)?,
        unchanged,
        "{label}"
    );

    let mut subtract = SessionMaskMutation::new(Some(TimelineCursor::new(1)));
    subtract.subtract.keys.push("summary".to_owned());
    let subtracted = store
        .mutate_masks(&id, subtract)
        .await
        .map_err(store_error)?;
    let persisted = store.load(&id).await.map_err(store_error)?;
    assert_eq!(subtracted.revision, persisted.revision, "{label}");
    assert!(persisted.active_masks().is_empty(), "{label}");
    let assigned = store
        .append_timeline(
            &id,
            persisted.revision,
            vec![PendingTimelineEvent::assistant_text_snapshot(
                id.clone(),
                1,
                PendingTimelineItemId::New,
                "after mask",
            )],
        )
        .await
        .map_err(store_error)?;
    assert_eq!(assigned[0].cursor, subtracted.cursor.next(), "{label}");
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn assert_timeline_surface<S>(
    store: &S,
    session_id: &waltr_agent::SessionId,
    label: &str,
) -> waltr_agent::Result<()>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    let all = store
        .timeline(session_id, TimelineQuery::forward(None, 512))
        .await
        .map_err(store_error)?;
    assert!(
        !all.is_empty(),
        "{label}: timeline query returned no events"
    );
    assert!(
        all.windows(2)
            .all(|window| window[0].cursor < window[1].cursor),
        "{label}: timeline cursors are not strictly increasing: {all:?}"
    );

    let last_cursor = all.last().expect("non-empty timeline").cursor;
    let before_last = store
        .timeline(
            session_id,
            TimelineQuery::backward(Some(last_cursor), all.len()),
        )
        .await
        .map_err(store_error)?;
    assert_eq!(
        before_last,
        all[..all.len() - 1],
        "{label}: backward timeline query did not preserve chronological order"
    );

    let assistant_events = store
        .timeline(
            session_id,
            TimelineQuery::forward(None, 512)
                .with_channels([TimelineChannel::from(CHANNEL_ASSISTANT_TEXT)]),
        )
        .await
        .map_err(store_error)?;
    assert!(
        !assistant_events.is_empty(),
        "{label}: assistant channel query returned no events"
    );
    assert!(
        assistant_events
            .iter()
            .all(|event| crate::session::channel_is(event, CHANNEL_ASSISTANT_TEXT)),
        "{label}: assistant channel query returned a non-assistant event"
    );

    let tool_and_external_events = store
        .timeline(
            session_id,
            TimelineQuery::forward(None, 512).with_channels([
                TimelineChannel::from(CHANNEL_TOOL_CALL),
                TimelineChannel::from(CHANNEL_TOOL_RESULT),
                TimelineChannel::from(CHANNEL_EXTERNAL_REQUEST),
            ]),
        )
        .await
        .map_err(store_error)?;
    assert!(
        tool_and_external_events
            .iter()
            .any(|event| crate::session::channel_is(event, CHANNEL_TOOL_CALL)),
        "{label}: tool call channel was not present in storage timeline"
    );
    assert!(
        tool_and_external_events
            .iter()
            .any(|event| crate::session::channel_is(event, CHANNEL_TOOL_RESULT)),
        "{label}: tool result channel was not present in storage timeline"
    );
    assert!(
        tool_and_external_events
            .iter()
            .any(|event| crate::session::channel_is(event, CHANNEL_EXTERNAL_REQUEST)),
        "{label}: external request channel was not present in storage timeline"
    );

    let mut assistant_text_by_item = BTreeMap::new();
    for event in &assistant_events {
        match &event.content {
            TimelineContent::TextDelta { delta } => {
                assistant_text_by_item
                    .entry(event.item_id.clone())
                    .or_insert_with(String::new)
                    .push_str(delta);
            }
            TimelineContent::TextSnapshot { text } => {
                assistant_text_by_item.insert(event.item_id.clone(), text.clone());
            }
            TimelineContent::Empty
            | TimelineContent::ToolCall { .. }
            | TimelineContent::ToolResult { .. }
            | TimelineContent::ExternalRequest { .. }
            | TimelineContent::ContentRef { .. }
            | TimelineContent::Json { .. } => {}
        }
    }
    let (final_assistant_item, final_assistant_text) = assistant_text_by_item
        .iter()
        .find(|(_, text)| text.contains("storage resume complete"))
        .unwrap_or_else(|| {
            panic!("{label}: no assistant timeline item contained final resume text: {assistant_text_by_item:?}")
        });
    let reconstructed = store
        .timeline_item_text(session_id, final_assistant_item)
        .await
        .map_err(store_error)?;

    assert_eq!(
        reconstructed.text, *final_assistant_text,
        "{label}: reconstructed assistant item text did not match timeline deltas"
    );
    assert_eq!(
        reconstructed.bytes,
        final_assistant_text.len() as u64,
        "{label}: reconstructed assistant item byte count was wrong"
    );
    assert!(
        reconstructed.last_cursor.is_some(),
        "{label}: reconstructed assistant item did not report a last cursor"
    );

    Ok(())
}

#[derive(Default)]
struct CollectedSession {
    text: String,
    finished: bool,
    external_request_ids: Vec<ExternalRequestId>,
}

async fn collect(
    mut events: waltr_agent::BoxAgentTimelineStream<'_>,
) -> waltr_agent::Result<CollectedSession> {
    let mut collected = CollectedSession::default();
    while let Some(event) = events.next().await {
        let event = event?;
        match &event.content {
            TimelineContent::TextDelta { delta }
                if crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT) =>
            {
                collected.text.push_str(delta);
            }
            TimelineContent::ExternalRequest { request }
                if crate::session::channel_is(&event, CHANNEL_EXTERNAL_REQUEST) =>
            {
                collected.external_request_ids.push(request.id.clone());
            }
            TimelineContent::Empty
                if event.kind == TimelineEventKind::SessionStatusChanged
                    && event.status == Some(TimelineStatus::Finished) =>
            {
                collected.finished = true;
            }
            TimelineContent::Empty
            | TimelineContent::TextDelta { .. }
            | TimelineContent::TextSnapshot { .. }
            | TimelineContent::ToolCall { .. }
            | TimelineContent::ToolResult { .. }
            | TimelineContent::ExternalRequest { .. }
            | TimelineContent::ContentRef { .. }
            | TimelineContent::Json { .. } => {}
        }
    }
    Ok(collected)
}

struct GarageContainer {
    container: testcontainers::ContainerAsync<GenericImage>,
    _config_dir: TempDir,
    endpoint: String,
    bucket: String,
    access_key: String,
    secret_key: String,
}

impl GarageContainer {
    async fn start() -> waltr_agent::Result<Self> {
        let host_port = free_host_port()?;
        let config_dir = TempDir::new()?;
        let config_path = config_dir.path().join("garage.toml");
        fs::write(
            &config_path,
            r#"metadata_dir = "/tmp/meta"
data_dir = "/tmp/data"
db_engine = "sqlite"
replication_factor = 1
rpc_bind_addr = "[::]:3901"
rpc_public_addr = "127.0.0.1:3901"
rpc_secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[s3_api]
s3_region = "garage"
api_bind_addr = "[::]:3900"
"#,
        )?;
        let access_key = "garage-access-key".to_owned();
        let secret_key = "garage-secret-key-for-waltr-session-storage-tests".to_owned();
        let bucket = format!("waltr-storage-{}", std::process::id());
        let image = GenericImage::new("dxflrs/garage", "v2.3.0")
            .with_wait_for(WaitFor::seconds(5))
            .with_mapped_port(host_port, 3900.tcp())
            .with_mount(Mount::bind_mount(
                config_path.to_string_lossy().into_owned(),
                "/etc/garage.toml",
            ))
            .with_env_var("GARAGE_DEFAULT_ACCESS_KEY", &access_key)
            .with_env_var("GARAGE_DEFAULT_SECRET_KEY", &secret_key)
            .with_env_var("GARAGE_DEFAULT_BUCKET", &bucket)
            .with_cmd(["/garage", "server", "--single-node", "--default-bucket"]);
        let container = image.start().await.map_err(store_error)?;
        let endpoint = format!("http://127.0.0.1:{host_port}");
        let garage = Self {
            container,
            _config_dir: config_dir,
            endpoint,
            bucket,
            access_key,
            secret_key,
        };
        garage.wait_for_s3().await?;
        Ok(garage)
    }

    async fn s3_client(&self) -> aws_sdk_s3::Client {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .endpoint_url(&self.endpoint)
            .region(aws_sdk_s3::config::Region::new("garage"))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                self.access_key.clone(),
                self.secret_key.clone(),
                None,
                None,
                "garage-test",
            ))
            .load()
            .await;
        let config = aws_sdk_s3::config::Builder::from(&config)
            .force_path_style(true)
            .build();
        aws_sdk_s3::Client::from_conf(config)
    }

    async fn wait_for_s3(&self) -> waltr_agent::Result<()> {
        let client = self.s3_client().await;
        let key = "__waltr_session_storage_preflight";
        let mut last_error = String::new();
        for _ in 0..60 {
            match client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(aws_sdk_s3::primitives::ByteStream::from_static(b"ready"))
                .send()
                .await
            {
                Ok(_) => {
                    client
                        .delete_object()
                        .bucket(&self.bucket)
                        .key(key)
                        .send()
                        .await
                        .map_err(store_error)?;
                    return Ok(());
                }
                Err(error) => {
                    last_error = aws_sdk_s3::error::DisplayErrorContext(&error).to_string();
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
        Err(store_error(format!(
            "timed out waiting for Garage S3 bucket {} at {}: {last_error}\nstdout:\n{}\nstderr:\n{}",
            self.bucket,
            self.endpoint,
            self.container_stdout().await,
            self.container_stderr().await
        )))
    }

    async fn container_stdout(&self) -> String {
        self.container
            .stdout_to_vec()
            .await
            .ok()
            .and_then(|output| String::from_utf8(output).ok())
            .unwrap_or_else(|| "<unavailable>".to_owned())
    }

    async fn container_stderr(&self) -> String {
        self.container
            .stderr_to_vec()
            .await
            .ok()
            .and_then(|output| String::from_utf8(output).ok())
            .unwrap_or_else(|| "<unavailable>".to_owned())
    }
}

struct ScyllaContainer {
    _container: testcontainers::ContainerAsync<GenericImage>,
    session: Arc<Session>,
    keyspace: String,
}

impl ScyllaContainer {
    async fn start() -> waltr_agent::Result<Self> {
        let image = GenericImage::new("scylladb/scylla", "6.2")
            .with_wait_for(WaitFor::seconds(15))
            .with_mapped_port(9042, 9042.tcp())
            .with_cmd([
                "--smp",
                "1",
                "--memory",
                "750M",
                "--overprovisioned",
                "1",
                "--developer-mode",
                "1",
                "--listen-address",
                "0.0.0.0",
                "--rpc-address",
                "0.0.0.0",
                "--broadcast-rpc-address",
                "127.0.0.1",
            ]);
        let container = image.start().await.map_err(store_error)?;
        let session = Arc::new(Box::pin(connect_scylla_with_retry(9042)).await?);
        let keyspace = format!("waltr_storage_{}", std::process::id());
        session
            .query_unpaged(
                format!("CREATE KEYSPACE IF NOT EXISTS {keyspace} WITH REPLICATION = {{'class': 'SimpleStrategy', 'replication_factor': 1}}"),
                (),
            )
            .await
            .map_err(store_error)?;
        Ok(Self {
            _container: container,
            session,
            keyspace,
        })
    }
}

struct PostgresContainer {
    _container: testcontainers::ContainerAsync<GenericImage>,
    connection_string: String,
}

impl PostgresContainer {
    async fn start() -> waltr_agent::Result<Self> {
        let image = GenericImage::new("postgres", "16-alpine")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_USER", "waltr")
            .with_env_var("POSTGRES_PASSWORD", "waltr")
            .with_env_var("POSTGRES_DB", "waltr");
        let container = image.start().await.map_err(store_error)?;
        let port = container
            .get_host_port_ipv4(5432.tcp())
            .await
            .map_err(store_error)?;
        let connection_string = format!("postgres://waltr:waltr@127.0.0.1:{port}/waltr");
        Ok(Self {
            _container: container,
            connection_string,
        })
    }
}

async fn connect_scylla_with_retry(port: u16) -> waltr_agent::Result<Session> {
    let node = format!("127.0.0.1:{port}");
    let mut last_error = String::new();
    for _ in 0..60 {
        match Box::pin(SessionBuilder::new().known_node(&node).build()).await {
            Ok(session) => return Ok(session),
            Err(error) => {
                last_error = error.to_string();
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
    Err(store_error(format!(
        "timed out connecting to Scylla at {node}: {last_error}"
    )))
}

fn install_rustls_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        drop(rustls::crypto::ring::default_provider().install_default());
    });
}

fn free_host_port() -> waltr_agent::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(store_error)?;
    let port = listener.local_addr().map_err(store_error)?.port();
    drop(listener);
    Ok(port)
}

fn store_error(error: impl std::fmt::Display) -> waltr_agent::LlmError {
    waltr_agent::LlmError::Protocol {
        provider: "session-storage-test",
        message: error.to_string(),
    }
}
