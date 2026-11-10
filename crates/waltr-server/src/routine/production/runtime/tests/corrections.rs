use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Live(Arc<AtomicUsize>);
impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct LocalProvider {
    live: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    ordinary: ScriptedProvider,
}
impl waltr_agent::LlmStream for LocalProvider {
    fn name(&self) -> &'static str {
        "local-generation-fixture"
    }
    fn stream_request<'a>(
        &'a self,
        request: waltr_agent::CompletionRequest<'a>,
    ) -> waltr_agent::BoxStreamFuture<'a> {
        assert!(request.tools.iter().all(|tool| tool.name != "code"));
        if !format!("{:?}", request.prompt).contains("LOCAL_GENERATION_BLOCK") {
            return self.ordinary.stream_request(request);
        }
        self.live.fetch_add(1, Ordering::SeqCst);
        let live = Live(self.live.clone());
        self.entered.notify_one();
        Box::pin(async move {
            Ok(
                Box::pin(futures_util::stream::unfold(live, |live| async move {
                    std::future::pending::<()>().await;
                    Some((Ok(waltr_agent::CompletionEvent::Finished), live))
                })) as waltr_agent::BoxCompletionStream,
            )
        })
    }
}

#[tokio::test]
async fn embedded_mcp_generation_origin_creates_sends_and_joins() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::corrections::embedded_mcp_generation_origin_creates_sends_and_joins",
    ) {
        return;
    }
    let root = crate::test_support::temp_root();
    let resources = root.join("resources");
    seed(&resources);
    let live = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(LocalProvider {
        live: live.clone(),
        entered: entered.clone(),
        ordinary: Default::default(),
    });
    let embedded = crate::embedded::build_embedded_server_with_registration(crate::embedded::EmbeddedServerConfig {
        resources_root: resources, waltr_database_path: root.join("data/waltr.sqlite3"), mcp_database_path: root.join("data/mcp.sqlite3"),
        session_root: root.join("sessions"), workspace_root: root.join("workspace"),
        catalog_source: crate::embedded::EmbeddedCatalogSource::Path(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models-dev-defaults.json")),
        catalog_cache_path: root.join("cache/models.json"), catalog_fallback: None, default_agent: "routine-title".into(),
        local_bearer: crate::LocalBearer::new("current-local-generation").unwrap(), outbound_policy: Default::default(), routine_authority: Default::default(),
    }, move |routes, input, root| {
        let sessions = waltr_agent::SessionCoordinator::new(waltr_agent::FilesystemSessionStore::new(root));
        let service = Arc::try_unwrap(crate::session::SessionService::from_sessions(sessions, input)).ok().unwrap().with_test_provider(provider);
        let service = Arc::new(service);
        routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));
        Ok(service)
    }).await.unwrap();
    let address = embedded.local_addr();
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let task =
        tokio::spawn(embedded.serve_with_shutdown(async move { shutdown.cancelled().await }));
    for token in [None, Some("wrong-generation"), Some("previous-generation")] {
        for action in ["session.create", "session.send"] {
            let request = reqwest::Client::new().post(format!("http://{address}/mcp")).json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"execute","arguments":{"action":action}}}));
            let request = if let Some(token) = token {
                request.bearer_auth(token)
            } else {
                request
            };
            assert_eq!(
                request.send().await.unwrap().status(),
                reqwest::StatusCode::UNAUTHORIZED
            );
        }
    }
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut rpc = p::routine_service_client::RoutineServiceClient::new(channel);
    let header = Some(("authorization", "Bearer current-local-generation"));
    let mut observations = rpc
        .watch_invocations(request(
            p::WatchInvocationsRequest {
                limit: 100,
                ..Default::default()
            },
            header,
        ))
        .await
        .unwrap()
        .into_inner();
    let created = mcp_origin::call(address, "current-local-generation", "create", serde_json::json!({"action":"session.create","input":{"agent_name":"routine-title","prompt":"Create through the embedded MCP origin"}})).await;
    let id = created["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("created MCP session");
    let mut seen = Vec::new();
    let observed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut completed = BTreeSet::new();
        while let Some(event) = observations.message().await.unwrap() {
            for row in event.invocations {
                seen.push((row.routine.clone(), row.status));
                if row.status == p::InvocationStatus::Succeeded as i32 {
                    let detail = rpc
                        .get_invocation(request(
                            p::GetInvocationRequest {
                                invocation_id: row.id,
                            },
                            header,
                        ))
                        .await
                        .unwrap()
                        .into_inner()
                        .invocation
                        .unwrap();
                    completed.insert(detail.routine);
                }
            }
            if completed.contains("compact-and-continue") && completed.contains("title") {
                return;
            }
        }
        panic!("production lifecycle observation closed");
    })
    .await;
    assert!(observed.is_ok(), "lifecycle observations: {seen:?}");
    drop(observations);
    let sent = mcp_origin::call(address, "current-local-generation", "execute", serde_json::json!({"action":"session.send","input":{"id":id,"prompt":"Send through the same local generation"}})).await;
    assert!(sent.get("result").is_some(), "{sent:?}");
    let code = mcp_origin::call(
        address,
        "current-local-generation",
        "code",
        serde_json::json!({"action":"execute","source":"return 1"}),
    )
    .await;
    assert!(code.get("error").is_some());
    let block = mcp_origin::call(address, "current-local-generation", "create", serde_json::json!({"action":"session.create","input":{"agent_name":"routine-title","prompt":"LOCAL_GENERATION_BLOCK"}})).await;
    assert!(block.get("result").is_some(), "{block:?}");
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert!(live.load(Ordering::SeqCst) > 0);
    stop.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(
        live.load(Ordering::SeqCst),
        0,
        "generation shutdown joins original streams"
    );
    std::fs::remove_dir_all(root).unwrap();
}
