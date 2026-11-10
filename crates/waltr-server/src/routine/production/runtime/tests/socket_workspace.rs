use super::*;

#[tokio::test]
async fn socket_workspace_absent_on_server_runs_production_routines() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let root = crate::test_support::temp_root();
        let resources = root.join("resources");
        seed(&resources);
        let workspace = PathBuf::from(format!("/nonexistent-on-host/{}", uuid::Uuid::new_v4()));
        assert!(!workspace.exists());
        let socket = root.join("gateway.sock");
        let worker_stop = CancellationToken::new();
        let stopping = worker_stop.clone();
        let worker_root = workspace.clone();
        let worker_socket = socket.clone();
        let worker = tokio::spawn(async move {
            use hyper_util::rt::TokioIo;
            use tokio_stream::wrappers::ReceiverStream;
            use waltr::grpc::waltr::sandbox::v1 as worker;
            let stream = loop {
                match tokio::net::UnixStream::connect(&worker_socket).await {
                    Ok(stream) => break stream,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                }
            };
            let stream = Arc::new(Mutex::new(Some(stream)));
            let channel = tonic::transport::Endpoint::from_static("http://sandbox.local")
                .connect_with_connector(tower::service_fn(move |_| {
                    let stream = stream.lock().unwrap().take().unwrap();
                    async move { Ok::<_, std::io::Error>(TokioIo::new(stream)) }
                })).await.unwrap();
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tx.send(worker::ConnectRequest { body: Some(worker::connect_request::Body::Hello(worker::Hello {
                worker_id: "absent-workspace-fixture".into(), workspace_id: "local".into(),
                generation: 1, tools: vec![], canonical_root: worker_root.to_str().unwrap().into(),
            })) }).await.unwrap();
            let mut client = worker::sandbox_service_client::SandboxServiceClient::new(channel);
            let mut responses = client.connect(ReceiverStream::new(rx)).await.unwrap().into_inner();
            loop {
                tokio::select! {
                    () = stopping.cancelled() => break,
                    response = responses.message() => if response.unwrap().is_none() { break; },
                }
            }
            drop(tx);
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let loader = crate::test_support::default_catalog_loader();
        let config_service = ConfigService::new(resources, loader.clone()).await.unwrap();
        let persistence = Persistence::initialize(crate::persistence::DatabaseConfig::from_urls(
            &format!("sqlite://{}", root.join("waltr.sqlite3").display()),
            &format!("sqlite://{}", root.join("mcp.sqlite3").display()),
        ).unwrap()).await.unwrap();
        let (authenticator, access_token, _, issuer_stop, issuer_task) =
            crate::test_support::composition_auth(&persistence, address).await;
        let sessions = SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
        let app = assemble_app(AppServices {
            config_service, persistence, catalog_loader: loader, access_authenticator: authenticator,
            mcp_oauth: None, outbound_private_origins: Vec::new(),
            outbound_oauth: OutboundOAuthConfig::default(), automatic_self_mcp_resource: None,
            agent_state: None, local_admission: None, workspace_root: workspace.clone(),
            sandbox_socket: Some(socket), routine_authority: RoutineAuthority::default(),
            routine_private_parent: root,
        }, |routes, input| {
            let service = crate::session::SessionService::from_sessions(sessions, input);
            routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));
            Ok(service)
        }).await.expect("socket workspace need not exist on server");
        assert!(!workspace.exists());
        assert!(!worker.is_finished(), "mock worker must remain connected");
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let server = tokio::spawn(app.serve_with_shutdown(listener, shutdown.cancelled_owned()));
        exercise(address, Some(&access_token)).await;
        assert!(!workspace.exists());
        assert!(!worker.is_finished(), "mock worker must remain connected");
        stop.cancel();
        server.await.unwrap().unwrap();
        worker_stop.cancel();
        worker.await.unwrap();
        issuer_stop.cancel();
        issuer_task.await.unwrap();
    }).await.expect("bounded socket production composition");
}
