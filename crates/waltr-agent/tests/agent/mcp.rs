//! MCP agent integration tests.

use std::{
    collections::{BTreeSet, HashMap},
    fs,
    ops::Deref,
    process::Child,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use waltr_agent::definition::McpServerSelection;
use waltr_agent::{
    AgentRuntimeProvider, AgentSessionRunError, AgentSessionRuntimeError,
    AgentSessionRuntimeFactory, AgentTimelineEvent, AuthTarget, CHANNEL_ASSISTANT_TEXT,
    CHANNEL_TOOL_CALL, CHANNEL_TOOL_PROGRESS_INFO, CHANNEL_TOOL_RESULT, CompletionEvent,
    FileCredentialStore, InferenceProviderSettings, InferenceSettings, LlmStream,
    McpConnectionKind, McpResolvedConnection, MemorySessionStore, ModelApi, ModelRef,
    PendingWorkStatus, ProviderId, SessionCoordinator, SessionCreateRequest, SessionId,
    SessionStore, SessionTemplateKind, SessionTemplateSearchRequest, SessionTemplateSearchTarget,
    Suspension, TimelineContent, TimelineEventKind, TimelineQuery, TimelineStatus, ToolCall,
    ToolResultContentBlock,
};
use opentelemetry::{global, trace::TracerProvider as _};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    trace::{InMemorySpanExporter, SdkTracerProvider, SpanData},
};
use serde_json::json;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::{Registry, prelude::*};

use crate::sandbox::TestSandbox;
use crate::scripted_provider::{CapturingProvider, ScriptedProvider};
use crate::session::AgentTestSession;

const OPENAI_TEST_MODEL: &str = "gpt-5-nano";

#[tokio::test]
async fn mcp_skills_durable_permission_workflow_binds_activation_and_cross_origin_call()
-> waltr_agent::Result<()> {
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_skill_consent",
        |request| {
            let event = match request.tool_results().count() {
                0 => CompletionEvent::ToolCall(ToolCall::new(
                    "load_remote",
                    "load_skill",
                    json!({"source":"mcp","server":"origin","uri":"skill://ops/inspect/SKILL.md"}),
                )),
                1 => CompletionEvent::ToolCall(ToolCall::new(
                    "remote_action",
                    "mcp_origin_echo_marker",
                    json!({"value":"approved-action"}),
                )),
                2 => CompletionEvent::ToolCall(ToolCall::new(
                    "support",
                    "read_mcp_resource",
                    json!({"server":"origin","uri":"skill://ops/inspect/references/action.txt"}),
                )),
                _ => CompletionEvent::Finished,
            };
            vec![Ok(event)]
        },
    )));
    let root = tempfile::tempdir()?.keep();
    let sandbox = TestSandbox::new(&root).await;
    let mut definition = crate::agent_definitions::mcp_agent(false);
    definition.features.mcp.as_mut().unwrap().skills = true;
    let captured = provider.clone();
    let command = test_stdio_server_command();
    let factory = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.clone(),
        FileCredentialStore::new(root.join("credentials.json")),
        definition,
    )
    .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(test_model(), captured.clone())))
    .with_mcp_connection_resolver(move |_| {
        let command = command.clone();
        Box::pin(async move {
            Ok(vec![McpResolvedConnection {
                key: "origin".into(),
                kind: McpConnectionKind::Stdio,
                command,
                url: String::new(),
            }])
        })
    });
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone()).with_runtime_factory(factory);
    let mut create = SessionCreateRequest::new(SessionId::generate());
    create.inference_settings = Some(test_inference_settings());
    let id = sessions.create(create).await?.id().clone();
    sessions
        .submit_prompt(
            id.clone(),
            "load remote skill",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?
        .handle
        .wait()
        .await?;
    let record = store.load(&id).await.expect("stored session");
    let snapshot = record.snapshot();
    let activation = snapshot
        .external_requests
        .iter()
        .find(|request| request.tool_call_id.as_deref() == Some("load_remote"))
        .expect("activation consent");
    assert!(activation.prompt.contains("arbitrary host code execution"));
    assert_eq!(
        activation.payload["mcp_skill_consent"]["activation"]["manifest"]["uri"],
        "skill://ops/inspect/SKILL.md"
    );
    assert!(!record.timeline_events().any(|event| matches!(&event.content,TimelineContent::ToolResult {result} if result.name == "load_skill")));
    sessions
        .resume_external_response(
            id.clone(),
            activation.id.clone(),
            json!(waltr_agent::PermissionResponse::AllowOnce),
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?
        .handle
        .wait()
        .await?;
    let record = store.load(&id).await.expect("stored session");
    let snapshot = record.snapshot();
    let action = snapshot
        .external_requests
        .iter()
        .find(|request| request.tool_call_id.as_deref() == Some("remote_action"))
        .expect("origin-bound per-call consent");
    assert!(action.prompt.contains("origin"));
    assert!(record.timeline_events().any(|event| matches!(&event.content,TimelineContent::ToolResult {result} if result.name == "load_skill" && result.content.contains("@tool(shell) stays literal"))));
    sessions
        .resume_external_response(
            id.clone(),
            action.id.clone(),
            json!(waltr_agent::PermissionResponse::AllowOnce),
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?
        .handle
        .wait()
        .await?;
    let record = store.load(&id).await.expect("stored session");
    assert!(record.timeline_events().any(|event| matches!(&event.content,TimelineContent::ToolResult {result} if result.name == "read_mcp_resource" && result.content.contains("Supporting skill file."))));
    assert_eq!(
        record.snapshot().middleware_state["mcp-skills"]["held"][0]["approved"],
        true
    );
    assert_tool_was_surfaced(&provider, "load_skill");
    assert_tool_was_surfaced(&provider, "read_mcp_resource");
    sessions.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn mcp_skills_input_and_sandbox_ask_preserve_exact_call_approvals() -> waltr_agent::Result<()>
{
    for final_decision in 0..4 {
        let provider = Arc::new(ScriptedProvider::new(
            "skill_permission_retries",
            |request| {
                vec![Ok(match request.tool_results().count() {
                    0 => CompletionEvent::ToolCall(ToolCall::new(
                        "activate",
                        "load_skill",
                        json!({"source":"mcp","server":"origin","uri":"skill://ops/inspect/SKILL.md"}),
                    )),
                    1 => CompletionEvent::ToolCall(ToolCall::new(
                        "read-outside",
                        "read",
                        json!({"filePath":"../outside.txt"}),
                    )),
                    _ => CompletionEvent::Finished,
                })]
            },
        ));
        let parent = tempfile::tempdir()?;
        let root = parent.path().join("workspace");
        fs::create_dir(&root)?;
        fs::write(
            parent.path().join("outside.txt"),
            "sandbox-permission-protected",
        )?;
        let sandbox = TestSandbox::new(&root).await;
        let mut registry = waltr_agent::AgentDefinitionRegistry::new();
        registry
            .load_yaml(
                r"apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: test }
spec:
  features:
    - filesystem: { root: $(session.workspace.root), profile: auto, defaultPermissions: true }
    - mcp: { skills: true, defaultPermissions: true }
  permissions:
    - name: read
      permission:
        inputs:
          - match: [{ pointer: /filePath, value: { wildcard: '../*' } }]
            effect: ask
    - name: read
      permission:
        resources:
          paths: [{ match: { outsideRoot: $(session.workspace.root) }, effect: ask }]
",
            )
            .expect("load permission fixture");
        let definition = registry.resolve_agent("test").expect("resolve fixture");
        let command = test_stdio_server_command();
        let factory = AgentSessionRuntimeFactory::new(
            sandbox.handle(),
            root.clone(),
            FileCredentialStore::new(root.join("credentials.json")),
            definition,
        )
        .with_runtime_provider(move |_| {
            Ok(AgentRuntimeProvider::new(test_model(), provider.clone()))
        })
        .with_mcp_connection_resolver(move |_| {
            let command = command.clone();
            Box::pin(async move {
                Ok(vec![McpResolvedConnection {
                    key: "origin".into(),
                    kind: McpConnectionKind::Stdio,
                    command,
                    url: String::new(),
                }])
            })
        });
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store.clone()).with_runtime_factory(factory);
        let mut create = SessionCreateRequest::new(SessionId::generate());
        create.inference_settings = Some(test_inference_settings());
        let id = sessions.create(create).await?.id().clone();
        sessions
            .submit_prompt(
                id.clone(),
                "load and read",
                waltr_agent::RunConflictPolicy::ReplaceActive,
            )
            .await?
            .handle
            .wait()
            .await?;
        let mut seen = BTreeSet::new();
        // Activation, input Ask, skill-bound call consent, sandbox Ask. Each
        // request is approved once, despite replay across later suspensions.
        for stage in 0..4 {
            let snapshot = store.load(&id).await.unwrap().snapshot();
            let pending = snapshot
                .external_requests
                .iter()
                .find(|request| request.status == waltr_agent::ExternalRequestStatus::Pending)
                .expect("distinct pending approval");
            assert!(
                seen.insert(pending.id.as_str().to_owned()),
                "approval repeated at stage {stage}"
            );
            match stage {
                0 => assert!(pending.payload.get("mcp_skill_consent").is_some()),
                1 => assert!(pending.prompt.contains("input approval")),
                2 => assert!(
                    pending.payload["mcp_skill_consent"]["call"]["arguments"]["filePath"]
                        == "../outside.txt"
                ),
                3 => {
                    assert!(pending.payload.get("mcp_skill_consent").is_none());
                    assert!(!pending.prompt.contains("input approval"));
                    assert_eq!(
                        snapshot.middleware_state["permissions"]["one_shot"]
                            .as_array()
                            .unwrap()
                            .len(),
                        2
                    );
                }
                _ => unreachable!(),
            }
            if stage == 3 && final_decision == 3 {
                sessions.interrupt(&id).await?;
                continue;
            }
            let response = if stage == 3 && final_decision != 0 {
                waltr_agent::PermissionResponse::Deny {
                    message: None,
                    mode: waltr_agent::PermissionDenyMode::ReturnToolResult,
                }
            } else {
                waltr_agent::PermissionResponse::AllowOnce
            };
            if stage == 3 && final_decision == 2 {
                waltr_agent::AgentSessionRuntime::new(&store)
                    .resume(
                        &id,
                        waltr_agent::ResumeInput::ExternalResponse {
                            request_id: pending.id.clone(),
                            response: waltr_agent::ExternalResponse::new(json!(
                                waltr_agent::PermissionResponse::Deny {
                                    message: None,
                                    mode: waltr_agent::PermissionDenyMode::CancelWork
                                }
                            )),
                        },
                    )
                    .await?;
                continue;
            }
            sessions
                .resume_external_response(
                    id.clone(),
                    pending.id.clone(),
                    json!(response),
                    waltr_agent::RunConflictPolicy::ReplaceActive,
                )
                .await?
                .handle
                .wait()
                .await?;
        }
        let record = store.load(&id).await.unwrap();
        assert!(
            !record
                .snapshot()
                .external_requests
                .iter()
                .any(|request| request.status == waltr_agent::ExternalRequestStatus::Pending)
        );
        assert!(
            record.snapshot().middleware_state["permissions"]["one_shot"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(record.journal.iter().any(|event| matches!(event, waltr_agent::SessionJournalEvent::SandboxDispatchStarted { tool_call_id, .. } if tool_call_id == "read-outside")), final_decision == 0);
        let results = record
            .timeline_events()
            .filter_map(|event| match &event.content {
                TimelineContent::ToolResult { result } if result.call_id == "read-outside" => {
                    Some(result)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if final_decision < 2 {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].is_error, final_decision == 1);
        }
        assert_eq!(
            results
                .iter()
                .any(|result| result.content.contains("sandbox-permission-protected")),
            final_decision == 0
        );
        sessions.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn mcp_definition_selection_is_forwarded_to_host_resolver() -> waltr_agent::Result<()> {
    for (servers, expected) in [
        (
            "{ include: [selected] }",
            McpServerSelection::Include(BTreeSet::from(["selected".to_owned()])),
        ),
        (
            "{ exclude: [ignored] }",
            McpServerSelection::Exclude(BTreeSet::from(["ignored".to_owned()])),
        ),
        (
            "{ exclude: [] }",
            McpServerSelection::Exclude(BTreeSet::new()),
        ),
    ] {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let resolver_requests = Arc::clone(&requests);
        let provider = Arc::new(ScriptedProvider::from_turns(
            "mcp_selection_provider",
            vec![vec![CompletionEvent::Finished]],
        ));
        let root = tempfile::tempdir()?.keep();
        let sandbox = TestSandbox::new(&root).await;
        let factory = AgentSessionRuntimeFactory::new(
            sandbox.handle(),
            root.clone(),
            FileCredentialStore::new(root.join("credentials.json")),
            crate::agent_definitions::mcp_agent_with_servers(servers),
        )
        .with_runtime_provider(move |_| {
            Ok(AgentRuntimeProvider::new(test_model(), provider.clone()))
        })
        .with_mcp_connection_resolver(move |request| {
            resolver_requests
                .lock()
                .expect("resolver requests lock")
                .push(request);
            Box::pin(async { Ok(Vec::new()) })
        });
        let sessions =
            SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory);
        let mut request = SessionCreateRequest::new(SessionId::generate());
        request.inference_settings = Some(test_inference_settings());
        let session_id = sessions.create(request).await?.id().clone();
        let result = sessions
            .submit_prompt(
                session_id,
                "resolve selection",
                waltr_agent::RunConflictPolicy::ReplaceActive,
            )
            .await;

        if matches!(expected, McpServerSelection::Include(_)) {
            assert!(matches!(
                result,
                Err(AgentSessionRunError::Runtime(
                    AgentSessionRuntimeError::FailedPrecondition(_)
                ))
            ));
        } else {
            result?.handle.wait().await?;
        }
        assert_eq!(
            requests.lock().expect("resolver requests lock").as_slice(),
            &[waltr_agent::McpConnectionResolveRequest { servers: expected }]
        );
    }
    Ok(())
}

#[tokio::test]
async fn mcp_include_empty_selects_none_without_calling_host_resolver() -> waltr_agent::Result<()> {
    let called = Arc::new(AtomicBool::new(false));
    let resolver_called = Arc::clone(&called);
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_empty_selection_provider",
        vec![vec![CompletionEvent::Finished]],
    ));
    let root = tempfile::tempdir()?.keep();
    let sandbox = TestSandbox::new(&root).await;
    let factory = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.clone(),
        FileCredentialStore::new(root.join("credentials.json")),
        crate::agent_definitions::mcp_agent_with_servers("{ include: [] }"),
    )
    .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(test_model(), provider.clone())))
    .with_mcp_connection_resolver(move |_| {
        resolver_called.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(Vec::new()) })
    });
    let sessions = SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory);
    let mut request = SessionCreateRequest::new(SessionId::generate());
    request.inference_settings = Some(test_inference_settings());
    let session_id = sessions.create(request).await?.id().clone();
    sessions
        .submit_prompt(
            session_id,
            "no MCP",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?
        .handle
        .wait()
        .await?;

    assert!(!called.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test]
async fn mcp_agent_loop_discovers_and_calls_stdio_and_streamable_http_direct_tools()
-> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let http_server = HttpMcpTestServer::start().await?;
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "scripted_mcp_provider",
        |request| direct_events(request.tool_results().count()),
    )));

    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "call direct MCP tools",
        vec![
            stdio_runtime_server(&stdio_command),
            http_runtime_server(&http_server),
        ],
    )
    .await?;
    session.require_tool_execution(provider.name())?;
    let context = format!(
        "{}\n{}",
        crate::session::session_context(&provider, &session),
        provider.tool_context()
    );
    let direct_results = tool_result_text(&session);

    assert_tool_was_surfaced(&provider, "mcp_stdio_test_echo_marker");
    assert_tool_was_surfaced(&provider, "mcp_stdio_test_emit_log_marker");
    assert_tool_was_surfaced(&provider, "mcp_http_test_echo_marker");
    assert_tool_was_surfaced(&provider, "mcp_http_test_emit_log_marker");
    assert_tool_was_not_surfaced(&provider, "search_mcp_tools");
    assert_tool_was_not_surfaced(&provider, "get_mcp_tool_details");
    assert_tool_was_not_surfaced(&provider, "call_mcp_tool");
    assert_tool_call(&session, "mcp_stdio_test_echo_marker", &context);
    assert_tool_call(&session, "mcp_stdio_test_emit_log_marker", &context);
    assert_tool_call(&session, "mcp_http_test_echo_marker", &context);
    assert_tool_call(&session, "mcp_http_test_emit_log_marker", &context);
    assert!(
        direct_results.contains("stdio-mcp-tool-called-707"),
        "{context}"
    );
    assert!(
        direct_results.contains("http-mcp-tool-called-707"),
        "{context}"
    );
    assert!(
        direct_results.contains("stdio-mcp-log-emitted-818"),
        "{context}"
    );
    assert!(
        direct_results.contains("http-mcp-log-emitted-818"),
        "{context}"
    );
    assert!(session.text.contains("mcp-flow-complete"), "{context}");

    Ok(())
}

#[tokio::test]
async fn mcp_streamable_http_delivers_large_images_to_the_provider() -> waltr_agent::Result<()> {
    let http_server = HttpMcpTestServer::start().await?;
    let saw_image = Arc::new(AtomicBool::new(false));
    let provider_saw_image = saw_image.clone();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "scripted_mcp_image_provider",
        move |request| {
            let Some(result) = request.tool_results().next() else {
                return vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "call-large-image",
                    "mcp_http_test_large_image",
                    json!({}),
                )))];
            };
            assert!(matches!(
                &result.content.blocks()[1],
                ToolResultContentBlock::Image(image)
                    if image.data().len() > 64 * 1024
                        && image.media_type() == "image/png"
                        && image.decoded_bytes() == 96 * 1024
            ));
            assert!(!result.text_projection().contains("iVBOR"));
            provider_saw_image.store(true, Ordering::SeqCst);
            vec![
                Ok(CompletionEvent::Delta("image-flow-complete".to_owned())),
                Ok(CompletionEvent::Finished),
            ]
        },
    )));

    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "render image",
        vec![http_runtime_server(&http_server)],
    )
    .await?;

    assert_tool_was_surfaced(&provider, "mcp_http_test_large_image");
    assert!(saw_image.load(Ordering::SeqCst));
    assert!(session.text.contains("image-flow-complete"));
    assert!(tool_result_text(&session).contains("image/png"));
    assert!(!tool_result_text(&session).contains("iVBOR"));

    Ok(())
}

#[tokio::test]
async fn mcp_streamable_http_auth_failure_is_reported_as_auth_required() -> waltr_agent::Result<()> {
    let http_server = HttpMcpTestServer::start_requiring_bearer("valid-token").await?;
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_auth_required_provider",
        vec![vec![CompletionEvent::Finished]],
    ));
    let sessions =
        mcp_sessions_with_connections(provider, vec![http_runtime_server(&http_server)]).await;
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let result = sessions
        .submit_prompt(
            session_id,
            "trigger mcp auth check",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await;
    let error = match result {
        Ok(submission) => submission
            .handle
            .wait()
            .await
            .expect_err("protected MCP server should require auth"),
        Err(error) => error,
    };

    assert!(
        matches!(
            error,
            AgentSessionRunError::Runtime(AgentSessionRuntimeError::Authentication {
                target: AuthTarget::McpServer { ref key },
                ..
            }) if key == "http_test"
        ),
        "unexpected error: {error:?}"
    );

    Ok(())
}

#[tokio::test]
async fn mcp_authentication_is_the_first_blocker_before_provider_authentication()
-> waltr_agent::Result<()> {
    let http_server = HttpMcpTestServer::start_requiring_bearer("valid-token").await?;
    let expected_endpoint = http_server.endpoint.clone();
    let resolved_endpoint = expected_endpoint.clone();
    let root = tempfile::tempdir()?.keep();
    let sandbox = TestSandbox::new(&root).await;
    let credentials = FileCredentialStore::new(root.join("credentials.json"));
    let factory = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root,
        credentials,
        crate::agent_definitions::mcp_agent(true),
    )
    .with_runtime_provider(|_| {
        Err(AgentSessionRuntimeError::Authentication {
            target: AuthTarget::provider("configured-provider"),
            message: "provider OAuth credential is required".to_owned(),
            reason: waltr_agent::AuthenticationFailureReason::CredentialRequired,
            mcp_authorization: None,
        })
    })
    .with_mcp_connection_resolver(move |_| {
        let connection = McpResolvedConnection {
            key: "http_test".to_owned(),
            kind: McpConnectionKind::StreamableHttp,
            command: String::new(),
            url: resolved_endpoint.clone(),
        };
        Box::pin(async move { Ok(vec![connection]) })
    });
    let sessions = SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let error = sessions
        .submit_prompt(
            session_id,
            "trigger both authentication blockers",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await
        .expect_err("MCP authentication should block runtime construction first");

    assert!(matches!(
        error,
        AgentSessionRunError::Runtime(AgentSessionRuntimeError::Authentication {
            target: AuthTarget::McpServer { ref key },
            reason: waltr_agent::AuthenticationFailureReason::CredentialRequired,
            mcp_authorization: Some(ref context),
            ..
        }) if key == "http_test" && context.endpoint == expected_endpoint && context.http_status == 401
    ));

    Ok(())
}

#[tokio::test]
async fn mcp_progress_notifications_are_forwarded_to_timeline() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_progress_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "call_direct_progress",
                "mcp_stdio_test_progress_marker",
                json!({ "value": "stdio-mcp-progress-result-919" }),
            ))],
            vec![
                CompletionEvent::Delta("mcp-progress-flow-complete".to_owned()),
                CompletionEvent::Finished,
            ],
        ],
    ));
    let sessions = mcp_sessions(provider).await;
    let session = run_session_prompt(
        &sessions,
        "call MCP progress tool",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;

    assert!(
        tool_result_text(&session).contains("stdio-mcp-progress-result-919"),
        "{session:#?}"
    );
    let tool_item_id = session
        .timeline
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ToolCall { call } if call.id == "call_direct_progress" => {
                Some(event.item_id.clone())
            }
            _ => None,
        })
        .expect("tool call timeline item");
    assert!(
        session.timeline.iter().any(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_PROGRESS_INFO)
                && event.parent_id.as_ref() == Some(&tool_item_id)
                && matches!(
                    &event.content,
                    TimelineContent::TextDelta { delta }
                        if delta.contains("stdio-mcp-progress-marker (1/2)")
                )
        }),
        "expected MCP progress timeline event: {:#?}",
        session.timeline
    );

    Ok(())
}

#[tokio::test]
async fn mcp_trace_propagation_explicit_false_is_preserved() -> waltr_agent::Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());
    let _trace = TestTraceCapture::start();
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_trace_disabled_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "call_trace_disabled_stdio",
                "mcp_stdio_test_echo_marker",
                json!({ "value": "trace-disabled-tool-called" }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let sessions = mcp_sessions_with_trace(provider, false).await;

    let session = run_session_prompt(
        &sessions,
        "call MCP tool without trace propagation",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;

    assert!(
        !tool_result_text(&session).contains("trace:tool:present"),
        "{session:#?}"
    );
    Ok(())
}

#[tokio::test]
async fn mcp_trace_propagation_reaches_tool_requests() -> waltr_agent::Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());
    let _trace = TestTraceCapture::start();
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_trace_tool_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "call_trace_stdio",
                "mcp_stdio_test_echo_marker",
                json!({ "value": "trace-enabled-tool-called" }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let sessions = mcp_sessions(provider).await;
    let session = run_session_prompt(
        &sessions,
        "call MCP tool with trace propagation",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;

    assert!(
        tool_result_text(&session).contains("trace:tool:present"),
        "{session:#?}"
    );
    Ok(())
}

#[tokio::test]
async fn mcp_trace_propagation_reaches_prompt_resource_and_completion_requests()
-> waltr_agent::Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());
    let _trace = TestTraceCapture::start();
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_trace_template_provider",
        |_| {
            vec![
                Ok(CompletionEvent::Delta("trace-template-complete".to_owned())),
                Ok(CompletionEvent::Finished),
            ]
        },
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    sessions.set_connections(vec![stdio_runtime_server(&stdio_command)]);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let query = "@resource(stdio_test, \"test://items/4\")".to_owned();
    let cursor = query.rfind('"').expect("query contains closing quote");
    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query,
                cursor: Some(cursor),
                limit: 20,
                kinds: vec![SessionTemplateKind::Resource],
                target: SessionTemplateSearchTarget::Resources,
            },
        )
        .await?;
    assert!(
        options
            .iter()
            .any(|option| option.label == "trace:completion:present"),
        "expected trace completion marker: {options:#?}"
    );

    let submission = sessions
        .submit_prompt(
            session_id.clone(),
            "/stdio_test:greet_prompt({\"name\":\"Waltr\"})\nRead @resource(stdio_test, \"test://status\") before answering.",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?;
    submission.handle.wait().await?;
    let prompts = provider.request_prompts();
    let rendered = prompts
        .first()
        .unwrap_or_else(|| panic!("expected captured provider prompt"));

    assert!(rendered.contains("trace:prompt:present"), "{rendered}");
    assert!(rendered.contains("trace:resource:present"), "{rendered}");
    Ok(())
}

#[tokio::test]
async fn mcp_trace_tool_request_span_is_child_of_agent_tool_call() -> waltr_agent::Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());
    let trace = TestTraceCapture::start();
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_trace_span_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "call_trace_span_stdio",
                "mcp_stdio_test_echo_marker",
                json!({ "value": "trace-span-tool-called" }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let sessions = mcp_sessions(provider).await;
    run_session_prompt(
        &sessions,
        "call MCP tool for span assertions",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;

    let spans = trace.finished_spans();
    let turn = find_span(&spans, "agent.turn", |_| true);
    let tool_call = find_span(&spans, "agent.tool_call", |_| true);
    let mcp_request = find_span(&spans, "mcp.client.request", |span| {
        span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == "rpc.method" && attribute.value.as_str() == "tools/call"
        })
    });

    assert_eq!(
        tool_call.span_context.trace_id(),
        turn.span_context.trace_id()
    );
    assert_eq!(
        mcp_request.span_context.trace_id(),
        tool_call.span_context.trace_id()
    );
    assert!(
        is_descendant(&spans, tool_call, turn),
        "expected agent.tool_call under agent.turn: {spans:#?}"
    );
    assert!(
        is_descendant(&spans, mcp_request, tool_call)
            || mcp_request.parent_span_id == tool_call.span_context.span_id()
            || shares_trace_with_parent(&spans, mcp_request, tool_call),
        "expected tools/call MCP request under or within agent.tool_call trace: {spans:#?}"
    );

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn mcp_cancellation_cancels_tool_work_and_preserves_stdio_runtime() -> waltr_agent::Result<()>
{
    let temp = tempfile::tempdir()?;
    let marker = temp.path().join("mcp-cancel-marker.txt");
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_cancel_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "mcp-cancel-call",
                "mcp_stdio_test_delayed_write_marker",
                json!({
                    "path": marker.display().to_string(),
                    "value": "foo",
                    "delay_ms": 5000_u64,
                }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let store = MemorySessionStore::new();
    let sessions = mcp_sessions_with_store(store.clone(), provider).await;
    sessions.set_connections(vec![stdio_runtime_server(&stdio_command)]);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;
    let submission = sessions
        .submit_prompt(
            session_id.clone(),
            "run delayed MCP write",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?;
    let mut stream = sessions.watch_timeline(session_id.clone());

    loop {
        let update = stream.next().await.expect("tool call event");
        let waltr_agent::SessionTimelineEvent::Event(event) = update.event else {
            continue;
        };
        assert!(
            !crate::session::channel_is(&event, CHANNEL_TOOL_RESULT),
            "MCP tool finished before cancellation: {event:#?}"
        );
        if crate::session::channel_is(&event, CHANNEL_TOOL_CALL) {
            break;
        }
    }

    tokio::select! {
        event = stream.next() => panic!("MCP tool finished before cancellation: {event:#?}"),
        () = tokio::time::sleep(Duration::from_secs(1)) => {}
    }
    sessions.interrupt(&session_id).await?;
    drop(submission.handle.wait().await);

    let remaining = sessions
        .timeline(&session_id, TimelineQuery::forward(None, 100))
        .await?;

    assert!(
        remaining.iter().any(|event| {
            matches!(
                &event.content,
                TimelineContent::ToolResult { result }
                    if crate::session::channel_is(event, CHANNEL_TOOL_RESULT)
                        && result.content == "interrupted"
            )
        }),
        "canceled run should persist an interrupted MCP tool result: {remaining:#?}"
    );
    assert!(
        !marker.exists() || fs::read_to_string(&marker)?.is_empty(),
        "canceled MCP tool should not write marker before the server observes cancellation"
    );
    let record = store.load(&session_id).await.expect("stored session");
    assert!(record.snapshot().pending_work.iter().any(|work| {
        work.id == "mcp-cancel-call"
            && matches!(
                work.status,
                PendingWorkStatus::Canceled(waltr_agent::CancelReason::SessionInterrupted)
            )
    }));
    assert!(matches!(
        record.snapshot().status,
        waltr_agent::SessionStatus::Suspended(Suspension::Interrupted { .. })
    ));
    tokio::time::sleep(Duration::from_secs(5)).await;

    let recovery_provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_recovery_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "mcp-recovery-call",
                "mcp_stdio_test_echo_marker",
                json!({ "value": "stdio-after-cancel-909" }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let recovery_sessions = mcp_sessions(recovery_provider.clone()).await;
    let recovery = run_session_prompt(
        &recovery_sessions,
        "recover MCP stdio",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    assert!(
        tool_result_text(&recovery).contains("stdio-after-cancel-909"),
        "{}",
        crate::session::session_context(&recovery_provider, &recovery)
    );

    Ok(())
}

#[tokio::test]
async fn mcp_prompt_templates_are_discovered_from_stdio_server() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::new("mcp_prompt_search_provider", |_| {
        vec![Ok(CompletionEvent::Finished)]
    }));
    let sessions = mcp_sessions(provider).await;
    sessions.set_connections(vec![stdio_runtime_server(&stdio_command)]);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: "stdio".to_owned(),
                limit: 20,
                kinds: vec![SessionTemplateKind::Prompt],
                target: SessionTemplateSearchTarget::Prompts,
                cursor: None,
            },
        )
        .await?;

    let status = options
        .iter()
        .find(|option| option.template == "/stdio_test:status_prompt")
        .unwrap_or_else(|| panic!("expected status prompt template: {options:#?}"));
    assert_eq!(status.metadata["promptType"], "mcp");
    assert_eq!(status.metadata["server"], "stdio_test");
    let greet = options
        .iter()
        .find(|option| option.template == "/stdio_test:greet_prompt({\"name\":\"\"})")
        .unwrap_or_else(|| panic!("expected greet prompt template: {options:#?}"));
    assert_eq!(greet.kind, SessionTemplateKind::Prompt);
    assert_eq!(greet.metadata["arguments"][0]["name"], "name");
    assert_eq!(greet.metadata["arguments"][0]["required"], true);
    assert!(
        options
            .iter()
            .all(|option| option.metadata["name"] != "review.code"),
        "dotted MCP prompt names cannot be represented by slash templates yet: {options:#?}"
    );

    Ok(())
}

#[tokio::test]
async fn mcp_prompt_template_renders_before_provider_request() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_prompt_render_provider",
        |_| {
            vec![
                Ok(CompletionEvent::Delta("prompt-render-complete".to_owned())),
                Ok(CompletionEvent::Finished),
            ]
        },
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "/stdio_test:greet_prompt({\"name\":\"Waltr\"})\nUse this prompt.",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    let prompts = provider.request_prompts();
    let rendered = prompts
        .first()
        .unwrap_or_else(|| panic!("expected captured provider prompt"));

    assert!(
        rendered.contains("hello Waltr from stdio-mcp-prompt-414"),
        "{rendered}"
    );
    assert!(
        rendered
            .contains("<template-result kind=\"mcp_prompt\" source=\"stdio_test:greet_prompt\">"),
        "{rendered}"
    );
    assert!(session.tool_calls.is_empty(), "{session:#?}");
    assert!(session.text.contains("prompt-render-complete"));

    Ok(())
}

#[tokio::test]
async fn mcp_prompt_template_rejects_non_string_arguments() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_prompt_argument_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "/stdio_test:greet_prompt({\"name\":123})\nUse this prompt.",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    let prompts = provider.request_prompts();
    let rendered = prompts
        .first()
        .unwrap_or_else(|| panic!("expected captured provider prompt"));

    assert!(
        rendered.contains(
            "<template-error>mcp prompt stdio_test:greet_prompt: argument `name` must be a string</template-error>"
        ),
        "{rendered}"
    );
    assert!(
        !rendered.contains("hello 123 from stdio-mcp-prompt-414"),
        "{rendered}"
    );
    assert!(session.tool_calls.is_empty(), "{session:#?}");

    Ok(())
}

#[tokio::test]
async fn mcp_resource_templates_are_discovered_from_stdio_server() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::new(
        "mcp_resource_search_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    ));
    let sessions = mcp_sessions(provider).await;
    sessions.set_connections(vec![stdio_runtime_server(&stdio_command)]);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: "status".to_owned(),
                limit: 20,
                kinds: vec![SessionTemplateKind::Resource],
                target: SessionTemplateSearchTarget::Resources,
                cursor: None,
            },
        )
        .await?;

    let resource = options
        .iter()
        .find(|option| option.template == "@resource(stdio_test, \"test://status\")")
        .unwrap_or_else(|| panic!("expected status resource template: {options:#?}"));
    assert_eq!(resource.kind, SessionTemplateKind::Resource);
    assert_eq!(resource.metadata["resourceType"], "mcp");
    assert_eq!(resource.metadata["server"], "stdio_test");
    assert_eq!(resource.metadata["uri"], "test://status");
    assert_eq!(resource.metadata["mimeType"], "text/plain");

    Ok(())
}

#[tokio::test]
async fn mcp_uri_resource_templates_are_discovered_from_stdio_server() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::new(
        "mcp_uri_resource_search_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    ));
    let sessions = mcp_sessions(provider).await;
    sessions.set_connections(vec![stdio_runtime_server(&stdio_command)]);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: "item".to_owned(),
                limit: 20,
                kinds: vec![SessionTemplateKind::Resource],
                target: SessionTemplateSearchTarget::Resources,
                cursor: None,
            },
        )
        .await?;

    let resource = options
        .iter()
        .find(|option| option.template == "@resource(stdio_test, \"test://items/{id}\")")
        .unwrap_or_else(|| panic!("expected item resource URI template: {options:#?}"));
    assert_eq!(resource.kind, SessionTemplateKind::Resource);
    assert_eq!(resource.metadata["resourceType"], "mcp_template");
    assert_eq!(resource.metadata["server"], "stdio_test");
    assert_eq!(resource.metadata["uriTemplate"], "test://items/{id}");
    assert_eq!(resource.metadata["mimeType"], "text/plain");

    Ok(())
}

#[tokio::test]
async fn mcp_uri_resource_template_completions_are_search_options() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::new(
        "mcp_uri_resource_completion_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    ));
    let sessions = mcp_sessions(provider).await;
    sessions.set_connections(vec![stdio_runtime_server(&stdio_command)]);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let query = "@resource(stdio_test, \"test://items/4\")".to_owned();
    let cursor = query.rfind('"').expect("query contains closing quote");
    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query,
                cursor: Some(cursor),
                limit: 20,
                kinds: vec![SessionTemplateKind::Resource],
                target: SessionTemplateSearchTarget::Resources,
            },
        )
        .await?;

    let completion = options
        .iter()
        .find(|option| option.template == "@resource(stdio_test, \"test://items/42\")")
        .unwrap_or_else(|| panic!("expected item resource URI completion: {options:#?}"));
    assert_eq!(completion.kind, SessionTemplateKind::Resource);
    assert_eq!(completion.label, "42");
    assert_eq!(completion.detail, "test://items/42");
    assert_eq!(
        completion.metadata["resourceType"],
        "mcp_template_completion"
    );
    assert_eq!(completion.metadata["server"], "stdio_test");
    assert_eq!(completion.metadata["uriTemplate"], "test://items/{id}");
    assert_eq!(completion.metadata["argument"], "id");
    assert_eq!(completion.metadata["value"], "42");

    Ok(())
}

#[tokio::test]
async fn mcp_resource_template_renders_before_provider_request() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_resource_render_provider",
        |_| {
            vec![
                Ok(CompletionEvent::Delta(
                    "resource-render-complete".to_owned(),
                )),
                Ok(CompletionEvent::Finished),
            ]
        },
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "Read @resource(stdio_test, \"test://status\") before answering.",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    let prompts = provider.request_prompts();
    let rendered = prompts
        .first()
        .unwrap_or_else(|| panic!("expected captured provider prompt"));

    assert!(
        rendered.contains("stdio-mcp-resource-status-616"),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "<template-result kind=\"mcp_resource\" source=\"stdio_test:test://status\">"
        ),
        "{rendered}"
    );
    assert!(session.tool_calls.is_empty(), "{session:#?}");
    assert!(session.text.contains("resource-render-complete"));

    Ok(())
}

#[tokio::test]
async fn mcp_uri_resource_template_renders_filled_uri_before_provider_request()
-> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_uri_resource_render_provider",
        |_| {
            vec![
                Ok(CompletionEvent::Delta(
                    "resource-template-render-complete".to_owned(),
                )),
                Ok(CompletionEvent::Finished),
            ]
        },
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "Read @resource(stdio_test, \"test://items/42\") before answering.",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    let prompts = provider.request_prompts();
    let rendered = prompts
        .first()
        .unwrap_or_else(|| panic!("expected captured provider prompt"));

    assert!(
        rendered.contains("stdio-mcp-resource-template-item-42-717"),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "<template-result kind=\"mcp_resource\" source=\"stdio_test:test://items/42\">"
        ),
        "{rendered}"
    );
    assert!(session.tool_calls.is_empty(), "{session:#?}");
    assert!(session.text.contains("resource-template-render-complete"));

    Ok(())
}

#[tokio::test]
async fn mcp_resources_surface_one_generic_reader_not_per_resource_tools() -> waltr_agent::Result<()>
{
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_resource_tool_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let sessions = mcp_sessions(provider.clone()).await;

    run_session_prompt(
        &sessions,
        "resources use one generic reader",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;

    assert_tool_was_not_surfaced(&provider, "mcp_stdio_test_status_resource");
    assert_tool_was_surfaced(&provider, "read_mcp_resource");

    Ok(())
}

#[tokio::test]
async fn mcp_generic_reader_reads_listed_and_filled_template_resources_as_data()
-> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_generic_resource_reader",
        |request| {
            if request.tool_results().next().is_none() {
                ["test://status", "test://items/42"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, uri)| {
                        Ok(CompletionEvent::ToolCall(ToolCall::new(
                            format!("resource-{index}"),
                            "read_mcp_resource",
                            json!({"server":"stdio_test","uri":uri}),
                        )))
                    })
                    .collect()
            } else {
                let results: Vec<_> = request.tool_results().collect();
                assert_eq!(results.len(), 2);
                for (result, marker) in results.iter().zip([
                    "stdio-mcp-resource-status-616",
                    "stdio-mcp-resource-template-item-42-717",
                ]) {
                    assert!(!result.is_error, "{result:?}");
                    let data: serde_json::Value = serde_json::from_str(&result.content).unwrap();
                    assert!(
                        data["data"]["contents"][0]["text"]
                            .as_str()
                            .unwrap()
                            .contains(marker)
                    );
                    assert_eq!(data["verified_skill_origins"], json!([]));
                }
                vec![Ok(CompletionEvent::Finished)]
            }
        },
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "read resource data",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    assert_eq!(session.tool_calls.len(), 2);
    assert_tool_was_surfaced(&provider, "read_mcp_resource");
    Ok(())
}

#[tokio::test]
async fn mcp_server_instructions_are_added_to_system_prompt() -> waltr_agent::Result<()> {
    let server = HttpMcpTestServer::start_with_instructions(
        "Prefer http marker tools for instruction-test requests.",
    )
    .await?;
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_instructions_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let sessions = mcp_sessions(provider.clone()).await;

    run_session_prompt(
        &sessions,
        "check MCP instructions",
        vec![http_runtime_server(&server)],
    )
    .await?;

    let system_prompt = captured_system_prompt(&provider);
    assert!(
        system_prompt.contains("MCP server instructions from `http_test`:"),
        "{system_prompt}"
    );
    assert!(
        system_prompt.contains("Prefer http marker tools for instruction-test requests."),
        "{system_prompt}"
    );

    Ok(())
}

#[tokio::test]
async fn mcp_server_instructions_are_ordered_by_server_key() -> waltr_agent::Result<()> {
    let alpha = HttpMcpTestServer::start_with_instructions("alpha instructions marker").await?;
    let beta = HttpMcpTestServer::start_with_instructions("beta instructions marker").await?;
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_ordered_instructions_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let sessions = mcp_sessions(provider.clone()).await;

    run_session_prompt(
        &sessions,
        "check ordered MCP instructions",
        vec![
            http_runtime_server_with_key("beta", &beta),
            http_runtime_server_with_key("alpha", &alpha),
        ],
    )
    .await?;

    let system_prompt = captured_system_prompt(&provider);
    let alpha_index = system_prompt
        .find("MCP server instructions from `alpha`:")
        .expect("alpha instructions");
    let beta_index = system_prompt
        .find("MCP server instructions from `beta`:")
        .expect("beta instructions");
    assert!(alpha_index < beta_index, "{system_prompt}");

    Ok(())
}

#[tokio::test]
async fn mcp_server_instructions_can_be_disabled_by_definition() -> waltr_agent::Result<()> {
    let server = HttpMcpTestServer::start_with_instructions("disabled instructions marker").await?;
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_disabled_instructions_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let sessions = mcp_sessions_with_instructions_enabled(provider.clone(), false).await;

    run_session_prompt(
        &sessions,
        "check disabled MCP instructions",
        vec![http_runtime_server(&server)],
    )
    .await?;

    let system_prompt = captured_system_prompt(&provider);
    assert!(
        !system_prompt.contains("disabled instructions marker"),
        "{system_prompt}"
    );

    Ok(())
}

fn stdio_runtime_server(command: &str) -> McpResolvedConnection {
    McpResolvedConnection {
        key: "stdio_test".to_owned(),
        kind: McpConnectionKind::Stdio,
        command: command.to_owned(),
        url: String::new(),
    }
}

fn http_runtime_server(server: &HttpMcpTestServer) -> McpResolvedConnection {
    http_runtime_server_with_key("http_test", server)
}

fn http_runtime_server_with_key(key: &str, server: &HttpMcpTestServer) -> McpResolvedConnection {
    McpResolvedConnection {
        key: key.to_owned(),
        kind: McpConnectionKind::StreamableHttp,
        command: String::new(),
        url: server.endpoint.clone(),
    }
}

struct TestMcpSessions {
    sessions: SessionCoordinator<MemorySessionStore>,
    connections: Arc<Mutex<Vec<McpResolvedConnection>>>,
    _sandbox: TestSandbox,
}

impl TestMcpSessions {
    fn set_connections(&self, connections: Vec<McpResolvedConnection>) {
        *self.connections.lock().expect("MCP connections lock") = connections;
    }
}

impl Deref for TestMcpSessions {
    type Target = SessionCoordinator<MemorySessionStore>;

    fn deref(&self) -> &Self::Target {
        &self.sessions
    }
}

async fn mcp_sessions(provider: Arc<impl LlmStream + 'static>) -> TestMcpSessions {
    mcp_sessions_with_instructions_enabled(provider, true).await
}

async fn mcp_sessions_with_trace(
    provider: Arc<impl LlmStream + 'static>,
    trace_propagation: bool,
) -> TestMcpSessions {
    mcp_sessions_with_store_and_definition(
        MemorySessionStore::new(),
        provider,
        crate::agent_definitions::mcp_agent_with_trace(true, Some(trace_propagation)),
    )
    .await
}

async fn mcp_sessions_with_connections(
    provider: Arc<impl LlmStream + 'static>,
    connections: Vec<McpResolvedConnection>,
) -> TestMcpSessions {
    let sessions = mcp_sessions(provider).await;
    sessions.set_connections(connections);
    sessions
}

async fn mcp_sessions_with_instructions_enabled(
    provider: Arc<impl LlmStream + 'static>,
    instructions_enabled: bool,
) -> TestMcpSessions {
    mcp_sessions_with_store_and_instructions_enabled(
        MemorySessionStore::new(),
        provider,
        instructions_enabled,
    )
    .await
}

async fn mcp_sessions_with_store(
    store: MemorySessionStore,
    provider: Arc<impl LlmStream + 'static>,
) -> TestMcpSessions {
    mcp_sessions_with_store_and_instructions_enabled(store, provider, true).await
}

async fn mcp_sessions_with_store_and_instructions_enabled(
    store: MemorySessionStore,
    provider: Arc<impl LlmStream + 'static>,
    instructions_enabled: bool,
) -> TestMcpSessions {
    mcp_sessions_with_store_and_definition(
        store,
        provider,
        crate::agent_definitions::mcp_agent(instructions_enabled),
    )
    .await
}

async fn mcp_sessions_with_store_and_definition(
    store: MemorySessionStore,
    provider: Arc<impl LlmStream + 'static>,
    definition: waltr_agent::ResolvedAgent,
) -> TestMcpSessions {
    let root = tempfile::tempdir().expect("tempdir").keep();
    let sandbox = TestSandbox::new(&root).await;
    let credentials = FileCredentialStore::new(root.join("credentials.json"));
    let factory = AgentSessionRuntimeFactory::new(sandbox.handle(), root, credentials, definition)
        .with_runtime_provider(move |_| {
            Ok(AgentRuntimeProvider::new(test_model(), provider.clone()))
        });
    let connections = Arc::new(Mutex::new(Vec::new()));
    let resolver_connections = Arc::clone(&connections);
    let factory = factory.with_mcp_connection_resolver(move |_| {
        let resolved = resolver_connections
            .lock()
            .expect("MCP connections lock")
            .clone();
        Box::pin(async move { Ok(resolved) })
    });
    TestMcpSessions {
        sessions: SessionCoordinator::new(store).with_runtime_factory(factory),
        connections,
        _sandbox: sandbox,
    }
}

async fn run_session_prompt(
    sessions: &TestMcpSessions,
    prompt: &str,
    connections: Vec<McpResolvedConnection>,
) -> waltr_agent::Result<AgentTestSession> {
    sessions.set_connections(connections);
    run_session_prompt_with_current_connections(sessions, prompt).await
}

async fn run_session_prompt_with_current_connections(
    sessions: &TestMcpSessions,
    prompt: &str,
) -> waltr_agent::Result<AgentTestSession> {
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;
    let submission = sessions
        .submit_prompt(
            session_id.clone(),
            prompt,
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?;
    submission.handle.wait().await?;
    let events = sessions
        .timeline(&session_id, TimelineQuery::forward(None, 500))
        .await?;
    Ok(test_session_from_timeline(events))
}

fn test_inference_settings() -> InferenceSettings {
    InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: OPENAI_TEST_MODEL.to_owned(),
            model_cost: None,
            headers: std::collections::BTreeMap::default(),
            oauth: None,
            reasoning_effort: None,
        },
    }
}

fn test_session_from_timeline(timeline: Vec<AgentTimelineEvent>) -> AgentTestSession {
    let mut session = AgentTestSession::default();
    let mut saw_tool_result = false;
    for event in timeline {
        match &event.content {
            TimelineContent::TextDelta { delta }
                if crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT) =>
            {
                if saw_tool_result && !delta.is_empty() {
                    session.saw_text_after_tool_result = true;
                }
                session.text.push_str(delta);
            }
            TimelineContent::ToolCall { call }
                if crate::session::channel_is(&event, CHANNEL_TOOL_CALL) =>
            {
                session.tool_calls.push(call.clone());
            }
            TimelineContent::ToolResult { result }
                if crate::session::channel_is(&event, CHANNEL_TOOL_RESULT) =>
            {
                session.tool_results.push(result.clone());
                saw_tool_result = true;
                if result.content.starts_with("Tool error:") {
                    session.tool_errors.push(result.clone());
                }
            }
            TimelineContent::ExternalRequest { request } => {
                session.external_requests.push(request.clone());
            }
            TimelineContent::Empty
                if event.kind == TimelineEventKind::SessionStatusChanged
                    && event.status == Some(TimelineStatus::Finished) =>
            {
                session.finished = true;
            }
            TimelineContent::Empty
            | TimelineContent::TextDelta { .. }
            | TimelineContent::TextSnapshot { .. }
            | TimelineContent::ContentRef { .. }
            | TimelineContent::Json { .. }
            | TimelineContent::ToolCall { .. }
            | TimelineContent::ToolResult { .. } => {}
        }
        session.timeline.push(event);
    }
    session
}

fn captured_system_prompt(provider: &CapturingProvider<impl LlmStream>) -> String {
    provider
        .system_prompts()
        .into_iter()
        .next()
        .flatten()
        .unwrap_or_default()
}

fn direct_events(step: usize) -> Vec<waltr_agent::Result<CompletionEvent>> {
    match step {
        0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
            "call_direct_stdio",
            "mcp_stdio_test_echo_marker",
            json!({ "value": "stdio-mcp-tool-called-707" }),
        )))],
        1 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
            "call_direct_stdio_log",
            "mcp_stdio_test_emit_log_marker",
            json!({ "message": "stdio-mcp-log-emitted-818" }),
        )))],
        2 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
            "call_direct_http",
            "mcp_http_test_echo_marker",
            json!({ "value": "http-mcp-tool-called-707" }),
        )))],
        3 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
            "call_direct_http_log",
            "mcp_http_test_emit_log_marker",
            json!({ "message": "http-mcp-log-emitted-818" }),
        )))],
        _ => vec![
            Ok(CompletionEvent::Delta("mcp-flow-complete".to_owned())),
            Ok(CompletionEvent::Finished),
        ],
    }
}

fn assert_tool_was_surfaced(provider: &CapturingProvider<impl LlmStream>, name: &str) {
    let tools = provider.all_tool_names();
    assert!(
        tools.iter().any(|tool| tool == name),
        "{}",
        provider.tool_context()
    );
}

fn assert_tool_was_not_surfaced(provider: &CapturingProvider<impl LlmStream>, name: &str) {
    let tools = provider.all_tool_names();
    assert!(
        !tools.iter().any(|tool| tool == name),
        "{}",
        provider.tool_context()
    );
}

fn test_stdio_server_command() -> String {
    if let Some(command) = option_env!("CARGO_BIN_EXE_waltr-agent-test-mcp-server") {
        return command.to_owned();
    }
    let mut path = std::env::current_exe().expect("current test executable path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("waltr-agent-test-mcp-server");
    path.display().to_string()
}

#[derive(Debug)]
struct HttpMcpTestServer {
    endpoint: String,
    child: Child,
}

impl HttpMcpTestServer {
    async fn start() -> waltr_agent::Result<Self> {
        Self::start_with_optional_instructions(None).await
    }

    async fn start_with_instructions(instructions: &str) -> waltr_agent::Result<Self> {
        Self::start_with_optional_instructions(Some(instructions)).await
    }

    async fn start_requiring_bearer(token: &str) -> waltr_agent::Result<Self> {
        Self::start_with_options(None, Some(token)).await
    }

    async fn start_with_optional_instructions(
        instructions: Option<&str>,
    ) -> waltr_agent::Result<Self> {
        Self::start_with_options(instructions, None).await
    }

    async fn start_with_options(
        instructions: Option<&str>,
        required_bearer: Option<&str>,
    ) -> waltr_agent::Result<Self> {
        let temp = tempfile::tempdir()?;
        let address_file = temp.path().join("mcp-http-address.txt");
        let mut command = std::process::Command::new(test_stdio_server_command());
        command
            .arg("--http")
            .arg("127.0.0.1:0")
            .arg(&address_file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(instructions) = instructions {
            command.env("WALTR_TEST_MCP_INSTRUCTIONS", instructions);
        }
        if let Some(required_bearer) = required_bearer {
            command.env("WALTR_TEST_MCP_REQUIRED_BEARER", required_bearer);
        }
        let mut child = command.spawn()?;
        let address = wait_for_http_address(&mut child, &address_file).await?;
        Ok(Self {
            endpoint: format!("http://{address}/mcp"),
            child,
        })
    }
}

impl Drop for HttpMcpTestServer {
    fn drop(&mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

async fn wait_for_http_address(
    child: &mut Child,
    address_file: &std::path::Path,
) -> waltr_agent::Result<String> {
    for _ in 0..100 {
        if let Some(status) = child.try_wait()? {
            return Err(waltr_agent::LlmError::api(
                "mcp",
                format!("MCP HTTP test server exited before startup: {status}"),
            ));
        }
        if let Ok(address) = fs::read_to_string(address_file) {
            let address = address.trim().to_owned();
            if !address.is_empty() {
                return Ok(address);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(waltr_agent::LlmError::api(
        "mcp",
        "MCP HTTP test server did not report an address",
    ))
}

fn tool_result_text(session: &AgentTestSession) -> String {
    session
        .tool_results
        .iter()
        .map(|result| result.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_tool_call(session: &AgentTestSession, name: &str, context: &str) {
    assert!(
        session.tool_calls.iter().any(|call| call.name == name),
        "{context}"
    );
    assert!(
        session
            .tool_results
            .iter()
            .any(|result| result.name == name),
        "{context}"
    );
}

#[derive(Debug)]
struct TestTraceCapture {
    exporter: InMemorySpanExporter,
    provider: SdkTracerProvider,
    _guard: DefaultGuard,
}

impl TestTraceCapture {
    fn start() -> Self {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("waltr-agent-mcp-test");
        let subscriber =
            Registry::default().with(tracing_opentelemetry::layer().with_tracer(tracer));
        let guard = tracing::subscriber::set_default(subscriber);

        Self {
            exporter,
            provider,
            _guard: guard,
        }
    }

    fn finished_spans(&self) -> Vec<SpanData> {
        drop(self.provider.force_flush());
        self.exporter.get_finished_spans().unwrap_or_default()
    }
}

fn find_span<'a>(
    spans: &'a [SpanData],
    name: &str,
    predicate: impl Fn(&SpanData) -> bool,
) -> &'a SpanData {
    spans
        .iter()
        .find(|span| span.name == name && predicate(span))
        .unwrap_or_else(|| panic!("expected {name} span: {spans:#?}"))
}

fn is_descendant(spans: &[SpanData], child: &SpanData, ancestor: &SpanData) -> bool {
    let parents = spans
        .iter()
        .map(|span| (span.span_context.span_id(), span.parent_span_id))
        .collect::<HashMap<_, _>>();
    let mut parent = child.parent_span_id;
    while parent != opentelemetry::trace::SpanId::INVALID {
        if parent == ancestor.span_context.span_id() {
            return true;
        }
        let Some(next_parent) = parents.get(&parent) else {
            return false;
        };
        parent = *next_parent;
    }
    false
}

fn shares_trace_with_parent(
    spans: &[SpanData],
    child: &SpanData,
    expected_parent: &SpanData,
) -> bool {
    spans.iter().any(|span| {
        span.span_context.trace_id() == expected_parent.span_context.trace_id()
            && span.span_context.span_id() == child.parent_span_id
    })
}

fn test_model() -> ModelRef {
    ModelRef::new(
        ProviderId::OpenAi,
        OPENAI_TEST_MODEL,
        ModelApi::OpenAiResponses,
    )
}
