//! MCP agent integration tests.

use std::{
    collections::HashMap,
    fs,
    process::Child,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use waltr_agent::{
    AgentRuntime, AgentSessionRunError, AgentSessionRuntimeError, AgentSessionRuntimeFactory,
    AgentTimelineEvent, AuthTarget, CHANNEL_ASSISTANT_TEXT, CHANNEL_TOOL_CALL,
    CHANNEL_TOOL_PROGRESS_INFO, CHANNEL_TOOL_RESULT, CompletionEvent, FileCredentialStore,
    LlmStream, MemorySessionStore, ModelApi, ModelRef, PendingWorkStatus, ProviderId,
    SessionCoordinator, SessionMcpServerRuntimeKind, SessionMcpServerRuntimeSettings,
    SessionProviderRuntimeSettings, SessionRuntimeSettings, SessionStore, SessionTemplateKind,
    SessionTemplateSearchRequest, SessionTemplateSearchTarget, Suspension, TimelineContent,
    TimelineEventKind, TimelineQuery, TimelineStatus, ToolCall,
};
use opentelemetry::{global, trace::TracerProvider as _};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    trace::{InMemorySpanExporter, SdkTracerProvider, SpanData},
};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Notify, Semaphore},
};
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::{Registry, prelude::*};

use crate::scripted_provider::{CapturingProvider, ScriptedProvider};
use crate::session::AgentTestSession;

const OPENAI_TEST_MODEL: &str = "gpt-5-nano";
const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

#[tokio::test]
async fn test_mcp_server_advertises_logging_and_accepts_set_level() -> waltr_agent::Result<()> {
    let temp = tempfile::tempdir()?;
    let level_file = temp.path().join("mcp-log-level.txt");
    let mut child = tokio::process::Command::new(test_stdio_server_command())
        .env("WALTR_TEST_MCP_LOG_LEVEL_FILE", &level_file)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout).lines();

    write_json_rpc(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "waltr-agent-test", "version": "0" }
            }
        }),
    )
    .await?;
    let initialize = read_json_rpc(&mut stdout).await?;
    assert_eq!(initialize["id"], json!(1));
    assert_eq!(initialize["result"]["capabilities"]["logging"], json!({}));

    write_json_rpc(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )
    .await?;
    write_json_rpc(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "logging/setLevel",
            "params": { "level": "warning" }
        }),
    )
    .await?;
    let set_level = read_json_rpc(&mut stdout).await?;
    assert_eq!(set_level["id"], json!(2));
    assert!(set_level.get("error").is_none(), "{set_level:#?}");
    assert_eq!(fs::read_to_string(&level_file)?, "warning");

    child.kill().await.ok();
    child.wait().await.ok();
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

    let sessions = mcp_sessions(provider.clone());
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
async fn mcp_streamable_http_auth_failure_is_reported_as_auth_required() -> waltr_agent::Result<()> {
    let http_server = HttpMcpTestServer::start_requiring_bearer("valid-token").await?;
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_auth_required_provider",
        vec![vec![CompletionEvent::Finished]],
    ));
    let sessions = mcp_sessions(provider);
    let session_id = sessions.create().await?.id().clone();
    sessions
        .set_runtime_settings(
            &session_id,
            Some(test_runtime_settings(vec![http_runtime_server(
                &http_server,
            )])),
        )
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
            AgentSessionRunError::Runtime(AgentSessionRuntimeError::AuthorizationRequired {
                target: AuthTarget::McpServer { ref key },
                ..
            }) if key == "http_test"
        ),
        "unexpected error: {error:?}"
    );

    Ok(())
}

#[tokio::test]
async fn mcp_shutdown_releases_coordinator_lock_attempts_all_servers_and_propagates_failure()
-> waltr_agent::Result<()> {
    let failing = Arc::new(ShutdownProbe::new(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        true,
    ));
    let succeeding = Arc::new(ShutdownProbe::new(axum::http::StatusCode::ACCEPTED, false));
    let (failing_endpoint, failing_task) = start_shutdown_probe(failing.clone()).await?;
    let (succeeding_endpoint, succeeding_task) = start_shutdown_probe(succeeding.clone()).await?;
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_shutdown_provider",
        vec![vec![CompletionEvent::Finished]],
    ));
    let sessions = mcp_sessions(provider);
    let session_id = sessions.create().await?.id().clone();
    sessions
        .set_runtime_settings(
            &session_id,
            Some(test_runtime_settings(vec![
                http_runtime_endpoint("a_failing", failing_endpoint),
                http_runtime_endpoint("b_succeeding", succeeding_endpoint),
            ])),
        )
        .await?;
    let submission = sessions
        .submit_prompt(
            session_id.clone(),
            "start MCP runtimes",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?;
    submission.handle.wait().await?;

    let shutdown_sessions = sessions.clone();
    let shutdown_session_id = session_id.clone();
    let shutdown = tokio::spawn(async move {
        shutdown_sessions
            .set_runtime_settings(&shutdown_session_id, None)
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), failing.delete_started.notified())
        .await
        .expect("first MCP shutdown starts");

    tokio::time::timeout(
        Duration::from_secs(1),
        sessions.set_runtime_settings(&session_id, None),
    )
    .await
    .expect("coordinator MCP runtime lock is released before shutdown")?;
    failing.release_delete.add_permits(1);

    let error = shutdown
        .await
        .expect("shutdown task completes")
        .expect_err("failed MCP shutdown is propagated");
    assert!(
        error.to_string().contains("500"),
        "unexpected error: {error}"
    );
    assert_eq!(failing.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(succeeding.deletes.load(Ordering::SeqCst), 1);

    failing_task.abort();
    succeeding_task.abort();
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
    let sessions = mcp_sessions(provider);
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
    assert!(
        session.timeline.iter().any(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_PROGRESS_INFO)
                && event
                    .parent_id
                    .as_ref()
                    .map(waltr_agent::TimelineItemId::as_str)
                    == Some("tool-call:call_direct_progress")
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
async fn mcp_trace_propagation_disabled_by_default() -> waltr_agent::Result<()> {
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
    let sessions = mcp_sessions(provider);

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
    let sessions = mcp_sessions(provider);
    let mut settings = test_runtime_settings(vec![stdio_runtime_server(&stdio_command)]);
    settings.mcp_trace_propagation = true;

    let session = run_session_prompt_with_settings(
        &sessions,
        "call MCP tool with trace propagation",
        settings,
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
    let sessions = mcp_sessions(provider.clone());
    let session_id = sessions.create().await?.id().clone();
    let mut settings = test_runtime_settings(vec![stdio_runtime_server(&stdio_command)]);
    settings.mcp_trace_propagation = true;
    sessions
        .set_runtime_settings(&session_id, Some(settings))
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
    let sessions = mcp_sessions(provider);
    let mut settings = test_runtime_settings(vec![stdio_runtime_server(&stdio_command)]);
    settings.mcp_trace_propagation = true;

    run_session_prompt_with_settings(&sessions, "call MCP tool for span assertions", settings)
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
    let sessions = mcp_sessions_with_store(store.clone(), provider);
    let session_id = sessions.create().await?.id().clone();
    sessions
        .set_runtime_settings(
            &session_id,
            Some(test_runtime_settings(vec![stdio_runtime_server(
                &stdio_command,
            )])),
        )
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
    let recovery_sessions = mcp_sessions(recovery_provider.clone());
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
    let sessions = mcp_sessions(provider);
    let session_id = sessions.create().await?.id().clone();
    sessions
        .set_runtime_settings(
            &session_id,
            Some(test_runtime_settings(vec![stdio_runtime_server(
                &stdio_command,
            )])),
        )
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
    let sessions = mcp_sessions(provider.clone());
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
    let sessions = mcp_sessions(provider.clone());
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
    let sessions = mcp_sessions(provider);
    let session_id = sessions.create().await?.id().clone();
    sessions
        .set_runtime_settings(
            &session_id,
            Some(test_runtime_settings(vec![stdio_runtime_server(
                &stdio_command,
            )])),
        )
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
    let sessions = mcp_sessions(provider);
    let session_id = sessions.create().await?.id().clone();
    sessions
        .set_runtime_settings(
            &session_id,
            Some(test_runtime_settings(vec![stdio_runtime_server(
                &stdio_command,
            )])),
        )
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
    let sessions = mcp_sessions(provider);
    let session_id = sessions.create().await?.id().clone();
    sessions
        .set_runtime_settings(
            &session_id,
            Some(test_runtime_settings(vec![stdio_runtime_server(
                &stdio_command,
            )])),
        )
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
    let sessions = mcp_sessions(provider.clone());
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
    let sessions = mcp_sessions(provider.clone());
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
async fn mcp_resources_are_not_surfaced_as_provider_tools() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_resource_tool_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let sessions = mcp_sessions(provider.clone());

    run_session_prompt(
        &sessions,
        "resources are templates only",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;

    assert_tool_was_not_surfaced(&provider, "mcp_stdio_test_status_resource");
    assert_tool_was_not_surfaced(&provider, "read_mcp_resource");

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
    let sessions = mcp_sessions(provider.clone());

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
    let sessions = mcp_sessions(provider.clone());

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
async fn mcp_server_instructions_can_be_disabled_on_factory() -> waltr_agent::Result<()> {
    let server = HttpMcpTestServer::start_with_instructions("disabled instructions marker").await?;
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_disabled_instructions_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let sessions = mcp_sessions_with_instructions_enabled(provider.clone(), false);

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

fn stdio_runtime_server(command: &str) -> SessionMcpServerRuntimeSettings {
    SessionMcpServerRuntimeSettings {
        key: "stdio_test".to_owned(),
        kind: SessionMcpServerRuntimeKind::Stdio,
        command: command.to_owned(),
        url: String::new(),
    }
}

fn http_runtime_server(server: &HttpMcpTestServer) -> SessionMcpServerRuntimeSettings {
    http_runtime_server_with_key("http_test", server)
}

fn http_runtime_server_with_key(
    key: &str,
    server: &HttpMcpTestServer,
) -> SessionMcpServerRuntimeSettings {
    SessionMcpServerRuntimeSettings {
        key: key.to_owned(),
        kind: SessionMcpServerRuntimeKind::StreamableHttp,
        command: String::new(),
        url: server.endpoint.clone(),
    }
}

fn http_runtime_endpoint(key: &str, endpoint: String) -> SessionMcpServerRuntimeSettings {
    SessionMcpServerRuntimeSettings {
        key: key.to_owned(),
        kind: SessionMcpServerRuntimeKind::StreamableHttp,
        command: String::new(),
        url: endpoint,
    }
}

fn mcp_sessions(provider: Arc<impl LlmStream + 'static>) -> SessionCoordinator<MemorySessionStore> {
    mcp_sessions_with_instructions_enabled(provider, true)
}

fn mcp_sessions_with_instructions_enabled(
    provider: Arc<impl LlmStream + 'static>,
    instructions_enabled: bool,
) -> SessionCoordinator<MemorySessionStore> {
    mcp_sessions_with_store_and_instructions_enabled(
        MemorySessionStore::new(),
        provider,
        instructions_enabled,
    )
}

fn mcp_sessions_with_store(
    store: MemorySessionStore,
    provider: Arc<impl LlmStream + 'static>,
) -> SessionCoordinator<MemorySessionStore> {
    mcp_sessions_with_store_and_instructions_enabled(store, provider, true)
}

fn mcp_sessions_with_store_and_instructions_enabled(
    store: MemorySessionStore,
    provider: Arc<impl LlmStream + 'static>,
    instructions_enabled: bool,
) -> SessionCoordinator<MemorySessionStore> {
    let root = tempfile::tempdir().expect("tempdir").keep();
    let credentials = FileCredentialStore::new(root.join("credentials.json"));
    let factory = AgentSessionRuntimeFactory::new(root, credentials)
        .with_mcp_server_instructions_enabled(instructions_enabled)
        .with_runtime_builder(move |_| Ok(AgentRuntime::builder(test_model(), provider.clone())));
    SessionCoordinator::new(store).with_runtime_factory(factory)
}

async fn run_session_prompt(
    sessions: &SessionCoordinator<MemorySessionStore>,
    prompt: &str,
    mcp_servers: Vec<SessionMcpServerRuntimeSettings>,
) -> waltr_agent::Result<AgentTestSession> {
    run_session_prompt_with_settings(sessions, prompt, test_runtime_settings(mcp_servers)).await
}

async fn run_session_prompt_with_settings(
    sessions: &SessionCoordinator<MemorySessionStore>,
    prompt: &str,
    settings: SessionRuntimeSettings,
) -> waltr_agent::Result<AgentTestSession> {
    let session_id = sessions.create().await?.id().clone();
    sessions
        .set_runtime_settings(&session_id, Some(settings))
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

fn test_runtime_settings(
    mcp_servers: Vec<SessionMcpServerRuntimeSettings>,
) -> SessionRuntimeSettings {
    SessionRuntimeSettings {
        provider: SessionProviderRuntimeSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: OPENAI_TEST_MODEL.to_owned(),
            model_cost: None,
            headers: std::collections::BTreeMap::default(),
            oauth: None,
            reasoning_effort: None,
        },
        mcp_servers,
        mcp_trace_propagation: false,
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

async fn write_json_rpc(
    stdin: &mut tokio::process::ChildStdin,
    value: serde_json::Value,
) -> waltr_agent::Result<()> {
    stdin.write_all(value.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_json_rpc(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> waltr_agent::Result<serde_json::Value> {
    let line = lines.next_line().await?.ok_or_else(|| {
        waltr_agent::LlmError::api("mcp", "MCP test server closed stdout before response")
    })?;
    Ok(serde_json::from_str(&line)?)
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

#[derive(Debug)]
struct ShutdownProbe {
    deletes: AtomicUsize,
    delete_status: axum::http::StatusCode,
    block_delete: bool,
    delete_started: Notify,
    release_delete: Semaphore,
}

impl ShutdownProbe {
    fn new(delete_status: axum::http::StatusCode, block_delete: bool) -> Self {
        Self {
            deletes: AtomicUsize::new(0),
            delete_status,
            block_delete,
            delete_started: Notify::new(),
            release_delete: Semaphore::new(0),
        }
    }
}

async fn start_shutdown_probe(
    probe: Arc<ShutdownProbe>,
) -> waltr_agent::Result<(String, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let router = axum::Router::new()
        .route("/mcp", axum::routing::any(shutdown_probe))
        .with_state(probe);
    let task = tokio::spawn(async move {
        drop(axum::serve(listener, router).await);
    });
    Ok((format!("http://{address}/mcp"), task))
}

async fn shutdown_probe(
    axum::extract::State(probe): axum::extract::State<Arc<ShutdownProbe>>,
    request: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    match *request.method() {
        axum::http::Method::DELETE => {
            probe.deletes.fetch_add(1, Ordering::SeqCst);
            probe.delete_started.notify_one();
            if probe.block_delete {
                let permit = probe
                    .release_delete
                    .acquire()
                    .await
                    .expect("shutdown probe semaphore remains open");
                permit.forget();
            }
            probe.delete_status.into_response()
        }
        axum::http::Method::GET => {
            let stream = futures_util::stream::pending::<Result<String, std::io::Error>>();
            (
                axum::http::StatusCode::OK,
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        axum::http::Method::POST => {
            let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
                .await
                .expect("bounded probe request body");
            let message: serde_json::Value =
                serde_json::from_slice(&body).expect("probe JSON-RPC request");
            if message["method"] == "initialize" {
                return (
                    axum::http::StatusCode::OK,
                    [
                        ("content-type", "application/json"),
                        ("mcp-session-id", "shutdown-probe-session"),
                        ("mcp-protocol-version", MCP_PROTOCOL_VERSION),
                    ],
                    axum::Json(json!({
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "result": {
                            "protocolVersion": MCP_PROTOCOL_VERSION,
                            "capabilities": {},
                            "serverInfo": { "name": "shutdown-probe", "version": "1" }
                        }
                    })),
                )
                    .into_response();
            }
            axum::http::StatusCode::ACCEPTED.into_response()
        }
        _ => axum::http::StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
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
