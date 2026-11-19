use super::*;
use crate::{
    access_auth::OidcConfig,
    agent_host::sandbox_gateway::WorkspaceAccount,
    persistence::DatabaseConfig,
    test_support::{
        default_catalog_loader, isolated_process_test, jwks, sign_access_token_for_subject_with_ttl,
    },
};
use axum::{Form, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use serde_json::json;
use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    task::JoinHandle,
    time::timeout,
};
use tonic::{Code, Request as GrpcRequest, Status, transport::Channel};
use waltr::grpc::waltr::{
    auth::v1::{
        AuthAttemptLifecycle, AuthCompletionMode, AuthMethod, AuthStatusKind, AuthTarget,
        BeginAuthRequest, GetAuthAttemptStatusRequest, GetAuthStatusRequest, LogoutRequest,
        RefreshAuthRequest, auth_service_client::AuthServiceClient, auth_target::Target,
    },
    resource::v1::{
        InspectResourceRequest, ReplaceResourceRequest, ResetResourceRequest,
        resource_service_client::ResourceServiceClient,
    },
    session::v1::{
        CreateSessionRequest, WorkspaceSelection, session_service_client::SessionServiceClient,
    },
};

const MACHINE: &str = "exporting-machine";
const BINDING: &str = "test-binding";
const RESOURCE: &str = "workspace-bindings/test-binding.yaml";
const PROJECT: &str = "project";
const SENTINEL: &str = "sentinel.txt";
const CONTENTS: &str = "only the exporting worker has this file";
const SUBJECT: &str = "test-user";
const SCOPE: &str = "waltr:read";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEADLINE: Duration = Duration::from_secs(15);
const TEST: &str = "app::tests::workspace_binding::workspace_binding_authorizes_exports_refreshes_and_stops_without_fallback";
const EXPORTER_ROOT: &str = "WALTR_WORKSPACE_BINDING_TEST_EXPORTER_ROOT";
const DESTINATION_URL: &str = "WALTR_WORKSPACE_BINDING_TEST_DESTINATION";
const ISSUER_URL: &str = "WALTR_WORKSPACE_BINDING_TEST_ISSUER";
const EXPORTER_ADDRESS: &str = "exporter-address";

#[tokio::test]
async fn workspace_binding_authorizes_exports_refreshes_and_stops_without_fallback() {
    if let Some(root) = std::env::var_os(EXPORTER_ROOT) {
        serve_exporter(Path::new(&root)).await;
        return;
    }
    // The Routine blob store permits only one application per process.
    if !isolated_process_test(TEST) {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let local_root = root.join("destination");
    let worker_root = root.join("exporter");
    let export_root = worker_root.join("shared");
    std::fs::create_dir_all(local_root.join(PROJECT)).unwrap();
    std::fs::create_dir_all(export_root.join(PROJECT)).unwrap();
    std::fs::write(export_root.join(PROJECT).join(SENTINEL), CONTENTS).unwrap();
    assert!(!local_root.join(PROJECT).join(SENTINEL).exists());

    let listener = loopback().await;
    let endpoint = origin(&listener);
    let (idp, idp_stop, idp_task) = DeviceIssuer::start(endpoint.clone()).await;
    let destination_db = persistence().await;
    let oidc = OidcConfig::new(
        &idp.url,
        "fixture-browser",
        None,
        format!("{endpoint}/oidc/callback"),
        "waltr-api",
        vec!["openid".into()],
        Duration::from_hours(1),
        true,
        true,
    )
    .unwrap()
    .with_access_domains(&endpoint, &idp.url, "waltr-api", vec![SCOPE.into()])
    .unwrap()
    .with_api_client_id("fixture-native")
    .unwrap();
    let access = AccessAuthenticator::initialize(
        AccessAuthConfig::oidc(oidc),
        &destination_db,
        AccessAuthHttpClient::build(reqwest::Client::builder().no_proxy().timeout(DEADLINE))
            .unwrap(),
    )
    .await
    .unwrap();
    let bearer = idp.access_token(SUBJECT, Duration::from_hours(1));
    let account = WorkspaceAccount::from_principal(
        &access
            .validate_api_access_token(&bearer, &[SCOPE.into()])
            .await
            .unwrap(),
    )
    .unwrap();
    let destination = application(
        &root.join("destination-config"),
        &local_root,
        destination_db,
        access,
        Vec::new(),
    )
    .await;
    let machines = destination.machines.clone();
    let destination = RunningApp::start(destination, listener).await;
    let mut exporter = start_exporter(&root, &endpoint, &idp.url);
    let exporter_channel = connect(&exporter_address(&root, &mut exporter).await).await;

    let mut config = ResourceServiceClient::new(exporter_channel.clone());
    let mut auth = AuthServiceClient::new(exporter_channel);
    let mut sessions = SessionServiceClient::new(destination.channel.clone());
    let absent = config
        .inspect_resource(InspectResourceRequest {
            path: Some(RESOURCE.into()),
        })
        .await
        .unwrap()
        .into_inner()
        .resources
        .remove(0);
    assert!(absent.active.is_none());
    let binding = config
        .replace_resource(ReplaceResourceRequest {
            path: RESOURCE.into(),
            source: format!(
                "apiVersion: waltr.dev/v1alpha1\nkind: WorkspaceBinding\nmetadata: {{name: {BINDING}}}\nspec:\n  endpoint: {endpoint}\n  machineId: {MACHINE}\n  exportRoot: '{}'\n",
                export_root.display()
            ),
            expected_etag: absent.persisted_etag,
        })
        .await
        .unwrap()
        .into_inner()
        .resource
        .unwrap();
    let status = auth
        .get_auth_status(GetAuthStatusRequest { target: target() })
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    assert_eq!(status.kind, AuthStatusKind::NotConfigured as i32);
    assert_eq!(status.supported_methods, [AuthMethod::OauthDevice as i32]);
    assert_eq!(idp.device_starts.load(Ordering::Acquire), 0);
    assert_offline(
        sessions
            .create_session(selection(MACHINE, &bearer))
            .await
            .unwrap_err(),
    );

    let initial_expiry = authorize(&mut auth, &idp).await;
    wait_for("exported machine claim", || {
        machines.owns_live(MACHINE, &account)
    })
    .await;
    assert_selection(&mut sessions, MACHINE, &bearer).await;
    let other = idp.access_token("different-user", Duration::from_hours(1));
    assert_offline(
        sessions
            .create_session(selection(MACHINE, &other))
            .await
            .unwrap_err(),
    );
    assert_sentinel(&machines, &account).await;

    let refreshed = auth
        .refresh_auth(RefreshAuthRequest { target: target() })
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    assert_eq!(idp.refreshes.load(Ordering::Acquire), 1);
    assert_eq!(refreshed.kind, AuthStatusKind::OauthConfigured as i32);
    assert!(refreshed.expires_at_ms > initial_expiry);
    assert_selection(&mut sessions, MACHINE, &bearer).await;
    assert_sentinel(&machines, &account).await;

    auth.logout(LogoutRequest { target: target() })
        .await
        .unwrap();
    wait_for("logout to retire the machine claim", || {
        !machines.owns_live(MACHINE, &account)
    })
    .await;
    assert_offline(
        sessions
            .create_session(selection(MACHINE, &bearer))
            .await
            .unwrap_err(),
    );
    assert_selection(&mut sessions, "local", &bearer).await;

    let _new_expiry = authorize(&mut auth, &idp).await;
    wait_for(
        "explicit reauthorization to restore the machine claim",
        || machines.owns_live(MACHINE, &account),
    )
    .await;
    let deleted = config
        .reset_resource(ResetResourceRequest {
            path: RESOURCE.into(),
            expected_etag: binding.persisted_etag,
        })
        .await
        .unwrap()
        .into_inner();
    let removed = deleted.resource.unwrap();
    assert!(!removed.persisted);
    assert!(removed.active.is_none());
    wait_for("deletion to retire the machine claim", || {
        !machines.owns_live(MACHINE, &account)
    })
    .await;
    assert_offline(
        sessions
            .create_session(selection(MACHINE, &bearer))
            .await
            .unwrap_err(),
    );
    assert_selection(&mut sessions, "local", &bearer).await;
    assert_eq!(idp.device_starts.load(Ordering::Acquire), 2);

    drop(exporter.stdin.take());
    assert!(
        timeout(DEADLINE, exporter.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    destination.shutdown().await;
    idp_stop.cancel();
    timeout(DEADLINE, idp_task).await.unwrap().unwrap();
}

fn start_exporter(root: &Path, destination: &str, issuer: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env(EXPORTER_ROOT, root)
        .env(DESTINATION_URL, destination)
        .env(ISSUER_URL, issuer)
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn exporter_address(root: &Path, child: &mut Child) -> String {
    timeout(DEADLINE, async {
        let path = root.join(EXPORTER_ADDRESS);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("exporter exited before becoming ready: {status}");
            }
            match std::fs::read_to_string(&path) {
                Ok(address) => break address,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => panic!("read exporter address: {error}"),
            }
        }
    })
    .await
    .expect("exporter should become ready")
}

async fn serve_exporter(root: &Path) {
    let endpoint = std::env::var(DESTINATION_URL).unwrap();
    let issuer = std::env::var(ISSUER_URL).unwrap();
    let listener = loopback().await;
    let address = origin(&listener);
    let app = RunningApp::start(
        application(
            &root.join("exporter-config"),
            &root.join("exporter"),
            persistence().await,
            AccessAuthenticator::disabled(),
            vec![endpoint.parse().unwrap(), issuer.parse().unwrap()],
        )
        .await,
        listener,
    )
    .await;
    let staged = root.join("exporter-address.tmp");
    std::fs::write(&staged, address).unwrap();
    std::fs::rename(staged, root.join(EXPORTER_ADDRESS)).unwrap();
    // Close the exporter when the destination fixture closes its end of the pipe.
    tokio::task::spawn_blocking(|| {
        use std::io::Read as _;
        assert_eq!(io::stdin().read(&mut [0]).unwrap(), 0);
    })
    .await
    .unwrap();
    app.shutdown().await;
}

async fn authorize(auth: &mut AuthServiceClient<Channel>, idp: &DeviceIssuer) -> i64 {
    let begin = auth
        .begin_auth(BeginAuthRequest {
            target: target(),
            method: AuthMethod::OauthDevice as i32,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        begin.completion_mode,
        AuthCompletionMode::ServerDevicePoll as i32
    );
    assert_eq!(begin.verification_url, format!("{}/verify", idp.url));
    assert_eq!(begin.user_code, "TEST-CODE");
    idp.approved.store(true, Ordering::Release);
    let status = timeout(DEADLINE, async {
        loop {
            let response = auth
                .get_auth_attempt_status(GetAuthAttemptStatusRequest {
                    attempt_id: begin.attempt_id.clone(),
                })
                .await
                .unwrap()
                .into_inner();
            let attempt = response.attempt.unwrap();
            match AuthAttemptLifecycle::try_from(attempt.lifecycle).unwrap() {
                AuthAttemptLifecycle::Pending | AuthAttemptLifecycle::Completing => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                AuthAttemptLifecycle::Succeeded => break response.status.unwrap(),
                state => panic!(
                    "device authorization ended in {state:?}: {}",
                    attempt.failure_reason
                ),
            }
        }
    })
    .await
    .expect("device authorization should complete");
    assert_eq!(status.kind, AuthStatusKind::OauthConfigured as i32);
    status.expires_at_ms
}

fn target() -> Option<AuthTarget> {
    Some(AuthTarget {
        target: Some(Target::WorkspaceBindingKey(BINDING.into())),
    })
}

fn selection(machine: &str, token: &str) -> GrpcRequest<CreateSessionRequest> {
    let mut request = GrpcRequest::new(CreateSessionRequest {
        workspace: Some(WorkspaceSelection {
            machine_id: machine.into(),
            working_directory: Some(PROJECT.into()),
        }),
        ..Default::default()
    });
    request.set_timeout(DEADLINE);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

async fn assert_selection(
    sessions: &mut SessionServiceClient<Channel>,
    machine: &str,
    token: &str,
) {
    let workspace = sessions
        .create_session(selection(machine, token))
        .await
        .unwrap()
        .into_inner()
        .session
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(workspace.machine_id, machine);
    assert_eq!(workspace.working_directory.as_deref(), Some(PROJECT));
}

fn assert_offline(status: Status) {
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("workspace machine offline"),
        "{status}"
    );
}

async fn assert_sentinel(machines: &MachineRegistry, account: &WorkspaceAccount) {
    let handle = machines
        .resolve(MACHINE, Some(PROJECT), account)
        .await
        .unwrap();
    assert_eq!(handle.read_file(SENTINEL).await.unwrap().content, CONTENTS);
}

async fn wait_for(description: &str, mut condition: impl FnMut() -> bool) {
    timeout(DEADLINE, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
}

async fn loopback() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

fn origin(listener: &TcpListener) -> String {
    format!("http://{}", listener.local_addr().unwrap())
}

async fn persistence() -> Persistence {
    Persistence::initialize(DatabaseConfig::for_tests().unwrap())
        .await
        .unwrap()
}

async fn application(
    config_root: &Path,
    workspace_root: &Path,
    persistence: Persistence,
    access_authenticator: AccessAuthenticator,
    outbound_private_origins: Vec<url::Url>,
) -> ServerApplication {
    std::fs::create_dir_all(config_root.join(".waltr/resources/mcp-servers")).unwrap();
    std::fs::write(
        config_root.join(".waltr/resources/mcp-servers/waltr.yaml"),
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {name: waltr}\nspec: {transport: streamableHttp, url: 'http://127.0.0.1:1/mcp'}\n",
    )
    .unwrap();
    let catalog_loader = default_catalog_loader();
    let config_service = ConfigService::new(config_root, catalog_loader.clone())
        .await
        .unwrap();
    assemble_app(
        AppServices {
            config_service,
            persistence,
            catalog_loader,
            access_authenticator,
            mcp_oauth: None,
            outbound_private_origins,
            outbound_oauth: OutboundOAuthConfig::default().with_workspace_binding_loopback(),
            automatic_self_mcp_resource: None,
            agent_state: None,
            local_admission: None,
            workspace_root: workspace_root.into(),
            sandbox_socket: None,
            routine_authority: RoutineAuthority::default(),
            routine_private_parent: config_root.into(),
        },
        |routes, input| {
            Ok(session::add_store_service(
                routes,
                waltr_agent::MemorySessionStore::new(),
                input,
            ))
        },
    )
    .await
    .unwrap()
}

struct RunningApp {
    channel: Channel,
    stop: CancellationToken,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl RunningApp {
    async fn start(app: ServerApplication, listener: TcpListener) -> Self {
        timeout(DEADLINE, app.wait_until_ready()).await.unwrap();
        let endpoint = origin(&listener);
        let stop = CancellationToken::new();
        let task = tokio::spawn(app.serve_with_shutdown(listener, stop.clone().cancelled_owned()));
        let channel = connect(&endpoint).await;
        Self {
            channel,
            stop,
            task: Some(task),
        }
    }

    async fn shutdown(mut self) {
        self.stop.cancel();
        timeout(DEADLINE, self.task.take().unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

async fn connect(endpoint: &str) -> Channel {
    tonic::transport::Endpoint::from_shared(endpoint.to_owned())
        .unwrap()
        .connect_timeout(DEADLINE)
        .timeout(DEADLINE)
        .connect()
        .await
        .unwrap()
}

impl Drop for RunningApp {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct DeviceIssuer {
    url: String,
    destination: String,
    device_starts: AtomicUsize,
    approved: AtomicBool,
    refreshes: AtomicUsize,
}

impl DeviceIssuer {
    async fn start(destination: String) -> (Arc<Self>, CancellationToken, JoinHandle<()>) {
        let listener = loopback().await;
        let issuer = Arc::new(Self {
            url: origin(&listener),
            destination,
            device_starts: AtomicUsize::new(0),
            approved: AtomicBool::new(false),
            refreshes: AtomicUsize::new(0),
        });
        let router = Router::new()
            .route("/.well-known/openid-configuration", get(Self::metadata))
            .route("/device", post(Self::device))
            .route("/token", post(Self::token))
            .route("/jwks", get(jwks))
            .with_state(issuer.clone());
        let stop = CancellationToken::new();
        let shutdown = stop.clone().cancelled_owned();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown)
                .await
                .unwrap();
        });
        (issuer, stop, task)
    }

    fn access_token(&self, subject: &str, ttl: Duration) -> String {
        sign_access_token_for_subject_with_ttl(&self.url, subject, SCOPE, ttl)
    }

    async fn metadata(State(issuer): State<Arc<Self>>) -> impl IntoResponse {
        Json(json!({
            "issuer": issuer.url,
            "authorization_endpoint": format!("{}/authorize", issuer.url),
            "token_endpoint": format!("{}/token", issuer.url),
            "device_authorization_endpoint": format!("{}/device", issuer.url),
            "jwks_uri": format!("{}/jwks", issuer.url),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", DEVICE_GRANT, "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "access_token_signing_alg_values_supported": ["RS256"],
        }))
    }

    async fn device(
        State(issuer): State<Arc<Self>>,
        Form(form): Form<HashMap<String, String>>,
    ) -> impl IntoResponse {
        assert_eq!(form["client_id"], "fixture-native");
        assert_eq!(form["scope"], "openid offline_access waltr:read");
        assert_eq!(form["resource"].trim_end_matches('/'), issuer.destination);
        assert_eq!(form["audience"], "waltr-api");
        issuer.approved.store(false, Ordering::Release);
        issuer.device_starts.fetch_add(1, Ordering::AcqRel);
        Json(json!({
            "device_code": "private-fixture-device-code",
            "user_code": "TEST-CODE",
            "verification_uri": format!("{}/verify", issuer.url),
            "expires_in": 65,
            "interval": 1,
        }))
    }

    async fn token(
        State(issuer): State<Arc<Self>>,
        Form(form): Form<HashMap<String, String>>,
    ) -> axum::response::Response {
        assert_eq!(form["client_id"], "fixture-native");
        if form["grant_type"] == "refresh_token" {
            assert_eq!(form["refresh_token"], "fixture-refresh-token");
            let access_token = issuer.access_token(SUBJECT, Duration::from_mins(10));
            issuer.refreshes.fetch_add(1, Ordering::AcqRel);
            return Json(json!({
                "access_token": access_token, "token_type": "Bearer", "expires_in": 600,
            }))
            .into_response();
        }
        assert_eq!(form["grant_type"], DEVICE_GRANT);
        assert_eq!(form["device_code"], "private-fixture-device-code");
        if !issuer.approved.load(Ordering::Acquire) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "authorization_pending" })),
            )
                .into_response();
        }
        Json(json!({
            "access_token": issuer.access_token(SUBJECT, Duration::from_mins(2)),
            "refresh_token": "fixture-refresh-token",
            "token_type": "Bearer",
            "expires_in": 120,
        }))
        .into_response()
    }
}
