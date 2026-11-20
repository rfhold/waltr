use std::{path::Path, time::Duration};

use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use waltr::grpc::waltr::sandbox::v1::{
    self as proto, connect_request::Body as WorkerBody, connect_response::Body as HostBody,
    sandbox_service_client::SandboxServiceClient,
};
use waltr_agent::{MemorySessionStore, SessionCreateRequest};

use super::*;
use crate::{
    access_auth::{AccessAuthenticator, AuthenticatedPrincipal},
    agent_definition::{AgentDefinitionInspection, AgentDefinitionState},
    agent_host::sandbox_gateway::{
        SandboxGateway, WorkspaceAccount, peers::MachineRegistry, stdio_launch,
    },
    auth::FileCredentialStore,
    sandbox::SandboxHost,
    session::resolve_mcp_connections,
    test_support::{
        composition_auth, default_agent_catalog, resource_state_with_overrides,
        sign_access_token_with_ttl, temp_root,
    },
};

const RESOURCE: &str = "mcp-servers/stdio-fixture.yaml";
const SOURCE: &str = "apiVersion: waltr.dev/v1alpha1
kind: McpServer
metadata: {name: stdio-fixture}
spec: {transport: stdio, command: '/bin/sh ./stdio-fixture.sh'}
";
const AGENT: &str = "apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: {name: coding}
spec:
  features:
    - mcp: {servers: {include: [stdio-fixture]}, instructions: true}
";
const SCRIPT: &str = r#"#!/bin/sh
IFS= read -r marker < marker || exit 1
while IFS= read -r request; do
    case "$request" in
        *'"method":"server/discover"'*)
            id=${request#*'"id":'}
            id=${id%%,*}
            printf '{"jsonrpc":"2.0","id":%s,"result":{"supportedVersions":["2026-07-28"],"capabilities":{},"ttlMs":0,"cacheScope":"private","instructions":"%s"}}\n' "$id" "$marker"
            ;;
    esac
done
"#;

fn factory(
    root: &Path,
    sandbox: crate::agent_host::sandbox_gateway::SandboxHandle,
    state: AgentDefinitionState,
    name: &str,
) -> ServerAgentRuntimeFactory {
    let source = AGENT.replace("stdio-fixture", name);
    let catalog =
        crate::agent_host::definition::AgentDefinitionCatalog::from_sources(vec![source], "coding")
            .unwrap();
    let initial = default_agent_catalog();
    ServerAgentRuntimeFactory::new(
        sandbox,
        root.into(),
        FileCredentialStore::new(root.join("auth.json")),
        initial
            .resolved(initial.default_agent_name())
            .unwrap()
            .clone(),
    )
    .with_agent_catalog(catalog)
    .with_mcp_resource_state(state.clone())
    .with_mcp_connection_resolver(move |request| {
        let state = state.clone();
        Box::pin(async move { resolve_mcp_connections(state.snapshot().configuration(), request) })
    })
}

fn etag(state: &AgentDefinitionState, path: &str) -> String {
    let AgentDefinitionInspection::Resource(detail) = state.inspect(Some(path)).unwrap() else {
        panic!("fixture resource must exist");
    };
    detail.persisted_etag
}

async fn session(
    factory: &ServerAgentRuntimeFactory,
    workspace: Option<(&str, &str)>,
) -> (SessionCoordinator<MemorySessionStore>, SessionRecord) {
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let mut request = SessionCreateRequest::new(SessionId::generate());
    request.agent_definition = Some(
        serde_json::to_value(
            factory
                .bundle_for_agent(factory.default_agent_name())
                .unwrap(),
        )
        .unwrap(),
    );
    let selection =
        workspace.map(
            |(machine, cwd)| waltr::grpc::waltr::session::v1::WorkspaceSelection {
                machine_id: machine.into(),
                working_directory: Some(cwd.into()),
            },
        );
    let session = factory
        .create_with_workspace(&sessions, request, selection.as_ref(), None)
        .await
        .unwrap();
    (sessions, session.record)
}

#[tokio::test]
async fn worker_stdio_uses_bound_cwd_reuses_live_client_and_revokes_idle_on_reload() {
    let state = resource_state_with_overrides(&[(RESOURCE, SOURCE)]);
    let root = temp_root();
    let bound = root.join("project with spaces");
    std::fs::create_dir_all(&bound).unwrap();
    std::fs::write(bound.join("marker"), "bound-worker-marker\n").unwrap();
    std::fs::write(bound.join("stdio-fixture.sh"), SCRIPT).unwrap();
    std::fs::write(
        bound.join("second.sh"),
        SCRIPT
            .replace("read -r marker", "read -r old_marker")
            .replace("while IFS=", "marker=reconfigured-worker\nwhile IFS="),
    )
    .unwrap();
    let worker = SandboxHost::start(root.clone(), None).await.unwrap();
    let factory = factory(&root, worker.handle(), state.clone(), "stdio-fixture");
    let (sessions, record) = session(&factory, Some(("local", "project with spaces"))).await;
    let registry = factory.mcp_registry();
    let first = registry
        .runtime(&factory, &record, &sessions)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first.instructions().await[0].instructions,
        "bound-worker-marker"
    );
    let again = registry
        .runtime(&factory, &record, &sessions)
        .await
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&first, &again));

    state
        .replace(
            RESOURCE,
            SOURCE.replace("stdio-fixture.sh", "second.sh"),
            &etag(&state, RESOURCE),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while first.reusable() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("idle channel should be fenced by the resource publication");
    let replacement = registry
        .runtime(&factory, &record, &sessions)
        .await
        .unwrap()
        .unwrap();
    assert!(!Arc::ptr_eq(&first, &replacement));
    assert_eq!(
        replacement.instructions().await[0].instructions,
        "reconfigured-worker"
    );
    registry.retire(&record.id).await.unwrap();
    assert!(!replacement.reusable());
    worker.shutdown().await;
}

async fn authenticate(auth: &AccessAuthenticator, token: &str) -> AuthenticatedPrincipal {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    auth.authenticate_bearer_headers(&headers, auth.api_required_scopes())
        .await
        .unwrap_or_else(|_| panic!("fixture bearer did not authenticate"))
}

#[tokio::test]
async fn remote_stdio_uses_exporter_launch_and_the_same_authenticated_account_only() {
    let destination = temp_root();
    let exporter = temp_root();
    let scoped = exporter.join("nested");
    std::fs::create_dir_all(&scoped).unwrap();
    std::fs::write(scoped.join("marker"), "exporter-only-marker\n").unwrap();
    std::fs::write(scoped.join("stdio-fixture.sh"), SCRIPT).unwrap();
    let destination_command = format!(
        "/usr/bin/touch {}",
        destination.join("must-not-exist").display()
    );
    let state = resource_state_with_overrides(&[(
        RESOURCE,
        &SOURCE.replace("/bin/sh ./stdio-fixture.sh", &destination_command),
    )]);
    let gateway = SandboxGateway::new("local".into()).with_export_control();
    let machines = MachineRegistry::default();
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let callback = "127.0.0.1:1".parse().unwrap();
    let (auth, token, issuer, auth_stop, auth_task) =
        composition_auth(&persistence, callback).await;
    let owner = authenticate(&auth, &token).await;
    let refreshed_token =
        sign_access_token_with_ttl(&issuer, "waltr:read", Duration::from_mins(45));
    let refreshed_owner = authenticate(&auth, &refreshed_token).await;
    let other_persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let (other_auth, other_token, _, other_auth_stop, other_auth_task) =
        composition_auth(&other_persistence, callback).await;
    let other = authenticate(&other_auth, &other_token).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = machines.mount(axum::Router::new(), auth);
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let worker = SandboxHost::start(exporter.clone(), None).await.unwrap();
    let exports = worker.export_control().connection().unwrap();
    let exported = waltr_agent_sandbox::peer_config::WorkspaceExport {
        endpoint,
        machine_id: "test-peer".into(),
        export_root: exporter,
    };
    let launch = stdio_launch("stdio-fixture", "/bin/sh ./stdio-fixture.sh").unwrap();
    let expiry = (std::time::SystemTime::now() + Duration::from_mins(5))
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    exports
        .apply(
            "fixture",
            exported.clone(),
            vec![launch.clone()],
            token.clone(),
            expiry,
        )
        .await
        .unwrap();
    let account = WorkspaceAccount::from_principal(&owner).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !machines.owns_live("test-peer", &account) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let factory = factory(&destination, gateway.handle(), state, "stdio-fixture")
        .with_workspace_machines(machines.clone())
        .with_workspace_account(Some(account.clone()));
    let (sessions, record) = session(&factory, Some(("test-peer", "nested"))).await;
    let registry = factory.mcp_registry();
    let runtime = registry
        .runtime(&factory, &record, &sessions)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        runtime.instructions().await[0].instructions,
        "exporter-only-marker"
    );
    let refreshed_factory = factory
        .clone()
        .with_workspace_account(WorkspaceAccount::from_principal(&refreshed_owner));
    let reused = registry
        .runtime(&refreshed_factory, &record, &sessions)
        .await
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&runtime, &reused));
    let denied = factory
        .clone()
        .with_workspace_account(WorkspaceAccount::from_principal(&other));
    let error = registry
        .runtime(&denied, &record, &sessions)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("belongs to another account"),
        "{error}"
    );
    assert!(runtime.reusable());
    assert!(!destination.join("must-not-exist").exists());
    exports.remove("fixture").await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while runtime.reusable() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        registry
            .runtime(&factory, &record, &sessions)
            .await
            .is_err()
    );
    std::fs::write(scoped.join("marker"), "fresh-exporter-stream\n").unwrap();
    exports
        .apply("fixture", exported, vec![launch], token, expiry)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !machines.owns_live("test-peer", &account) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let fresh = registry
        .runtime(&factory, &record, &sessions)
        .await
        .unwrap()
        .unwrap();
    assert!(!Arc::ptr_eq(&runtime, &fresh));
    assert_eq!(
        fresh.instructions().await[0].instructions,
        "fresh-exporter-stream"
    );
    // The new child is retired, but the old peer cannot confirm a correlated terminal.
    assert!(registry.retire(&record.id).await.is_err());
    assert!(!fresh.reusable());
    worker.shutdown().await;
    machines.shutdown();
    server.abort();
    auth_stop.cancel();
    other_auth_stop.cancel();
    auth_task.await.unwrap();
    other_auth_task.await.unwrap();
}

#[tokio::test]
async fn worker_stdio_rejects_absent_and_legacy_workers_without_configured_host_spawn() {
    let root = temp_root();
    let touched = root.join("must-not-exist");
    let command = format!("/usr/bin/touch {}", touched.display());
    let state = resource_state_with_overrides(&[(
        RESOURCE,
        &SOURCE.replace("/bin/sh ./stdio-fixture.sh", &command),
    )]);
    let gateway = SandboxGateway::new("local".into()).with_export_control();
    let factory = factory(&root, gateway.handle(), state, "stdio-fixture");
    let (sessions, record) = session(&factory, None).await;
    let registry = factory.mcp_registry();
    let missing = registry
        .runtime(&factory, &record, &sessions)
        .await
        .unwrap_err();
    assert!(
        missing.to_string().contains("sandbox") && !touched.exists(),
        "{missing}"
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let service = gateway.service();
    let host = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let channel = tonic::transport::Endpoint::from_shared(endpoint)
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = SandboxServiceClient::new(channel);
    let (send, receive) = mpsc::channel(1);
    send.send(proto::ConnectRequest {
        body: Some(WorkerBody::Hello(proto::Hello {
            workspace_id: "local".into(),
            worker_id: "legacy-fixture".into(),
            canonical_root: root.to_str().unwrap().into(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let mut stream = client
        .connect(ReceiverStream::new(receive))
        .await
        .unwrap()
        .into_inner();
    let legacy = registry
        .runtime(&factory, &record, &sessions)
        .await
        .unwrap_err();
    assert!(
        legacy.to_string().contains("does not support stdio"),
        "{legacy}"
    );
    assert!(!touched.exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), stream.message())
            .await
            .is_err()
    );
    registry.retire_all().await.unwrap();
    drop(send);
    host.abort();
}

#[tokio::test]
async fn http_only_mcp_registry_does_not_require_a_worker_connection() {
    struct Empty;
    impl mcp::server::ServerHandler for Empty {
        fn capabilities(&self) -> mcp::server::ServerCapabilities {
            mcp::server::ServerCapabilities::default()
        }
        fn server_info(&self) -> mcp::server::ServerInfo {
            mcp::server::ServerInfo::new("empty", "1")
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let router = mcp::server::streamable_http_router(Arc::new(Empty));
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let source = SOURCE.replace("stdio-fixture", "synthetic-http").replace(
        "transport: stdio, command: '/bin/sh ./synthetic-http.sh'",
        &format!("transport: streamableHttp, url: '{url}'"),
    );
    let state = resource_state_with_overrides(&[("mcp-servers/synthetic-http.yaml", &source)]);
    let gateway = SandboxGateway::new("local".into()).with_export_control();
    let root = temp_root();
    let factory = factory(&root, gateway.handle(), state, "synthetic-http");
    let (sessions, record) = session(&factory, None).await;
    let registry = factory.mcp_registry();
    factory.for_session_workspace(&record).await.unwrap();
    assert!(
        registry
            .runtime(&factory, &record, &sessions)
            .await
            .unwrap()
            .is_some()
    );
    registry.retire_all().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn worker_preparation_cancellation_and_failed_validation_do_not_replay() {
    let root = temp_root();
    let state = resource_state_with_overrides(&[
        (RESOURCE, SOURCE),
        (
            "mcp-servers/unselected.yaml",
            &SOURCE.replace("name: stdio-fixture", "name: unselected"),
        ),
    ]);
    let gateway = SandboxGateway::new("local".into()).with_export_control();
    let factory = factory(&root, gateway.handle(), state.clone(), "stdio-fixture");
    let (sessions, record) = session(&factory, None).await;
    let registry = factory.mcp_registry();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let service = gateway.service();
    let host = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let channel = tonic::transport::Endpoint::from_shared(endpoint)
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = SandboxServiceClient::new(channel);
    let (send, receive) = mpsc::channel(8);
    let answer = |body| {
        let send = send.clone();
        async move {
            send.send(proto::ConnectRequest { body: Some(body) })
                .await
                .unwrap();
        }
    };
    answer(WorkerBody::Hello(proto::Hello {
        workspace_id: "local".into(),
        worker_id: "stdio-fixture".into(),
        canonical_root: root.to_str().unwrap().into(),
        mcp_stdio_version: 1,
        ..Default::default()
    }))
    .await;
    let mut stream = client
        .connect(ReceiverStream::new(receive))
        .await
        .unwrap()
        .into_inner();
    let prepare = || {
        tokio::spawn({
            let registry = registry.clone();
            let id = record.clone();
            let factory = factory.clone();
            let sessions = sessions.clone();
            async move { registry.runtime(&factory, &id, &sessions).await }
        })
    };

    async fn next(stream: &mut tonic::Streaming<proto::ConnectResponse>) -> HostBody {
        tokio::time::timeout(Duration::from_secs(3), stream.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .body
            .unwrap()
    }
    let publishing = prepare();
    let HostBody::McpStdioCatalogUpdate(catalog) = next(&mut stream).await else {
        panic!("expected private catalog");
    };
    assert_eq!(
        catalog
            .launches
            .iter()
            .map(|launch| launch.name.as_str())
            .collect::<Vec<_>>(),
        ["stdio-fixture", "unselected"]
    );
    publishing.abort();
    assert!(publishing.await.unwrap_err().is_cancelled());
    answer(WorkerBody::McpStdioCatalogAck(proto::McpStdioCatalogAck {
        request_id: catalog.request_id,
        applied: true,
    }))
    .await;
    // A dropped preparation finishes the already-sent publication but never opens a child.
    registry.retire(&record.id).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), stream.message())
            .await
            .is_err()
    );
    let discovering = prepare();
    let HostBody::McpStdioOpen(open) = next(&mut stream).await else {
        panic!("expected open");
    };
    assert_eq!(open.working_directory, ".");
    answer(WorkerBody::McpStdioOpened(proto::McpStdioOpened {
        channel_id: open.channel_id.clone(),
        opened: true,
    }))
    .await;
    let HostBody::McpStdioFrame(frame) = next(&mut stream).await else {
        panic!("expected discovery");
    };
    assert_eq!(frame.channel_id, open.channel_id);
    assert!(
        String::from_utf8(frame.data)
            .unwrap()
            .contains("server/discover")
    );
    discovering.abort();
    assert!(discovering.await.unwrap_err().is_cancelled());
    let HostBody::McpStdioClose(close) = next(&mut stream).await else {
        panic!("expected close instead of a replay");
    };
    assert_eq!(close.channel_id, open.channel_id);
    answer(WorkerBody::McpStdioTerminal(proto::McpStdioTerminal {
        channel_id: close.channel_id,
        status: proto::McpStdioTerminalStatus::Closed as i32,
        reaped: true,
    }))
    .await;
    registry.retire(&record.id).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), stream.message())
            .await
            .is_err()
    );

    let revoked = prepare();
    let HostBody::McpStdioOpen(open) = next(&mut stream).await else {
        panic!("expected new open after a fresh request");
    };
    state
        .replace(
            RESOURCE,
            SOURCE.replace("stdio-fixture.sh", "revoked.sh"),
            &etag(&state, RESOURCE),
        )
        .await
        .unwrap();
    answer(WorkerBody::McpStdioOpened(proto::McpStdioOpened {
        channel_id: open.channel_id.clone(),
        opened: true,
    }))
    .await;
    let HostBody::McpStdioClose(close) = next(&mut stream).await else {
        panic!("expected close, not discovery on a revoked resource");
    };
    assert_eq!(close.channel_id, open.channel_id);
    gateway.shutdown();
    let error = revoked.await.unwrap().unwrap_err().to_string();
    assert!(error.contains("child cleanup not confirmed"), "{error}");
    host.abort();
}
