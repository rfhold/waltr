//! Ignored durable session storage resume tests.

use std::{
    fs,
    net::TcpListener,
    sync::{Arc, Once},
};

use futures_util::StreamExt;
use waltr_agent::{
    Agent, AgentEvent, ExternalRequestId, ExternalResponse, FilesystemSessionStore,
    OpenDalSessionStore, ResumeInput, S3SessionStore, ScyllaSessionStore, SessionStore,
    opendal_postgresql_session_store,
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
    assert_resume_round_trip(store, "scylla").await
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
    assert_resume_round_trip(store, "opendal-postgresql").await
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
    let provider = live.provider;
    let agent = Agent::builder(live.model)
        .with_system_prompt(
            "You are validating durable session storage. Follow the user instructions exactly.",
        )
        .with_question_tool(QuestionConfig::new())
        .build();
    let sessions = agent.sessions(store.clone());
    let session = sessions.create().await?;
    let session_id = session.id().clone();

    let first = collect(
        sessions
            .prompt(
                &provider,
                session_id.clone(),
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
                &provider,
                session_id.clone(),
                ResumeInput::ExternalResponse {
                    request_id: ExternalRequestId::new(request),
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
    assert!(
        stored
            .snapshot
            .tool_results()
            .any(|result| result.name == QUESTION_TOOL_NAME
                && result.content.contains("resume confirmed")),
        "{label}: stored session did not persist the resumed question result: {stored:?}"
    );
    Ok(())
}

#[derive(Default)]
struct CollectedSession {
    text: String,
    finished: bool,
    external_request_ids: Vec<String>,
}

async fn collect(
    mut events: waltr_agent::BoxAgentEventStream<'_>,
) -> waltr_agent::Result<CollectedSession> {
    let mut collected = CollectedSession::default();
    while let Some(event) = events.next().await {
        match event? {
            AgentEvent::TextDelta { delta, .. } => collected.text.push_str(&delta),
            AgentEvent::ExternalRequest { request, .. } => {
                collected.external_request_ids.push(request.id);
            }
            AgentEvent::Finished { .. } => collected.finished = true,
            AgentEvent::ThinkingDelta { .. }
            | AgentEvent::ToolCall { .. }
            | AgentEvent::ToolResult { .. }
            | AgentEvent::TurnFinished { .. } => {}
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
