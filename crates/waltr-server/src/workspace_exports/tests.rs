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

const BINDING: &str = "workspace-bindings/first.yaml";
const FIRST: &str = "apiVersion: waltr.dev/v1alpha1
kind: WorkspaceBinding
metadata: {name: first}
spec: {endpoint: 'http://127.0.0.1:1234', machineId: first, exportRoot: /first}
";
const UNRELATED: &str = "apiVersion: waltr.dev/v1alpha1
kind: WorkspaceBinding
metadata: {name: unrelated}
spec: {endpoint: 'http://127.0.0.1:1234', machineId: unrelated, exportRoot: /unrelated}
";

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

fn test_lease(access: &str, authority: &str) -> WorkspaceBindingCredentialLease {
    WorkspaceBindingCredentialLease {
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
        ("workspace-bindings/unrelated.yaml", UNRELATED),
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
                        pending::<Option<WorkspaceBindingCredentialLease>>().await
                    } else {
                        current.lock().unwrap().as_ref().map(|lease| {
                            WorkspaceBindingCredentialLease {
                                access_token: lease.access_token.clone(),
                                expires_at_ms: lease.expires_at_ms,
                                authority_id: lease.authority_id.clone(),
                            }
                        })
                    }
                }
            }
        },
    ));

    let apply = command(&mut stream).await;
    assert!(matches!(apply.change, Some(Change::Apply(_))));
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
    assert!(matches!(apply.change, Some(Change::Apply(_))));
    let crate::resource_catalog::AgentDefinitionInspection::Resource(detail) =
        catalog.inspect(Some(BINDING)).unwrap()
    else {
        panic!("expected resource detail")
    };
    let changed = catalog
        .replace(
            BINDING,
            FIRST.replace("/first", "/changed"),
            &detail.persisted_etag,
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
    let Some(Change::Apply(export)) = replacement.change else {
        panic!("expected reconfigured export")
    };
    assert_eq!(export.export_root, "/first");
    ack(&tx, replacement.request_id).await;
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
