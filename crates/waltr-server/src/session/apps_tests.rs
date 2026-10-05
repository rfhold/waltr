const TEST_APP_HTML: &str = "<!doctype html><html><body><p>Generic fixture app</p></body></html>";
use super::super::mcp_apps as apps;
use super::*;
use waltr::grpc::waltr::session::v1::{session_service_server::SessionService as _, *};
use waltr_agent::{
    AgentRuntimeProvider, AgentSessionRuntimeFactory, BoxCompletionStream, BoxStreamFuture,
    CompletionEvent, CompletionRequest, LlmStream, MemorySessionStore, SessionCoordinator,
    SessionCreateRequest, SessionId,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tonic::{Code, Request};

struct Provider(AtomicUsize);

#[cfg(feature = "apps-served-assets")]
struct SelfStatusProvider(AtomicUsize);

#[cfg(feature = "apps-served-assets")]
impl LlmStream for SelfStatusProvider {
    fn name(&self) -> &'static str {
        "embedded-status-test"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        assert!(
            request
                .tools
                .iter()
                .any(|tool| tool.name == "mcp_waltr_status")
        );
        let turn = self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let events = if turn == 0 {
                vec![CompletionEvent::ToolCall(waltr_agent::ToolCall::new(
                    "self-status",
                    "mcp_waltr_status",
                    json!({}),
                ))]
            } else {
                vec![CompletionEvent::Finished]
            };
            Ok(
                Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)))
                    as BoxCompletionStream,
            )
        })
    }
}

#[cfg(feature = "apps-served-assets")]
#[tokio::test]
async fn embedded_self_status_records_app_association_from_local_credential_snapshot() {
    use crate::embedded::{
        EmbeddedCatalogSource, EmbeddedOutboundPolicy, EmbeddedServerConfig, build_embedded_server,
    };
    use waltr_agent::{AuthTarget, CredentialStore, FileCredentialStore};

    let root = crate::test_support::temp_root();
    let bearer = crate::LocalBearer::new("embedded-status-fixture-secret").unwrap();
    let server = build_embedded_server(EmbeddedServerConfig {
        resources_root: root.join("resources"),
        waltr_database_path: root.join("data/waltr.sqlite3"),
        mcp_database_path: root.join("data/mcp.sqlite3"),
        session_root: root.join("embedded-sessions"),
        workspace_root: root.join("embedded-workspace"),
        catalog_source: EmbeddedCatalogSource::Path(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/models-dev-defaults.json"),
        ),
        catalog_cache_path: root.join("cache/models.json"),
        catalog_fallback: None,
        default_agent: "coding".into(),
        local_bearer: bearer.clone(),
        outbound_policy: EmbeddedOutboundPolicy::default(),
    })
    .await
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let endpoint = format!("{origin}/mcp");
    let stop = tokio_util::sync::CancellationToken::new();
    let shutdown = stop.clone();
    let task = tokio::spawn(server.serve_with_shutdown(shutdown.cancelled_owned()));
    let credentials = CredentialStore::new(crate::local_admission::LocalCredentialStore::new(
        Arc::new(FileCredentialStore::new(
            root.join("fixture-credentials.json"),
        )),
        &bearer,
        &endpoint,
        None,
    ));
    let key = AuthTarget::mcp_server("waltr").credential_key();
    let before = credentials.read(&key).await.unwrap().unwrap();
    assert!(
        before
            .credential
            .authority_id
            .as_ref()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(before.credential.mcp_context().unwrap().resource, endpoint);

    tokio::fs::create_dir(root.join("fixture-workspace"))
        .await
        .unwrap();
    let sandbox = crate::sandbox::LocalSandbox::start(root.join("fixture-workspace"))
        .await
        .unwrap();
    let (definition, _) = crate::test_support::default_agent_definition();
    let provider = Arc::new(SelfStatusProvider(AtomicUsize::new(0)));
    let provider_factory = provider.clone();
    let factory = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.join("fixture-workspace"),
        credentials.clone(),
        definition,
    )
    .with_runtime_provider(move |_| {
        Ok(AgentRuntimeProvider::new(
            waltr_agent::ModelRef::new(
                waltr_agent::ProviderId::OpenAi,
                "gpt-5-nano",
                waltr_agent::ModelApi::OpenAiResponses,
            ),
            provider_factory.clone(),
        ))
    })
    .with_trusted_mcp_oauth_origins(vec![origin.parse().unwrap()])
    .with_mcp_connection_resolver(move |_| {
        let endpoint = endpoint.clone();
        Box::pin(async move {
            Ok(vec![waltr_agent::McpResolvedConnection {
                key: "waltr".into(),
                kind: waltr_agent::McpConnectionKind::StreamableHttp,
                url: endpoint,
                command: String::new(),
            }])
        })
    });
    let sessions = SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory);
    let id = SessionId::generate();
    sessions
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    sessions
        .set_inference_settings(
            &id,
            Some(waltr_agent::InferenceSettings {
                provider: waltr_agent::InferenceProviderSettings {
                    key: "fixture".into(),
                    api: waltr_agent::ModelApi::OpenAiResponses,
                    url: String::new(),
                    model_id: "gpt-5-nano".into(),
                    model_cost: None,
                    headers: Default::default(),
                    oauth: None,
                    reasoning_effort: None,
                },
            }),
        )
        .await
        .unwrap();
    let run = sessions
        .submit_prompt(
            id.clone(),
            "Read Waltr status",
            waltr_agent::RunConflictPolicy::RejectIfActive,
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), run.handle.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(provider.0.load(Ordering::SeqCst), 2);
    let after = credentials.read(&key).await.unwrap().unwrap();
    assert_eq!(before, after);
    let record = sessions.load(&id).await.unwrap();
    let result = record.record.timeline_events().find(|event| matches!(&event.content, waltr_agent::TimelineContent::ToolResult { result } if result.call_id == "self-status")).unwrap();
    let association = &result.metadata["mcp_app"];
    assert_eq!(association["version"], 1);
    assert_eq!(association["server"], "waltr");
    assert_eq!(association["tool"], "status");
    assert_eq!(association["resource_uri"], "ui://waltr/status");
    assert_eq!(
        association["declaration"]["_meta"]["ui"]["resourceUri"],
        "ui://waltr/status"
    );
    use sha2::{Digest as _, Sha256};
    let identity = format!(
        "http:{:?}:authorized:{}",
        before.credential.mcp_context().unwrap().resource,
        before.credential.authority_id.as_ref().unwrap()
    );
    let source = Sha256::digest(identity)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(association["source"], source);
    assert_eq!(association["arguments"], json!({}));
    assert!(association["result"]["structuredContent"].is_object());
    let projected =
        super::super::timeline_convert::timeline_event_to_proto(result.clone()).unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&projected.metadata_json).unwrap();
    assert_eq!(metadata["mcp_app"], *association);
    let app = sessions
        .open_session_mcp_app(
            &id,
            "self-status",
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(app.resource.html.contains("Waltr Status"));
    assert_eq!(app.association.result, association["result"]);
    sessions.close_session_mcp_app(&app.handle);
    sessions.shutdown().await;
    sandbox.shutdown().await;
    stop.cancel();
    task.await.unwrap().unwrap();
    assert!(!root.join("fixture-credentials.json").exists());
}

impl LlmStream for Provider {
    fn name(&self) -> &'static str {
        "apps-test"
    }
    fn stream_request<'a>(&'a self, _: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let turn = self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let events = if turn == 0 {
                vec![CompletionEvent::ToolCall(waltr_agent::ToolCall::new(
                    "original",
                    "mcp_fixture_observe_fixture",
                    json!({}),
                ))]
            } else {
                vec![CompletionEvent::Finished]
            };
            Ok(
                Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)))
                    as BoxCompletionStream,
            )
        })
    }
}

#[derive(Clone)]
struct Fixture(Arc<tokio::sync::Notify>);
impl mcp::server::ServerHandler for Fixture {
    fn server_info(&self) -> mcp::server::ServerInfo {
        mcp::server::ServerInfo::new("apps-fixture", "1")
    }
    fn capabilities(&self) -> mcp::server::ServerCapabilities {
        mcp::server::ServerCapabilities::new().tools().resources()
    }
    fn list_tools(
        &self,
        _: Option<String>,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpToolList>> {
        Box::pin(async {
            Ok(mcp::McpToolList { tools: vec![mcp::parse_tool_definition(&json!({"name":"observe_fixture","inputSchema":{"type":"object"},"_meta":{"ui":{"resourceUri":"ui://fixtures/observation"}}})).unwrap()], next_cursor: None })
        })
    }
    fn call_tool(
        &self,
        call: mcp::McpToolCall,
        context: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpToolResult>> {
        let started = self.0.clone();
        Box::pin(async move {
            if call.arguments["wait"] == true {
                started.notify_one();
                context.cancelled().await;
                return Err(mcp::server::ServerError::internal("fixture canceled"));
            }
            Ok(mcp::McpToolResult::new(
                json!({"content":[{"type":"text","text":"ready"}],"structuredContent":{"ready":true,"observedAtUnixMs":1}}),
            ))
        })
    }
    fn read_resource(
        &self,
        uri: String,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpResourceResult>> {
        Box::pin(async move {
            Ok(mcp::McpResourceResult::text(
                uri,
                mcp::apps::UI_MIME_TYPE,
                TEST_APP_HTML,
            ))
        })
    }
    fn list_resources(
        &self,
        _: Option<String>,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpResourceList>> {
        Box::pin(async {
            Ok(mcp::McpResourceList {
                resources: vec![],
                next_cursor: None,
            })
        })
    }
    fn list_resource_templates(
        &self,
        _: Option<String>,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpResourceTemplateList>> {
        Box::pin(async {
            Ok(mcp::McpResourceTemplateList {
                resource_templates: vec![],
                next_cursor: None,
            })
        })
    }
}

fn definition(level: &str) -> String {
    format!(
        "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: apps }}\nspec:\n  features: [{{ featureSetRef: {{ name: apps }} }}]\n  permissions: [{{ permissionSetRef: {{ name: apps }} }}]\n---\napiVersion: waltr.dev/v1alpha1\nkind: FeatureSet\nmetadata: {{ name: apps }}\nspec:\n  mcp: {{ exposeTools: true, defaultPermissions: false }}\n---\napiVersion: waltr.dev/v1alpha1\nkind: PermissionSet\nmetadata: {{ name: apps }}\nspec:\n  tools: [{{ name: mcp_fixture_observe_fixture, permission: {level} }}]\n"
    )
}

fn request<T>(value: T, subject: &str) -> Request<T> {
    let mut request = Request::new(value);
    request
        .extensions_mut()
        .insert(crate::access_auth::AuthenticatedPrincipal::apps_test_principal(subject));
    request
}

#[tokio::test]
async fn apps_rpc_owns_handles_preserves_snapshot_no_inference_and_fails_closed() {
    let root = crate::test_support::temp_root();
    let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let endpoint = format!("{origin}/mcp");
    let cancel = tokio_util::sync::CancellationToken::new();
    let server_cancel = cancel.clone();
    let started = Arc::new(tokio::sync::Notify::new());
    let fixture_started = started.clone();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            mcp::server::streamable_http_router(Arc::new(Fixture(fixture_started))),
        )
        .with_graceful_shutdown(server_cancel.cancelled_owned())
        .await
        .unwrap();
    });
    let source = definition("allow");
    let mut registry = waltr_agent::AgentDefinitionRegistry::new();
    registry.load_yaml(&source).unwrap();
    let resolved = registry.resolve_agent("apps").unwrap();
    let provider = Arc::new(Provider(AtomicUsize::new(0)));
    let provider_factory = provider.clone();
    let factory = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.clone(),
        waltr_agent::FileCredentialStore::new(root.join("credentials.json")),
        resolved,
    )
    .with_runtime_provider(move |_| {
        Ok(AgentRuntimeProvider::new(
            waltr_agent::ModelRef::new(
                waltr_agent::ProviderId::OpenAi,
                "gpt-5-nano",
                waltr_agent::ModelApi::OpenAiResponses,
            ),
            provider_factory.clone(),
        ))
    })
    .with_trusted_mcp_oauth_origins(vec![origin.parse().unwrap()])
    .with_mcp_connection_resolver(move |_| {
        let endpoint = endpoint.clone();
        Box::pin(async move {
            Ok(vec![waltr_agent::McpResolvedConnection {
                key: "fixture".into(),
                kind: waltr_agent::McpConnectionKind::StreamableHttp,
                url: endpoint,
                command: String::new(),
            }])
        })
    });
    let sessions = SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory);
    let id = SessionId::generate();
    sessions
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    sessions
        .set_inference_settings(
            &id,
            Some(waltr_agent::InferenceSettings {
                provider: waltr_agent::InferenceProviderSettings {
                    key: "test".into(),
                    api: waltr_agent::ModelApi::OpenAiResponses,
                    url: String::new(),
                    model_id: "gpt-5-nano".into(),
                    model_cost: None,
                    headers: Default::default(),
                    oauth: None,
                    reasoning_effort: None,
                },
            }),
        )
        .await
        .unwrap();
    let run = sessions
        .submit_prompt(
            id.clone(),
            "observe",
            waltr_agent::RunConflictPolicy::RejectIfActive,
        )
        .await
        .unwrap();
    run.handle.wait().await.unwrap();
    let state = crate::test_support::resource_state_with_overrides(&[]);
    let service = Arc::new(SessionService {
        sessions: sessions.clone(),
        config: crate::config::ConfigHandle::from_resources(state),
        agent_catalog: SessionCatalogSource::Static(crate::test_support::default_agent_catalog()),
        apps: apps::Registry::new(),
    });
    let open = || OpenSessionMcpAppRequest {
        session_id: id.as_str().to_owned(),
        call_id: "original".into(),
    };
    let app = service
        .open_session_mcp_app(request(open(), "alice"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(app.html, TEST_APP_HTML);
    let tool: serde_json::Value = serde_json::from_str(&app.tool_json).unwrap();
    assert_eq!(tool["name"], "observe_fixture");
    assert!(tool.get("source").is_none());
    let call = || CallSessionMcpAppToolRequest {
        app_id: app.app_id.clone(),
        name: "observe_fixture".into(),
        arguments_json: "{}".into(),
    };
    let result = service
        .call_session_mcp_app_tool(request(call(), "alice"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result.result_json).unwrap()["structuredContent"]
            ["ready"],
        true
    );
    assert_eq!(provider.0.load(Ordering::SeqCst), 2);
    let mut waiting = call();
    waiting.arguments_json = "{\"wait\":true}".into();
    let running_service = service.clone();
    let running = tokio::spawn(async move {
        running_service
            .call_session_mcp_app_tool(request(waiting, "alice"))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    service
        .call_session_mcp_app_tool(request(call(), "alice"))
        .await
        .unwrap();
    let mut waiting = call();
    waiting.arguments_json = "{\"wait\":true}".into();
    let running_service = service.clone();
    let timed = tokio::spawn(async move {
        running_service
            .call_session_mcp_app_tool(request(waiting, "alice"))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    let timeout_status = timed.await.unwrap().unwrap_err();
    assert!(matches!(
        timeout_status.code(),
        Code::DeadlineExceeded | Code::Unavailable
    ));
    tokio::time::resume();
    service
        .call_session_mcp_app_tool(request(call(), "alice"))
        .await
        .unwrap();
    let record = sessions.load(&id).await.unwrap();
    let original = record.record.timeline_events().find(|event| matches!(&event.content, waltr_agent::TimelineContent::ToolResult { result } if result.call_id == "original")).unwrap();
    assert_eq!(
        original.metadata["mcp_app"]["result"],
        serde_json::from_str::<serde_json::Value>(&app.result_json).unwrap()
    );
    assert_eq!(
        service
            .call_session_mcp_app_tool(request(call(), "bob"))
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    assert_eq!(
        service
            .call_session_mcp_app_tool(Request::new(call()))
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    for (level, message) in [
        ("ask", "MCP app approval required"),
        ("deny", "MCP app denied"),
    ] {
        sessions
            .set_agent_definition(
                &id,
                Some(waltr_agent::AgentDefinitionBundle::new(
                    "apps",
                    vec![definition(level)],
                )),
            )
            .await
            .unwrap();
        assert_eq!(
            service
                .call_session_mcp_app_tool(request(call(), "alice"))
                .await
                .unwrap_err()
                .message(),
            message
        );
    }
    sessions
        .set_agent_definition(
            &id,
            Some(waltr_agent::AgentDefinitionBundle::new("apps", vec![source])),
        )
        .await
        .unwrap();
    let pause = sessions
        .acquire_pause_reject_busy(
            &id,
            waltr_agent::SessionCoordinationReason::new("apps-test", None, None).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        service
            .open_session_mcp_app(request(open(), "alice"))
            .await
            .unwrap_err()
            .message(),
        "MCP app busy"
    );
    sessions.release_pause(&pause).await.unwrap();
    service
        .close_session_mcp_app(request(
            CloseSessionMcpAppRequest { app_id: app.app_id },
            "alice",
        ))
        .await
        .unwrap();
    let local = service
        .open_session_mcp_app(Request::new(open()))
        .await
        .unwrap()
        .into_inner();
    let local_call = || CallSessionMcpAppToolRequest {
        app_id: local.app_id.clone(),
        name: "observe_fixture".into(),
        arguments_json: "{}".into(),
    };
    service
        .call_session_mcp_app_tool(Request::new(local_call()))
        .await
        .unwrap();
    assert_eq!(
        service
            .call_session_mcp_app_tool(request(local_call(), "alice"))
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(900)).await;
    assert_eq!(
        service
            .call_session_mcp_app_tool(Request::new(local_call()))
            .await
            .unwrap_err()
            .message(),
        "MCP app stale"
    );
    tokio::time::resume();
    assert_eq!(
        service
            .open_session_mcp_app(request(
                OpenSessionMcpAppRequest {
                    session_id: id.as_str().into(),
                    call_id: "unrecorded".into()
                },
                "alice"
            ))
            .await
            .unwrap_err()
            .message(),
        "MCP app stale"
    );
    let mut leases = Vec::new();
    for _ in 0..32 {
        leases.push(
            service
                .open_session_mcp_app(request(open(), "alice"))
                .await
                .unwrap()
                .into_inner()
                .app_id,
        );
    }
    assert_eq!(
        service
            .open_session_mcp_app(request(open(), "alice"))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    sessions.delete(&id).await.unwrap();
    let app_id = leases.pop().unwrap();
    let close = || CloseSessionMcpAppRequest {
        app_id: app_id.clone(),
    };
    let owner = apps::owner(&request(close(), "alice"));
    let cancellation = service.apps.get(&app_id, &owner).unwrap().cancellation;
    for subject in [Some("bob"), None] {
        let denied = service
            .close_session_mcp_app(match subject {
                Some(subject) => request(close(), subject),
                None => Request::new(close()),
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code(), Code::FailedPrecondition);
        assert_eq!(denied.message(), "MCP app stale");
        assert!(service.apps.get(&app_id, &owner).is_ok());
        assert!(!cancellation.is_cancelled());
    }
    assert_eq!(
        service
            .call_session_mcp_app_tool(request(
                CallSessionMcpAppToolRequest {
                    app_id: app_id.clone(),
                    name: "observe_fixture".into(),
                    arguments_json: "{}".into(),
                },
                "alice",
            ))
            .await
            .unwrap_err()
            .message(),
        "MCP app stale"
    );
    assert_eq!(
        service
            .open_session_mcp_app(request(open(), "alice"))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    service
        .close_session_mcp_app(request(close(), "alice"))
        .await
        .unwrap();
    assert!(cancellation.is_cancelled());
    assert_eq!(
        service.apps.get(&app_id, &owner).unwrap_err().message(),
        "MCP app stale"
    );
    assert_eq!(
        service
            .close_session_mcp_app(request(close(), "alice"))
            .await
            .unwrap_err()
            .message(),
        "MCP app stale"
    );
    // Open now gets past the 32-slot limit, but still rejects the deleted session.
    assert_eq!(
        service
            .open_session_mcp_app(request(open(), "alice"))
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
    for app_id in leases {
        service
            .close_session_mcp_app(request(CloseSessionMcpAppRequest { app_id }, "alice"))
            .await
            .unwrap();
    }
    service.apps.clear();
    sessions.shutdown().await;
    sandbox.shutdown().await;
    cancel.cancel();
    task.await.unwrap();
}
