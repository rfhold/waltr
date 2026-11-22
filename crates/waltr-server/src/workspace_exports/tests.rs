use std::future::pending;

use tokio::{net::TcpListener, sync::mpsc, time::timeout};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use waltr::grpc::waltr::sandbox::v1::{
    self as proto, connect_request::Body as WorkerBody, connect_response::Body as HostBody,
    sandbox_service_client::SandboxServiceClient, workspace_export_control::Change,
};

use crate::{
    agent_host::sandbox_gateway::SandboxGateway, test_support::resource_state_with_overrides,
};

use super::*;

const BINDING: &str = "remotes/first.yaml";
const FIRST: &str = "apiVersion: waltr.dev/v1alpha1
kind: Remote
metadata: {name: first}
spec: {endpoint: 'http://127.0.0.1:1234', binding: {machineId: first, exportRoot: /first, mcpStdioServers: [exported]}}
";
const UNRELATED: &str = "apiVersion: waltr.dev/v1alpha1
kind: Remote
metadata: {name: unrelated}
spec: {endpoint: 'http://127.0.0.1:2345'}
";
const SERVER: &str = "mcp-servers/exported.yaml";
const STDIO: &str = "apiVersion: waltr.dev/v1alpha1
kind: McpServer
metadata: {name: exported}
spec: {transport: stdio, command: '/inert/fixture --initial'}
";
const OTHER_SERVER: &str = "mcp-servers/other.yaml";

fn expect_apply(change: Option<Change>, arg: &str) {
    let Some(Change::Apply(export)) = change else {
        panic!("expected export apply");
    };
    assert_eq!(export.export_root, "/first");
    assert_eq!(
        export.mcp_stdio_launches,
        vec![stdio_launch("exported", &format!("/inert/fixture {arg}")).unwrap()]
    );
}

fn etag(catalog: &AgentDefinitionState, path: &str) -> String {
    let crate::resource_catalog::AgentDefinitionInspection::Resource(detail) =
        catalog.inspect(Some(path)).unwrap()
    else {
        panic!("expected resource detail")
    };
    detail.persisted_etag
}

type WorkerStream = tonic::Streaming<proto::ConnectResponse>;

async fn command(stream: &mut WorkerStream) -> proto::WorkspaceExportControl {
    let response = timeout(Duration::from_secs(4), stream.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Some(HostBody::WorkspaceExportControl(control)) = response.body else {
        panic!("expected export control");
    };
    assert_eq!(control.binding, "first");
    control
}

async fn ack(tx: &mpsc::Sender<proto::ConnectRequest>, request_id: String) {
    tx.send(proto::ConnectRequest {
        body: Some(WorkerBody::WorkspaceExportAck(proto::WorkspaceExportAck {
            request_id,
            applied: true,
        })),
    })
    .await
    .unwrap();
}

fn test_lease(access: &str, authority: &str) -> RemoteCredentialLease {
    RemoteCredentialLease {
        access_token: access.into(),
        expires_at_ms: i64::try_from(
            (SystemTime::now().duration_since(UNIX_EPOCH).unwrap() + Duration::from_secs(60))
                .as_millis(),
        )
        .unwrap(),
        authority_id: authority.into(),
    }
}

#[tokio::test]
async fn export_lifecycle_rotates_and_stops_independently_of_unrelated_lease() {
    let catalog = resource_state_with_overrides(&[
        (BINDING, FIRST),
        ("remotes/unrelated.yaml", UNRELATED),
        (SERVER, STDIO),
        (OTHER_SERVER, &STDIO.replace("exported", "other")),
    ]);
    let gateway = SandboxGateway::new("workspace".into()).with_export_control();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let gateway = gateway.clone();
        async move {
            tonic::transport::Server::builder()
                .add_service(gateway.service())
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        }
    });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = SandboxServiceClient::new(channel);
    let (tx, rx) = mpsc::channel(16);
    tx.send(proto::ConnectRequest {
        body: Some(WorkerBody::Hello(proto::Hello {
            worker_id: "worker".into(),
            workspace_id: "workspace".into(),
            generation: 1,
            tools: vec![],
            canonical_root: "/worker/root".into(),
            mcp_stdio_version: 1,
        })),
    })
    .await
    .unwrap();
    let mut stream = client
        .connect(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let current = Arc::new(std::sync::Mutex::new(Some(test_lease(
        "initial",
        "first-grant",
    ))));
    let stop = CancellationToken::new();
    let task = tokio::spawn(supervise(
        catalog.clone(),
        gateway.export_control(),
        stop.clone(),
        {
            let current = current.clone();
            move |key, _| {
                let current = current.clone();
                async move {
                    if key == "unrelated" {
                        pending::<Option<RemoteCredentialLease>>().await
                    } else {
                        current
                            .lock()
                            .unwrap()
                            .as_ref()
                            .map(|lease| RemoteCredentialLease {
                                access_token: lease.access_token.clone(),
                                expires_at_ms: lease.expires_at_ms,
                                authority_id: lease.authority_id.clone(),
                            })
                    }
                }
            }
        },
    ));

    let apply = command(&mut stream).await;
    expect_apply(apply.change, "--initial");
    catalog
        .replace(
            OTHER_SERVER,
            STDIO
                .replace("exported", "other")
                .replace("--initial", "--unrelated"),
            &etag(&catalog, OTHER_SERVER),
        )
        .await
        .unwrap();
    *current.lock().unwrap() = Some(test_lease("rotated", "first-grant"));
    ack(&tx, apply.request_id).await;
    let rotate = command(&mut stream).await;
    assert!(matches!(rotate.change, Some(Change::Rotate(_))));
    *current.lock().unwrap() = None;
    ack(&tx, rotate.request_id).await;
    let remove = command(&mut stream).await;
    assert!(matches!(remove.change, Some(Change::Remove(_))));
    *current.lock().unwrap() = Some(test_lease("replacement", "second-grant"));
    ack(&tx, remove.request_id).await;
    let apply = command(&mut stream).await;
    expect_apply(apply.change, "--initial");
    let changed = catalog
        .replace(
            BINDING,
            FIRST.replace("/first", "/changed"),
            &etag(&catalog, BINDING),
        )
        .await
        .unwrap();
    // The latest binding looks unchanged when Apply returns, but it is a new incarnation.
    catalog
        .replace(BINDING, FIRST.to_owned(), &changed.persisted_etag)
        .await
        .unwrap();
    ack(&tx, apply.request_id).await;
    let removed_before_replacement = command(&mut stream).await;
    assert!(matches!(
        removed_before_replacement.change,
        Some(Change::Remove(_))
    ));
    ack(&tx, removed_before_replacement.request_id).await;
    let replacement = command(&mut stream).await;
    expect_apply(replacement.change, "--initial");

    // A referenced resource edit and revert both land while Apply is in flight.
    let edited = catalog
        .replace(
            SERVER,
            STDIO.replace("--initial", "--reverted"),
            &etag(&catalog, SERVER),
        )
        .await
        .unwrap();
    catalog
        .replace(SERVER, STDIO.to_owned(), &edited.persisted_etag)
        .await
        .unwrap();
    ack(&tx, replacement.request_id).await;
    let remove = command(&mut stream).await;
    assert!(matches!(remove.change, Some(Change::Remove(_))));
    ack(&tx, remove.request_id).await;
    let apply = command(&mut stream).await;
    expect_apply(apply.change, "--initial");
    ack(&tx, apply.request_id).await;

    let edited = catalog
        .replace(
            SERVER,
            STDIO.replace("--initial", "--edited"),
            &etag(&catalog, SERVER),
        )
        .await
        .unwrap();
    let remove = command(&mut stream).await;
    assert!(matches!(remove.change, Some(Change::Remove(_))));
    ack(&tx, remove.request_id).await;
    let apply = command(&mut stream).await;
    expect_apply(apply.change, "--edited");
    ack(&tx, apply.request_id).await;

    let missing = catalog.reset(SERVER, &edited.persisted_etag).await.unwrap();
    let remove = command(&mut stream).await;
    assert!(matches!(remove.change, Some(Change::Remove(_))));
    ack(&tx, remove.request_id).await;
    assert!(
        timeout(Duration::from_millis(100), stream.message())
            .await
            .is_err()
    );
    let http = catalog
        .replace(
            SERVER,
            STDIO.replace(
                "transport: stdio, command: '/inert/fixture --initial'",
                "transport: streamableHttp, url: 'https://inert.test/mcp'",
            ),
            &missing.persisted_etag,
        )
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_millis(100), stream.message())
            .await
            .is_err()
    );
    catalog
        .replace(SERVER, STDIO.to_owned(), &http.persisted_etag)
        .await
        .unwrap();
    let apply = command(&mut stream).await;
    expect_apply(apply.change, "--initial");
    ack(&tx, apply.request_id).await;
    let task = tokio::spawn(
        WorkspaceExports {
            stop,
            task: Some(task),
        }
        .shutdown(),
    );
    let shutdown = command(&mut stream).await;
    assert!(matches!(shutdown.change, Some(Change::Remove(_))));
    ack(&tx, shutdown.request_id).await;
    timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    gateway.shutdown();
    server.abort();
    drop(server.await);
}
