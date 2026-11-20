use tokio_stream::StreamExt as _;

use super::*;

fn hello(version: u32) -> proto::Hello {
    proto::Hello {
        worker_id: "worker".into(),
        workspace_id: "workspace".into(),
        canonical_root: "/export".into(),
        mcp_stdio_version: version,
        ..Default::default()
    }
}

async fn next(stream: &mut OutboundStream) -> HostBody {
    tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .body
        .unwrap()
}

fn answer(guard: &ConnectionGuard, body: WorkerBody) {
    guard.inner.dispatch(guard.epoch, body).unwrap();
}

async fn publish(
    target: &SandboxStdioTarget,
    launches: Vec<proto::McpStdioLaunch>,
    guard: &ConnectionGuard,
    stream: &mut OutboundStream,
) {
    let expected = launches.clone();
    let (result, ()) = tokio::join!(target.publish_local_catalog(launches), async {
        let HostBody::McpStdioCatalogUpdate(update) = next(stream).await else {
            panic!("expected private catalog update");
        };
        assert_eq!(update.launches, expected);
        for launch in &expected {
            assert!(
                target
                    .open_local(launch, CancellationToken::new())
                    .await
                    .is_err()
            );
        }
        answer(
            guard,
            WorkerBody::McpStdioCatalogAck(proto::McpStdioCatalogAck {
                request_id: "stale".into(),
                applied: false,
            }),
        );
        answer(
            guard,
            WorkerBody::McpStdioCatalogAck(proto::McpStdioCatalogAck {
                request_id: update.request_id,
                applied: true,
            }),
        );
    });
    result.unwrap();
}

async fn open_reply_named(
    guard: &ConnectionGuard,
    stream: &mut OutboundStream,
    name: &str,
    opened: bool,
) -> String {
    let HostBody::McpStdioOpen(open) = next(stream).await else {
        panic!("expected stdio open");
    };
    assert_eq!(open.server_name, name);
    assert_eq!(open.working_directory, ".");
    answer(
        guard,
        WorkerBody::McpStdioOpened(proto::McpStdioOpened {
            channel_id: open.channel_id.clone(),
            opened,
        }),
    );
    open.channel_id
}

async fn open_reply(guard: &ConnectionGuard, stream: &mut OutboundStream, opened: bool) -> String {
    open_reply_named(guard, stream, "echo", opened).await
}

fn frame(id: &str, data: &[u8]) -> WorkerBody {
    WorkerBody::McpStdioFrame(proto::McpStdioFrame {
        channel_id: id.into(),
        data: data.into(),
    })
}

fn terminal(id: &str, status: Terminal, reaped: bool) -> WorkerBody {
    WorkerBody::McpStdioTerminal(proto::McpStdioTerminal {
        channel_id: id.into(),
        status: status as i32,
        reaped,
    })
}

#[tokio::test]
async fn local_catalog_acks_open_frames_and_confirms_close() {
    let gateway = SandboxGateway::new("workspace".into()).with_export_control();
    let (guard, _, mut stream) = gateway.admit(hello(VERSION), None).unwrap();
    let target = gateway.handle().stdio_target().unwrap();
    let launch = stdio_launch("echo", "/inert/bin --fixture").unwrap();
    let missing = stdio_launch("missing", "definitely-not-installed-waltr-mcp").unwrap();
    assert!(
        target
            .open_local(&launch, CancellationToken::new())
            .await
            .is_err()
    );
    publish(
        &target,
        vec![launch.clone(), missing.clone()],
        &guard,
        &mut stream,
    )
    .await;
    let (rejected, old_id) = tokio::join!(
        target.open_local(&missing, CancellationToken::new()),
        open_reply_named(&guard, &mut stream, "missing", false),
    );
    assert!(matches!(rejected, Err(SandboxStdioOpenError::Rejected)));
    assert!(target.same_connection(&gateway.handle().stdio_target().unwrap()));
    answer(&guard, frame(&old_id, b"late"));
    let (channel, id) = tokio::join!(
        target.open_local(&launch, CancellationToken::new()),
        open_reply(&guard, &mut stream, true),
    );
    let channel = channel.unwrap();
    assert_ne!(id, old_id);
    let unrelated = stdio_launch("other", "/inert/other").unwrap();
    publish(
        &target,
        vec![launch.clone(), unrelated],
        &guard,
        &mut stream,
    )
    .await;
    assert!(channel.send(vec![b'\n']).await.is_err());
    assert!(channel.send(vec![0; FRAME_BYTES + 1]).await.is_err());
    channel.send(b"in".to_vec()).await.unwrap();
    let HostBody::McpStdioFrame(sent) = next(&mut stream).await else {
        panic!("expected frame");
    };
    assert_eq!(sent.channel_id, id);
    assert_eq!(sent.data, b"in");
    answer(&guard, frame(&id, b"out"));
    assert_eq!(channel.recv().await.unwrap().unwrap(), b"out");
    let (result, ()) = tokio::join!(channel.shutdown(), async {
        let HostBody::McpStdioClose(close) = next(&mut stream).await else {
            panic!("expected close");
        };
        assert_eq!(close.channel_id, id);
        answer(&guard, terminal(&id, Terminal::Closed, true));
    });
    assert_eq!(result.unwrap(), Terminal::Closed);
    assert!(channel.send(vec![]).await.is_err());
}

#[tokio::test]
async fn uncorrelated_negative_ack_and_stream_loss_are_not_definitive_rejection() {
    let gateway = SandboxGateway::new("workspace".into()).with_export_control();
    let (guard, _, mut stream) = gateway.admit(hello(VERSION), None).unwrap();
    let target = gateway.handle().stdio_target().unwrap();
    let launch = stdio_launch("echo", "/inert/bin").unwrap();
    publish(&target, vec![launch.clone()], &guard, &mut stream).await;
    let (result, ()) = tokio::join!(
        target.open_local(&launch, CancellationToken::new()),
        async {
            let HostBody::McpStdioOpen(open) = next(&mut stream).await else {
                panic!("expected open");
            };
            let unrelated = Uuid::now_v7().to_string();
            assert_ne!(open.channel_id, unrelated);
            answer(
                &guard,
                WorkerBody::McpStdioOpened(proto::McpStdioOpened {
                    channel_id: unrelated,
                    opened: false,
                }),
            );
            drop(guard);
        },
    );
    assert!(matches!(result, Err(SandboxStdioOpenError::Transport(_))));
}

#[tokio::test]
async fn reconnect_republishes_and_disconnected_channels_cannot_confirm_cleanup() {
    let gateway = SandboxGateway::new("workspace".into()).with_export_control();
    let launch = stdio_launch("echo", "/inert/bin").unwrap();
    for version in [0, VERSION + 1] {
        let (old, _, _) = gateway.admit(hello(version), None).unwrap();
        assert!(gateway.handle().stdio_target().is_err());
        drop(old);
    }
    let (guard, _, mut stream) = gateway.admit(hello(VERSION), None).unwrap();
    let old = gateway.handle().stdio_target().unwrap();
    publish(&old, vec![launch.clone()], &guard, &mut stream).await;
    let (channel, old_id) = tokio::join!(
        old.open_local(&launch, CancellationToken::new()),
        open_reply(&guard, &mut stream, true),
    );
    let channel = channel.unwrap();
    answer(&guard, frame(&old_id, b"buffered"));
    drop(guard);
    old.retired().await;
    assert!(channel.recv().await.is_err());
    assert!(channel.shutdown().await.is_err());

    let (guard, _, mut stream) = gateway.admit(hello(VERSION), None).unwrap();
    let new = gateway.handle().stdio_target().unwrap();
    assert_ne!(old.connection_epoch(), new.connection_epoch());
    assert!(
        old.publish_local_catalog(vec![launch.clone()])
            .await
            .is_err()
    );
    assert!(
        new.open_local(&launch, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        gateway
            .inner
            .dispatch(old.epoch, frame(&old_id, b"old epoch"))
            .is_err()
    );
    answer(&guard, frame(&old_id, b"old id"));
    publish(&new, vec![launch.clone()], &guard, &mut stream).await;
    let token = CancellationToken::new();
    let (channel, id) = tokio::join!(
        new.open_local(&launch, token.clone()),
        open_reply(&guard, &mut stream, true),
    );
    let channel = channel.unwrap();
    channel
        .send(b"discard on cancellation".to_vec())
        .await
        .unwrap();
    token.cancel();
    let HostBody::McpStdioClose(close) = next(&mut stream).await else {
        panic!("cancelled frame crossed fence instead of close");
    };
    assert_eq!(close.channel_id, id);
    answer(&guard, terminal(&id, Terminal::Failed, false));
    assert!(channel.shutdown().await.is_err());
}

#[tokio::test]
async fn local_replacement_revokes_only_changed_channels() {
    let gateway = SandboxGateway::new("workspace".into()).with_export_control();
    let (guard, _, mut stream) = gateway.admit(hello(VERSION), None).unwrap();
    let target = gateway.handle().stdio_target().unwrap();
    let launch = stdio_launch("echo", "/inert/bin").unwrap();
    publish(&target, vec![launch.clone()], &guard, &mut stream).await;
    let (channel, id) = tokio::join!(
        target.open_local(&launch, CancellationToken::new()),
        open_reply(&guard, &mut stream, true),
    );
    let channel = channel.unwrap();
    channel.send(b"old input".to_vec()).await.unwrap();
    answer(&guard, frame(&id, b"old output"));
    let (result, ()) = tokio::join!(target.publish_local_catalog(Vec::new()), async {
        let HostBody::McpStdioCatalogUpdate(update) = next(&mut stream).await else {
            panic!("revoked frame crossed fence instead of catalog update");
        };
        assert!(update.launches.is_empty());
        assert!(channel.recv().await.is_err());
        assert!(
            target
                .open_local(&launch, CancellationToken::new())
                .await
                .is_err()
        );
        answer(&guard, terminal(&id, Terminal::Revoked, true));
        answer(
            &guard,
            WorkerBody::McpStdioCatalogAck(proto::McpStdioCatalogAck {
                request_id: update.request_id,
                applied: true,
            }),
        );
    });
    result.unwrap();
    assert_eq!(channel.shutdown().await.unwrap(), Terminal::Revoked);
}

#[tokio::test]
async fn rejected_catalog_replacement_retires_the_epoch() {
    let gateway = SandboxGateway::new("workspace".into()).with_export_control();
    let (guard, _, mut stream) = gateway.admit(hello(VERSION), None).unwrap();
    let target = gateway.handle().stdio_target().unwrap();
    let launch = stdio_launch("echo", "/inert/bin").unwrap();
    publish(&target, vec![launch.clone()], &guard, &mut stream).await;
    let (result, ()) = tokio::join!(target.publish_local_catalog(Vec::new()), async {
        let HostBody::McpStdioCatalogUpdate(update) = next(&mut stream).await else {
            panic!("expected replacement");
        };
        answer(
            &guard,
            WorkerBody::McpStdioCatalogAck(proto::McpStdioCatalogAck {
                request_id: update.request_id,
                applied: false,
            }),
        );
    });
    assert!(result.is_err());
    target.retired().await;
    assert!(
        target
            .open_local(&launch, CancellationToken::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn hosted_target_requires_bound_account_and_never_sends_launch_definitions() {
    let peers = peers::MachineRegistry::default();
    let owner = WorkspaceAccount {
        issuer: "issuer".into(),
        subject: "owner".into(),
    };
    let foreign = WorkspaceAccount {
        issuer: "issuer".into(),
        subject: "foreign".into(),
    };
    let expires = Instant::now() + Duration::from_secs(30);
    let (guard, _, mut stream) = peers.claim(hello(VERSION), owner.clone(), expires).unwrap();
    assert!(peers.resolve("workspace", None, &foreign).await.is_err());
    let (handle, ()) = tokio::join!(peers.resolve("workspace", None, &owner), async {
        let HostBody::WorkspaceRequest(request) = next(&mut stream).await else {
            panic!("expected request to resolve the export directory");
        };
        answer(
            &guard,
            WorkerBody::WorkspaceResponse(proto::WorkspaceResponse {
                request_id: request.request_id,
                canonical_root: "/export".into(),
                working_directory: Some(".".into()),
                ..Default::default()
            }),
        );
    },);
    let handle = handle.unwrap();
    let target = handle.stdio_target().unwrap();
    let launch = stdio_launch("echo", "/inert/bin").unwrap();
    assert!(
        target
            .publish_local_catalog(vec![launch.clone()])
            .await
            .is_err()
    );
    assert!(
        target
            .open_local(&launch, CancellationToken::new())
            .await
            .is_err()
    );
    let (channel, id) = tokio::join!(
        target.open_exported("echo", CancellationToken::new()),
        open_reply(&guard, &mut stream, true),
    );
    let channel = channel.unwrap();
    answer(&guard, frame(&id, b"before exit"));
    answer(&guard, terminal(&id, Terminal::Exited, true));
    assert_eq!(channel.recv().await.unwrap().unwrap(), b"before exit");
    assert!(channel.recv().await.unwrap().is_none());
    assert_eq!(channel.shutdown().await.unwrap(), Terminal::Exited);
    drop(guard);
    let (_new, _, _) = peers.claim(hello(VERSION), owner, expires).unwrap();
    assert!(handle.stdio_target().is_err());
}
