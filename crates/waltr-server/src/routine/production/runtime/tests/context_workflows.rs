use super::*;
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use waltr_agent::{
    AgentRuntime, BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionRequest,
    LlmStream, MemorySessionStore, SessionCoordinator, SessionCreateRequest, SessionId, ToolCall,
};
#[path = "context_authority.rs"]
mod context_authority;

async fn workflow_checkpoint(sessions: &SessionCoordinator<MemorySessionStore>, id: &SessionId) -> Option<Value> {
    let record = sessions.load(id).await.ok()?.record;
    let checkpoint = record.metadata.values.get("routine.categories.v1")?.clone();
    let capture = sessions.capture_context(id, "workflow-test", waltr_agent::SessionCaptureLimits::default()).await.ok()?;
    if checkpoint["version"] != 1 || checkpoint["through"] != capture.identity.boundary().get().to_string() { return None; }
    let original = waltr_agent::SessionCopy::from_record(&record, capture.identity.boundary()).ok()?;
    let categories = checkpoint["categories"].as_array()?;
    for item in original.timeline() {
        let values = record.item_metadata_values(&item.item_id).ok()??;
        let key = values.get("routine.category.v1")?;
        if !categories.iter().any(|category| &category["key"] == key) { return None; }
    }
    Some(checkpoint)
}

#[derive(Default)]
struct Provider {
    category: AtomicUsize,
    mask: AtomicUsize,
    configuration: AtomicUsize,
    management: AtomicUsize,
    configure_tool: AtomicUsize,
    ceiling_denied: AtomicUsize,
    semantic_turns: AtomicUsize,
    task_gate: TaskGate,
    callbacks: AtomicUsize,
    callback_entered: tokio::sync::Notify,
    callback_release: TaskGate,
    category_armed: std::sync::atomic::AtomicBool,
    category_entered: tokio::sync::Notify,
    category_release: TaskGate,
    parent_entered: tokio::sync::Notify,
    parent_release: TaskGate,
}
struct TaskGate {
    release: Arc<tokio::sync::Semaphore>,
}
impl Default for TaskGate {
    fn default() -> Self {
        Self {
            release: Arc::new(tokio::sync::Semaphore::new(0)),
        }
    }
}
impl LlmStream for Provider {
    fn name(&self) -> &'static str {
        "production-context-workflows"
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let system = request.system_prompt.unwrap_or_default();
            if request.prompt.starts_with("Async task completed.\n") {
                self.callbacks.fetch_add(1, Ordering::SeqCst);
                self.callback_entered.notify_one();
                let release = self.callback_release.release.clone();
                return Ok(Box::pin(
                    futures_util::stream::once(async move {
                        release.acquire().await.unwrap().forget();
                        Ok(CompletionEvent::Delta("actual callback result".into()))
                    })
                    .chain(futures_util::stream::iter([Ok(CompletionEvent::Finished)])),
                ) as BoxCompletionStream);
            }
            if request.prompt == "ordinary delegated task" {
                let release = self.task_gate.release.clone();
                return Ok(Box::pin(
                    futures_util::stream::once(async move {
                        release.acquire().await.unwrap().forget();
                        Ok(CompletionEvent::Delta("actual delegated result".into()))
                    })
                    .chain(futures_util::stream::iter([Ok(CompletionEvent::Finished)])),
                ) as BoxCompletionStream);
            }
            if request.prompt == "RUN_TURN_BATCH" {
                let turn = self.semantic_turns.fetch_add(1, Ordering::SeqCst);
                if turn < 19 {
                    return Ok(Box::pin(futures_util::stream::iter(vec![
                        Ok(CompletionEvent::ToolCall(ToolCall::new(
                            format!("actual-batch-{turn}"),
                            "configure-waltr",
                            json!({"prompt":"Explain without changes"}),
                        ))),
                        Ok(CompletionEvent::Finished),
                    ])) as BoxCompletionStream);
                }
            }
            if request.prompt == "RUN_TASK_BATCH"
                && !format!("{:?}", request.turns).contains("actual-delegate-1")
            {
                assert!(request.tools.iter().any(|tool| tool.name == "task"));
                return Ok(Box::pin(futures_util::stream::iter(vec![
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "actual-delegate-0",
                        "task",
                        json!({"mode":"async","prompt":"ordinary delegated task"}),
                    ))),
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "actual-delegate-1",
                        "task",
                        json!({"mode":"async","prompt":"ordinary delegated task"}),
                    ))),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream);
            }
            if request.prompt == "RUN_TASK_BATCH" {
                self.parent_entered.notify_one();
                let release = self.parent_release.release.clone();
                return Ok(Box::pin(
                    futures_util::stream::once(async move {
                        release.acquire().await.unwrap().forget();
                        Ok(CompletionEvent::Delta("actual parent result".into()))
                    })
                    .chain(futures_util::stream::iter([Ok(CompletionEvent::Finished)])),
                ) as BoxCompletionStream);
            }
            if request.prompt == "RUN_CONFIGURATION_TOOL"
                && self.configure_tool.fetch_add(1, Ordering::SeqCst) == 0
            {
                assert!(
                    request
                        .tools
                        .iter()
                        .any(|tool| tool.name == "configure-waltr")
                );
                return Ok(Box::pin(futures_util::stream::iter(vec![
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "normal-configuration",
                        "configure-waltr",
                        json!({"prompt":"Explain the current configuration without changes"}),
                    ))),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream);
            }
            if system.contains("You are the Waltr configuration agent") {
                self.configuration.fetch_add(1, Ordering::SeqCst);
                let history = format!("{:?}", request.turns);
                if !history.contains("ceiling-probe") {
                    return Ok(Box::pin(futures_util::stream::iter(vec![
                        Ok(CompletionEvent::ToolCall(ToolCall::new(
                            "ceiling-probe",
                            "load_skill",
                            json!({"source":"mcp","server":"waltr","uri":"skill://resource-administration/SKILL.md"}),
                        ))),
                        Ok(CompletionEvent::Finished),
                    ])) as BoxCompletionStream);
                }
                let refusal = request
                    .turns
                    .iter()
                    .flat_map(|turn| &turn.items)
                    .find_map(|item| match item {
                        waltr_agent::TurnItem::ToolResult(result)
                            if result.call_id == "ceiling-probe" =>
                        {
                            Some(result)
                        }
                        _ => None,
                    })
                    .expect("the exact denied Skill activation result");
                assert!(refusal.is_error, "{refusal:?}");
                assert!(
                    refusal.text_projection().contains("permission denied"),
                    "{refusal:?}"
                );
                self.ceiling_denied.fetch_add(1, Ordering::SeqCst);
            }
            if matches!(
                request.prompt,
                "RUN_CONTEXT_TOOL" | "RUN_FRESH_CONTEXT_TOOL"
            ) && !format!("{:?}", request.turns).contains("normal-management")
            {
                self.management.fetch_add(1, Ordering::SeqCst);
                assert!(
                    request
                        .tools
                        .iter()
                        .any(|tool| tool.name == "manage-context")
                );
                return Ok(Box::pin(futures_util::stream::iter(vec![
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "normal-management",
                        "manage-context",
                        json!({"instruction":"Summarize work"}),
                    ))),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream);
            }
            let captured = format!("{:?}", request.turns).contains("_helper_capture");
            let category_blocked = system.contains("Organize the copied session items")
                && !captured
                && self.category_armed.swap(false, Ordering::SeqCst);
            let value = if system.contains("Organize the copied session items") && !captured {
                self.category.fetch_add(1, Ordering::SeqCst);
                let input: Value = serde_json::from_str(request.prompt).unwrap();
                let ids = input["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["item_id"].as_str().unwrap())
                    .collect::<std::collections::BTreeSet<_>>();
                Some(
                    json!({"categories":[{"key":"work","description":"Preserved work"}],"assignments":ids.into_iter().map(|id| json!({"item_id":id,"category":"work"})).collect::<Vec<_>>(),"note":"actual production category"}),
                )
            } else if system.contains("Decide context management") && !captured {
                Some(
                    json!({"kind":"edit","edits":[{"key":"work","instruction":"Preserve details"}]}),
                )
            } else if system.contains("Produce an accurate context summary") && !captured {
                self.mask.fetch_add(1, Ordering::SeqCst);
                let input: Value = serde_json::from_str(request.prompt).unwrap();
                Some(if input["selected"]["activeMaskId"].is_null() {
                    json!({"kind":"replace","summary":"Actual production category summary"})
                } else {
                    json!({"kind":"keep"})
                })
            } else {
                None
            };
            let event = value.map_or_else(
                || CompletionEvent::Delta("normal production response".into()),
                |value| {
                    CompletionEvent::ToolCall(ToolCall::new(
                        "actual-capture",
                        "_helper_capture",
                        value,
                    ))
                },
            );
            if category_blocked {
                self.category_entered.notify_one();
                let release = self.category_release.release.clone();
                return Ok(Box::pin(
                    futures_util::stream::once(async move {
                        release.acquire().await.unwrap().forget();
                        Ok(event)
                    })
                    .chain(futures_util::stream::iter([Ok(CompletionEvent::Finished)])),
                ) as BoxCompletionStream);
            }
            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(event),
                Ok(CompletionEvent::Finished),
            ])) as BoxCompletionStream)
        })
    }
}

#[tokio::test]
async fn unchanged_context_programs_commit_categories_replace_keep_and_deny_global_configuration() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::context_workflows::unchanged_context_programs_commit_categories_replace_keep_and_deny_global_configuration",
    ) {
        return;
    }
    run_context_workflows(false).await;
}

#[tokio::test]
async fn delegated_task_callbacks_reject_stale_commit_then_refresh_idle_coverage() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::context_workflows::delegated_task_callbacks_reject_stale_commit_then_refresh_idle_coverage",
    ) {
        return;
    }
    run_context_workflows(true).await;
}

async fn run_context_workflows(race: bool) {
    let root = crate::test_support::temp_root();
    let resources = root.join("resources");
    seed(&resources);
    let agents = resources.join(".waltr/resources/agents");
    std::fs::create_dir_all(&agents).unwrap();
    for (name, permissions) in [
        (
            "context-source",
            "[{name: _helper_capture, permission: allow}, {name: manage-context, permission: allow}, {name: configure-waltr, permission: allow}, {name: task, permission: allow}]",
        ),
        ("denied-source", "[]"),
    ] {
        std::fs::write(agents.join(format!("{name}.yaml")), format!("apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{name: {name}}}\nspec:\n  systemPrompt: SOURCE\n  features: [{{task: {{maxSubagentDepth: 1, defaultPermissions: false}}}}]\n  permissions: {permissions}\n")).unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mcp = resources.join(".waltr/resources/mcp-servers");
    std::fs::create_dir_all(&mcp).unwrap();
    std::fs::write(mcp.join("waltr.yaml"), format!("apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {{name: waltr}}\nspec: {{transport: streamableHttp, url: 'http://{address}/mcp'}}\n")).unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let loader = crate::test_support::default_catalog_loader();
    let config = ConfigService::new(resources, loader.clone()).await.unwrap();
    let state = config.state().clone();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let provider = Arc::new(Provider::default());
    let inference = bindings::default_inference(&state.snapshot()).unwrap();
    for name in [
        "context-source",
        "denied-source",
        "idle-source",
        "turn-source",
        "task-source",
        "fresh-management-source",
    ] {
        let mut create = SessionCreateRequest::new(SessionId::new(name));
        create.agent_definition = state
            .snapshot()
            .catalog()
            .bundle(if name == "denied-source" {
                name
            } else {
                "context-source"
            })
            .map(|bundle| serde_json::to_value(bundle).unwrap());
        create.inference_settings = Some(inference.clone());
        sessions.create(create).await.unwrap();
        let runtime = AgentRuntime::builder(bindings::model(&inference), provider.clone()).build();
        let events = sessions
            .prompt(
                &runtime,
                SessionId::new(name),
                "Original production context",
            )
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok));
    }
    let source = sessions.clone();
    let supplied = provider.clone();
    let persistence = Persistence::initialize(
        crate::persistence::DatabaseConfig::from_urls(
            &format!("sqlite://{}", root.join("waltr.sqlite3").display()),
            &format!("sqlite://{}", root.join("mcp.sqlite3").display()),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let app = assemble_app(AppServices {
        config_service: config, persistence, catalog_loader: loader,
        access_authenticator: crate::access_auth::AccessAuthenticator::disabled(), mcp_oauth: None,
        outbound_private_origins: Vec::new(), outbound_oauth: OutboundOAuthConfig::default(), automatic_self_mcp_resource: None,
        agent_state: Some(state), local_admission: None, workspace_root: workspace, sandbox_socket: None,
        routine_authority: RoutineAuthority::default(), routine_private_parent: root,
    }, move |routes, input| {
        let service = Arc::try_unwrap(crate::session::SessionService::from_sessions(source, input)).ok().unwrap().with_test_provider(supplied);
        let service = Arc::new(service);
        routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));
        Ok(service)
    }).await.unwrap();
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let router = app.router().layer(axum::Extension(
        crate::local_admission::ValidatedLocalOrigin(Arc::new(CancellationToken::new())),
    ));
    let task = tokio::spawn(async move {
        let result = crate::http_lifecycle::serve_owned_connections(
            listener,
            router,
            shutdown.cancelled_owned(),
        )
        .await;
        app.shutdown().await;
        result
    });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut session_client =
        waltr::grpc::waltr::session::v1::session_service_client::SessionServiceClient::new(
            channel.clone(),
        );
    let mut client = p::routine_service_client::RoutineServiceClient::new(channel);
    let definitions = client
        .watch_definitions(p::WatchDefinitionsRequest {})
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap()
        .unwrap()
        .definitions;
    let configuration = definitions
        .iter()
        .find(|definition| definition.name == "configuration")
        .unwrap();
    assert!(
        configuration
            .entrypoints
            .iter()
            .filter(|entry| entry.trigger == p::Trigger::Manual as i32)
            .all(|entry| !entry
                .start_contexts
                .contains(&(p::StartContext::Global as i32)))
    );
    for source in [None, Some("denied-source")] {
        let denied = client
            .start_invocation(p::StartInvocationRequest {
                selection: Some(p::InvocationSelection {
                    routine: "configuration".into(),
                    revision: configuration.revision.clone(),
                    entrypoint: "manual".into(),
                    source_session: source.map(str::to_owned),
                    correlation: "denied".into(),
                    parameters: Some(p::StructuredValue {
                        json: serde_json::to_vec(&json!({"prompt":"No authority"})).unwrap(),
                    }),
                }),
            })
            .await;
        assert!(denied.is_err());
    }
    assert_eq!(provider.configuration.load(Ordering::SeqCst), 0);
    for (name, parameters) in [
        ("context-categorization", Value::Null),
        (
            "manage-category-context",
            json!({"key":"work","instruction":"No capture authority"}),
        ),
    ] {
        let definition = definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap();
        let denied = client
            .start_invocation(p::StartInvocationRequest {
                selection: Some(p::InvocationSelection {
                    routine: name.into(),
                    revision: definition.revision.clone(),
                    entrypoint: "run".into(),
                    source_session: Some("denied-source".into()),
                    correlation: "capture-denied".into(),
                    parameters: Some(p::StructuredValue {
                        json: serde_json::to_vec(&parameters).unwrap(),
                    }),
                }),
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code(), tonic::Code::PermissionDenied);
    }
    assert_eq!(provider.category.load(Ordering::SeqCst), 0);
    assert_eq!(provider.mask.load(Ordering::SeqCst), 0);
    assert!(
        client
            .list_invocations(p::ListInvocationsRequest {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner()
            .invocations
            .is_empty()
    );
    for (name, entry, input) in [
        ("context-categorization", "run", Value::Null),
        (
            "context-management",
            "session-tool",
            json!({"instruction":"Summarize work"}),
        ),
        (
            "manage-category-context",
            "run",
            json!({"key":"work","instruction":"Keep exact summary"}),
        ),
    ] {
        if name == "context-management" {
            let mut watch = client
                .watch_invocations(p::WatchInvocationsRequest {
                    limit: 100,
                    ..Default::default()
                })
                .await
                .unwrap()
                .into_inner();
            session_client
                .submit_user_message(waltr::grpc::waltr::session::v1::SubmitUserMessageRequest {
                    session_id: "context-source".into(),
                    text: "RUN_CONTEXT_TOOL".into(),
                })
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    for row in watch.message().await.unwrap().unwrap().invocations {
                        if row.status >= p::InvocationStatus::Succeeded as i32 {
                            let detail = client
                                .get_invocation(p::GetInvocationRequest {
                                    invocation_id: row.id,
                                })
                                .await
                                .unwrap()
                                .into_inner()
                                .invocation
                                .unwrap();
                            if detail.routine == name {
                                assert_eq!(
                                    detail.status,
                                    p::InvocationStatus::Succeeded as i32,
                                    "{detail:?}"
                                );
                                return;
                            }
                        }
                    }
                }
            })
            .await
            .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let id = SessionId::new("context-source");
                    let checkpoint = workflow_checkpoint(&sessions, &id).await;
                    if sessions.active_state(&id).await.is_none()
                        && checkpoint.is_some()
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the original context tool owner must finish");
            continue;
        }
        let definition = definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap();
        let selection = p::InvocationSelection {
            routine: name.into(),
            revision: definition.revision.clone(),
            entrypoint: entry.into(),
            source_session: Some("context-source".into()),
            correlation: name.into(),
            parameters: Some(p::StructuredValue {
                json: serde_json::to_vec(&input).unwrap(),
            }),
        };
        let started = client
            .start_invocation(p::StartInvocationRequest {
                selection: Some(selection),
            })
            .await
            .unwrap()
            .into_inner()
            .invocation
            .unwrap();
        let mut watch = client
            .watch_invocations(p::WatchInvocationsRequest {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                for row in watch.message().await.unwrap().unwrap().invocations {
                    if row.id == started.id && row.status >= p::InvocationStatus::Succeeded as i32 {
                        assert_eq!(
                            row.status,
                            p::InvocationStatus::Succeeded as i32,
                            "{name}: {row:?}"
                        );
                        return;
                    }
                }
            }
        })
        .await
        .unwrap();
    }
    let record = sessions
        .load(&SessionId::new("context-source"))
        .await
        .unwrap()
        .record;
    assert!(
        record
            .metadata
            .values
            .contains_key("routine.categories.v1")
    );
    assert_eq!(record.active_session_masks().len(), 1);
    assert_eq!(
        record.active_session_masks()[0].mask.replacement.text,
        "Actual production category summary"
    );
    assert!(provider.category.load(Ordering::SeqCst) >= 1);
    assert_eq!(provider.mask.load(Ordering::SeqCst), 2);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let source = sessions
                .load(&SessionId::new("context-source"))
                .await
                .unwrap();
            if !source.record.has_unsettled_host_dispatch() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "original context tool owner must settle: {:?}",
            record.journal
        );
    });
    let mut watch = client
        .watch_invocations(p::WatchInvocationsRequest {
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    session_client
        .submit_user_message(waltr::grpc::waltr::session::v1::SubmitUserMessageRequest {
            session_id: "context-source".into(),
            text: "RUN_CONFIGURATION_TOOL".into(),
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            for row in watch.message().await.unwrap().unwrap().invocations {
                if row.status >= p::InvocationStatus::Succeeded as i32 {
                    let detail = client
                        .get_invocation(p::GetInvocationRequest {
                            invocation_id: row.id,
                        })
                        .await
                        .unwrap()
                        .into_inner()
                        .invocation
                        .unwrap();
                    if detail.routine == "configuration" {
                        assert_eq!(
                            detail.status,
                            p::InvocationStatus::Succeeded as i32,
                            "{detail:?}"
                        );
                        return;
                    }
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(provider.ceiling_denied.load(Ordering::SeqCst), 1);
    let direct = client
        .start_invocation(p::StartInvocationRequest {
            selection: Some(p::InvocationSelection {
                routine: "configuration".into(),
                revision: configuration.revision.clone(),
                entrypoint: "manual".into(),
                source_session: Some("context-source".into()),
                correlation: "permitted-manual".into(),
                parameters: Some(p::StructuredValue {
                    json: serde_json::to_vec(&json!({"prompt":"Explain without changes"})).unwrap(),
                }),
            }),
        })
        .await
        .unwrap()
        .into_inner()
        .invocation
        .unwrap();
    let mut watch = client
        .watch_invocations(p::WatchInvocationsRequest {
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            for row in watch.message().await.unwrap().unwrap().invocations {
                if row.id == direct.id && row.status >= p::InvocationStatus::Succeeded as i32 {
                    let operations = client.list_operations(p::ListOperationsRequest { invocation_id: row.id.clone(), limit: 100, ..Default::default() }).await.unwrap().into_inner();
                    assert_eq!(row.status, p::InvocationStatus::Succeeded as i32, "fresh context invocation {row:?}: {operations:?}");
                    return;
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(provider.ceiling_denied.load(Ordering::SeqCst), 2);
    let mut fresh = client
        .watch_invocations(p::WatchInvocationsRequest {
            limit: 100,
            source_session: Some("fresh-management-source".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    session_client
        .submit_user_message(waltr::grpc::waltr::session::v1::SubmitUserMessageRequest {
            session_id: "fresh-management-source".into(),
            text: "RUN_FRESH_CONTEXT_TOOL".into(),
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            for row in fresh.message().await.unwrap().unwrap().invocations {
                if row.routine == "context-management"
                    && row.status >= p::InvocationStatus::Succeeded as i32
                {
                    let operations = client.list_operations(p::ListOperationsRequest { invocation_id: row.id.clone(), limit: 100, ..Default::default() }).await.unwrap().into_inner();
                    assert_eq!(row.status, p::InvocationStatus::Succeeded as i32, "fresh context invocation {row:?}: {operations:?}");
                    return;
                }
            }
        }
    })
    .await
    .unwrap();
    let fresh_record = sessions
        .load(&SessionId::new("fresh-management-source"))
        .await
        .unwrap()
        .record;
    let fresh_checkpoint = fresh_record.metadata.values.get("routine.categories.v1").unwrap();
    let through = waltr_agent::TimelineCursor::new(fresh_checkpoint["through"].as_str().unwrap().parse().unwrap());
    let covered = waltr_agent::SessionCopy::from_record(&fresh_record, through).unwrap();
    assert!(!covered.timeline().is_empty());
    assert!(covered.timeline().iter().all(|item| fresh_record.item_metadata_values(&item.item_id).unwrap().unwrap().get("routine.category.v1") == Some(&json!("work"))));
    assert!(fresh_checkpoint["categories"].as_array().unwrap().iter().any(|category| category["key"] == "work"));
    assert!(fresh_record.timeline_events().any(|event| fresh_record.item_metadata_values(&event.item_id).ok().flatten().is_some_and(|values| values.get("routine.category.v1") == Some(&json!("work")))));
    assert_eq!(provider.management.load(Ordering::SeqCst), 2);
    drop(fresh);
    for (id, prompt, entrypoint) in [
        ("idle-source", "idle check", "idle-flush"),
        ("turn-source", "RUN_TURN_BATCH", "turn-batch"),
        ("task-source", "RUN_TASK_BATCH", "delegated-task-batch"),
    ] {
        let mut watch = client
            .watch_invocations(p::WatchInvocationsRequest {
                limit: 100,
                source_session: Some(id.into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        let mut source_updates = sessions.watch_sessions();
        session_client
            .submit_user_message(waltr::grpc::waltr::session::v1::SubmitUserMessageRequest {
                session_id: id.into(),
                text: prompt.into(),
            })
            .await
            .unwrap();
        if entrypoint == "delegated-task-batch" {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                provider.parent_entered.notified(),
            )
            .await
            .unwrap();
            provider.task_gate.release.add_permits(1);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let waltr_agent::SessionListEvent::Upsert(summary) =
                        source_updates.next().await.unwrap().event
                    {
                        if summary.id.as_str() == id && summary.coordination.queued_count == 1 {
                            break;
                        }
                    }
                }
            })
            .await
            .unwrap();
            // Queue the first real callback before the parent finishes, so no idle pin intervenes.
            provider.parent_release.release.add_permits(1);
            wait_callbacks(&provider, 1).await;
            provider.category_armed.store(true, Ordering::SeqCst);
            provider.task_gate.release.add_permits(1);
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                provider.category_entered.notified(),
            )
            .await
            .unwrap();
            let before = sessions.load(&SessionId::new(id)).await.unwrap().record;
            if race {
                // The second callback appends after the category helper pins its input.
                provider.callback_release.release.add_permits(1);
                wait_callbacks(&provider, 2).await;
                let advanced = sessions.load(&SessionId::new(id)).await.unwrap().record;
                assert_ne!(before.revision, advanced.revision);
            }
            provider.category_release.release.add_permits(1);
            let categorization = wait_semantic(
                &mut client,
                &mut watch,
                entrypoint,
                if race {
                    p::InvocationStatus::Uncertain
                } else {
                    p::InvocationStatus::Succeeded
                },
                None,
            )
            .await;
            if race {
                let operations = client
                    .list_operations(p::ListOperationsRequest {
                        invocation_id: categorization,
                        after_cursor: 0,
                        limit: 100,
                    })
                    .await
                    .unwrap()
                    .into_inner()
                    .operations;
                assert!(
                    operations
                        .iter()
                        .any(|operation| operation.name == "helper.run"
                            && operation.status == p::OperationStatus::Completed as i32)
                );
                assert!(
                    operations
                        .iter()
                        .any(|operation| operation.name == "context.transact"
                            && operation.status == p::OperationStatus::Uncertain as i32)
                );
                let rejected = sessions.load(&SessionId::new(id)).await.unwrap().record;
                assert_eq!(
                    before
                        .metadata
                        .values
                        .get("routine.categories.v1"),
                    rejected
                        .metadata
                        .values
                        .get("routine.categories.v1")
                );
                assert_eq!(
                    before.active_session_masks(),
                    rejected.active_session_masks()
                );
            } else {
                let record = sessions.load(&SessionId::new(id)).await.unwrap().record;
                let checkpoint = workflow_checkpoint(&sessions, &record.id).await.unwrap();
                let latest = sessions
                    .capture_context(
                        &SessionId::new(id),
                        "test-order",
                        waltr_agent::SessionCaptureLimits::default(),
                    )
                    .await
                    .unwrap();
                assert_eq!(checkpoint["through"], latest.identity.boundary().get().to_string());
                assert_eq!(before.active_session_masks(), record.active_session_masks());
            }
            provider
                .callback_release
                .release
                .add_permits(if race { 1 } else { 2 });
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let waltr_agent::SessionListEvent::Upsert(summary) =
                        source_updates.next().await.unwrap().event
                    {
                        if summary.id.as_str() == id
                            && summary.coordination.phase
                                == waltr_agent::SessionCoordinationPhase::Ready
                            && matches!(summary.status, waltr_agent::SessionStatus::Finished)
                            && summary
                                .current_prompt
                                .as_deref()
                                .is_some_and(|prompt| prompt.starts_with("Async task completed.\n"))
                            && provider.callbacks.load(Ordering::SeqCst) == 2
                        {
                            break;
                        }
                    }
                }
            })
            .await
            .unwrap();
            if race {
                wait_semantic(
                    &mut client,
                    &mut watch,
                    "idle-flush",
                    p::InvocationStatus::Succeeded,
                    None,
                )
                .await;
                let record = sessions.load(&SessionId::new(id)).await.unwrap().record;
                let checkpoint = workflow_checkpoint(&sessions, &record.id).await.unwrap();
                let latest = sessions
                    .capture_context(
                        &SessionId::new(id),
                        "test-order",
                        waltr_agent::SessionCaptureLimits::default(),
                    )
                    .await
                    .unwrap();
                assert_eq!(checkpoint["through"], latest.identity.boundary().get().to_string());
                assert_eq!(before.active_session_masks(), record.active_session_masks());
            }
            continue;
        }
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                for row in watch.message().await.unwrap().unwrap().invocations {
                    if row.status >= p::InvocationStatus::Succeeded as i32 {
                        let detail = client
                            .get_invocation(p::GetInvocationRequest {
                                invocation_id: row.id,
                            })
                            .await
                            .unwrap()
                            .into_inner()
                            .invocation
                            .unwrap();
                        if detail.routine == "context-categorization"
                            && detail.entrypoint == entrypoint
                        {
                            let operations = client.list_operations(p::ListOperationsRequest { invocation_id: detail.id.clone(), limit: 100, ..Default::default() }).await.unwrap().into_inner();
                            assert_eq!(
                                detail.status,
                                p::InvocationStatus::Succeeded as i32,
                                "{detail:?}; {operations:?}"
                            );
                            let record = sessions.load(&SessionId::new(id)).await.unwrap().record;
                            assert!(workflow_checkpoint(&sessions, &record.id).await.is_some());
                            return;
                        }
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "semantic event did not produce coverage: {id}/{entrypoint}; source turns={}",
                provider.semantic_turns.load(Ordering::SeqCst)
            )
        });
        if entrypoint == "turn-batch" {
            assert_eq!(provider.semantic_turns.load(Ordering::SeqCst), 20);
        }
    }
    stop.cancel();
    task.await.unwrap().unwrap();
}

async fn wait_callbacks(provider: &Provider, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while provider.callbacks.load(Ordering::SeqCst) < count {
            provider.callback_entered.notified().await;
        }
    })
    .await
    .unwrap();
}

async fn wait_semantic(
    client: &mut p::routine_service_client::RoutineServiceClient<tonic::transport::Channel>,
    watch: &mut tonic::Streaming<p::WatchInvocationsResponse>,
    entrypoint: &str,
    status: p::InvocationStatus,
    excluded: Option<&str>,
) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            for row in watch.message().await.unwrap().unwrap().invocations {
                if row.status >= p::InvocationStatus::Succeeded as i32
                    && excluded != Some(row.id.as_str())
                {
                    let detail = client
                        .get_invocation(p::GetInvocationRequest {
                            invocation_id: row.id,
                        })
                        .await
                        .unwrap()
                        .into_inner()
                        .invocation
                        .unwrap();
                    if detail.routine == "context-categorization" && detail.entrypoint == entrypoint
                    {
                        assert_eq!(detail.status, status as i32, "{detail:?}");
                        return detail.id;
                    }
                }
            }
        }
    })
    .await
    .unwrap()
}
