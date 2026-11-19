//! Hosted workspace availability must not become a prerequisite for server readiness.

use std::{
    fs,
    net::TcpListener,
    path::Path,
    process::{Child, Command},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Code, Request, Status};
use waltr::grpc::waltr::{
    sandbox::v1::{
        ConnectRequest, Hello, connect_request::Body, sandbox_service_client::SandboxServiceClient,
    },
    session::v1::{
        CreateSessionRequest, WorkspaceSelection, session_service_client::SessionServiceClient,
    },
};
use waltr_server::test_support::setup_access_auth_server;

const MACHINE: &str = "laptop";
const PROJECT: &str = "project";
const MISSING: &str = "nonexistent";
const OFFLINE: &str = "workspace machine offline";
const RETRY_INTERVAL: Duration = Duration::from_millis(25);
const DEADLINE: Duration = Duration::from_secs(10);

#[tokio::test]
async fn optional_worker_disconnect_does_not_affect_host_readiness()
-> Result<(), Box<dyn std::error::Error>> {
    let oidc = setup_access_auth_server();
    let fixture = tempfile::tempdir()?;
    let root = fixture.path();
    let local = root.join("local");
    let export = root.join("export");
    fs::create_dir_all(local.join(PROJECT))?;
    fs::create_dir_all(export.join(PROJECT))?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?.to_string();
    let origin = format!("http://{addr}");
    drop(listener);
    let mut server =
        ServerProcess(hosted_server_command(root, &local, &addr, &origin, &oidc.issuer)?.spawn()?);

    let http = Client::builder().no_proxy().timeout(DEADLINE).build()?;
    let ready_url = format!("{origin}/ready");
    wait_for_ready(&http, &ready_url, &mut server).await?;

    let channel = tonic::transport::Endpoint::from_shared(origin.clone())?
        .connect_timeout(DEADLINE)
        .connect()
        .await?;
    let mut workers = SandboxServiceClient::new(channel.clone());
    let mut sessions = SessionServiceClient::new(channel.clone());
    let request = |machine: &str, directory: &str, token: &str| -> Request<CreateSessionRequest> {
        let mut request = Request::new(CreateSessionRequest {
            workspace: Some(WorkspaceSelection {
                machine_id: machine.into(),
                working_directory: Some(directory.into()),
            }),
            ..Default::default()
        });
        request.set_timeout(DEADLINE);
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        request
    };
    let offline = sessions
        .create_session(request(MACHINE, PROJECT, &oidc.access_token))
        .await
        .unwrap_err();
    assert_offline(&offline);

    let (_sender, mut cookie_only) = worker_request(None, None).await;
    cookie_only
        .metadata_mut()
        .insert("cookie", oidc.session_cookie.parse()?);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), workers.connect(cookie_only))
            .await?
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    let (_sender, reserved) = worker_request(Some("local"), Some(&oidc.access_token)).await;
    assert_eq!(
        workers.connect(reserved).await.unwrap_err().code(),
        Code::InvalidArgument
    );

    let worker_stop = CancellationToken::new();
    let bearer: tonic::metadata::AsciiMetadataValue =
        format!("Bearer {}", oidc.access_token).parse()?;
    let worker = tokio::spawn(waltr_agent_sandbox::run_worker_with_authorization(
        channel,
        export,
        MACHINE,
        worker_stop.clone(),
        Arc::new(AtomicBool::new(false)),
        move || Some(bearer.clone()),
    ));
    let connected = tokio::time::timeout(DEADLINE, async {
        loop {
            match sessions
                .create_session(request(MACHINE, PROJECT, &oidc.access_token))
                .await
            {
                Err(status) if status.message().contains(OFFLINE) => {
                    tokio::time::sleep(RETRY_INTERVAL).await;
                }
                result => break result,
            }
        }
    })
    .await
    .expect("peer should register")?
    .into_inner()
    .session
    .expect("remote create should return a session")
    .workspace
    .expect("session should retain explicit selection");
    assert_eq!(connected.machine_id, MACHINE);
    assert_eq!(connected.working_directory.as_deref(), Some(PROJECT));
    let refreshed = oidc.access_token_with_ttl(Duration::from_mins(10));
    assert_ne!(refreshed, oidc.access_token);
    assert!(
        sessions
            .create_session(request(MACHINE, PROJECT, &refreshed))
            .await?
            .into_inner()
            .session
            .is_some()
    );

    let (_sender, duplicate) = worker_request(Some(MACHINE), Some(&refreshed)).await;
    assert_eq!(
        workers.connect(duplicate).await.unwrap_err().code(),
        Code::AlreadyExists
    );
    assert!(
        sessions
            .create_session(request(MACHINE, PROJECT, &refreshed))
            .await?
            .into_inner()
            .session
            .is_some()
    );
    assert_ready(http.get(&ready_url).send().await?).await?;

    worker_stop.cancel();
    tokio::time::timeout(DEADLINE, worker).await???;
    // Probe a nonexistent directory to observe disconnect without creating extra sessions.
    tokio::time::timeout(DEADLINE, async {
        loop {
            let status = sessions
                .create_session(request(MACHINE, MISSING, &refreshed))
                .await
                .unwrap_err();
            if status.message().contains(OFFLINE) {
                break;
            }
            assert!(
                status
                    .message()
                    .contains("cannot resolve working directory"),
                "{status}"
            );
            tokio::time::sleep(RETRY_INTERVAL).await;
        }
    })
    .await
    .expect("host should observe the worker disconnect");
    let offline = sessions
        .create_session(request(MACHINE, PROJECT, &refreshed))
        .await
        .unwrap_err();
    assert_offline(&offline);
    assert_ready(http.get(&ready_url).send().await?).await?;

    let local_session = sessions
        .create_session(request("local", PROJECT, &refreshed))
        .await?
        .into_inner();
    let local_selection = local_session.session.unwrap().workspace.unwrap();
    assert_eq!(local_selection.machine_id, "local");
    assert_eq!(local_selection.working_directory.as_deref(), Some(PROJECT));

    let short_lived = oidc.access_token_with_ttl(Duration::from_secs(3));
    let (_sender, waiting_for_hello) = worker_request(None, Some(&short_lived)).await;
    let (_sender, expiring) = worker_request(Some(MACHINE), Some(&short_lived)).await;
    let mut expiring = workers.connect(expiring).await?.into_inner();
    let mut other = workers.clone();
    let (waiting, connected) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(other.connect(waiting_for_hello), expiring.message())
    })
    .await?;
    for status in [waiting.unwrap_err(), connected.unwrap_err()] {
        assert_eq!(status.code(), Code::Unauthenticated);
        assert_eq!(status.message(), "workspace access token expired");
    }
    assert_ready(http.get(&ready_url).send().await?).await?;
    Ok(())
}

async fn worker_request(
    machine: Option<&str>,
    token: Option<&str>,
) -> (
    mpsc::Sender<ConnectRequest>,
    Request<ReceiverStream<ConnectRequest>>,
) {
    let (tx, rx) = mpsc::channel(1);
    if let Some(machine) = machine {
        tx.send(ConnectRequest {
            body: Some(Body::Hello(Hello {
                workspace_id: machine.into(),
                worker_id: "readiness-probe".into(),
                canonical_root: "/readiness-probe".into(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
    }
    let mut request = Request::new(ReceiverStream::new(rx));
    request.set_timeout(DEADLINE);
    if let Some(token) = token {
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    (tx, request)
}

async fn wait_for_ready(
    http: &Client,
    url: &str,
    server: &mut ServerProcess,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = tokio::time::timeout(DEADLINE, async {
        loop {
            match http.get(url).send().await {
                Ok(response) if response.status() != StatusCode::SERVICE_UNAVAILABLE => {
                    break Ok::<_, Box<dyn std::error::Error>>(response);
                }
                Err(error) if !error.is_connect() => break Err(error.into()),
                _ => {
                    if let Some(exit) = server.0.try_wait()? {
                        return Err(format!("host exited before becoming ready: {exit}").into());
                    }
                    tokio::time::sleep(RETRY_INTERVAL).await;
                }
            }
        }
    })
    .await
    .expect("host should become ready without a registered peer")?;
    assert_ready(response).await?;
    Ok(())
}

async fn assert_ready(response: reqwest::Response) -> reqwest::Result<()> {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await?, json!({"status": "ready"}));
    Ok(())
}

fn assert_offline(status: &Status) {
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(status.message().contains(OFFLINE), "{status}");
}

fn hosted_server_command(
    root: &Path,
    local: &Path,
    addr: &str,
    origin: &str,
    issuer: &str,
) -> std::io::Result<Command> {
    let wrapping_keys = root.join("mcp-wrapping-keys.json");
    fs::write(
        &wrapping_keys,
        r#"{"schema_version":1,"active":"one","keys":[{"id":"one","key":"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"}]}"#,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&wrapping_keys, fs::Permissions::from_mode(0o400))?;
    }
    let catalog =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models-dev-defaults.json");
    let mut command = Command::new(env!("CARGO_BIN_EXE_waltr-server"));
    command
        .env_clear()
        .current_dir(local)
        .env("HOME", root)
        .env("WALTR_SERVER_ADDR", addr)
        .env(
            "WALTR_DATABASE_URL",
            format!("sqlite://{}", root.join("waltr.sqlite3").display()),
        )
        .env("WALTR_SESSION_STORE", "memory")
        .env("WALTR_ROUTINE_OBJECT_ROOT", root.join("objects"))
        .env("WALTR_MODELS_DEV_PATH", catalog)
        .env(
            "WALTR_MODELS_DEV_CACHE_PATH",
            root.join("catalog-cache.json"),
        )
        .env("WALTR_PUBLIC_URL", "https://waltr.example")
        .env("WALTR_AUTH_MODE", "oidc")
        .env("WALTR_OIDC_ISSUER", issuer)
        .env("WALTR_OIDC_CLIENT_ID", "waltr-test-client")
        .env("WALTR_OIDC_REDIRECT_URI", format!("{origin}/oidc/callback"))
        .env("WALTR_API_OIDC_AUDIENCE", "waltr-api")
        .env("WALTR_API_REQUIRED_SCOPES", "waltr:read")
        .env("WALTR_OIDC_SCOPES", "openid")
        .env("WALTR_AUTH_ALLOW_INSECURE_COOKIE", "true")
        .env("WALTR_OIDC_ALLOW_INSECURE_HTTP", "true")
        .env("WALTR_MCP_REQUIRED_SCOPES", "mcp:use")
        .env("WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE", wrapping_keys);
    Ok(command)
}

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        drop(self.0.kill());
        drop(self.0.wait());
    }
}
