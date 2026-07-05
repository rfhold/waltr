//! MCP agent integration tests.

use std::{fs, process::Child, sync::Arc, time::Duration};

use futures_util::StreamExt;
use waltr_agent::{
    AgentRuntime, AgentSessionRuntimeFactory, AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT,
    CHANNEL_TOOL_CALL, CHANNEL_TOOL_PROGRESS_INFO, CHANNEL_TOOL_RESULT, CompletionEvent,
    FileCredentialStore, LlmStream, MemorySessionStore, ModelApi, ModelRef, PendingWorkStatus,
    ProviderId, SessionCoordinator, SessionMcpServerRuntimeKind, SessionMcpServerRuntimeSettings,
    SessionProviderRuntimeSettings, SessionRuntimeSettings, SessionStore, SessionTemplateKind,
    SessionTemplateSearchRequest, SessionTemplateSearchTarget, Suspension, TimelineContent,
    TimelineEventKind, TimelineQuery, TimelineStatus, ToolCall,
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::scripted_provider::{CapturingProvider, ScriptedProvider};
use crate::session::AgentTestSession;

const OPENAI_TEST_MODEL: &str = "gpt-5-nano";
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

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
async fn mcp_agent_loop_discovers_and_calls_rmcp_stdio_and_streamable_http_direct_tools()
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
                && event.parent_id.as_ref().map(|id| id.as_str())
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
        if crate::session::channel_is(&event, CHANNEL_TOOL_RESULT) {
            panic!("MCP tool finished before cancellation: {event:#?}");
        }
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

fn stdio_runtime_server(command: &str) -> SessionMcpServerRuntimeSettings {
    SessionMcpServerRuntimeSettings {
        key: "stdio_test".to_owned(),
        kind: SessionMcpServerRuntimeKind::Stdio,
        command: command.to_owned(),
        url: String::new(),
    }
}

fn http_runtime_server(server: &HttpMcpTestServer) -> SessionMcpServerRuntimeSettings {
    SessionMcpServerRuntimeSettings {
        key: "http_test".to_owned(),
        kind: SessionMcpServerRuntimeKind::StreamableHttp,
        command: String::new(),
        url: server.endpoint.clone(),
    }
}

fn mcp_sessions(provider: Arc<impl LlmStream + 'static>) -> SessionCoordinator<MemorySessionStore> {
    mcp_sessions_with_store(MemorySessionStore::new(), provider)
}

fn mcp_sessions_with_store(
    store: MemorySessionStore,
    provider: Arc<impl LlmStream + 'static>,
) -> SessionCoordinator<MemorySessionStore> {
    let root = tempfile::tempdir().expect("tempdir").keep();
    let credentials = FileCredentialStore::new(root.join("credentials.json"));
    let factory = AgentSessionRuntimeFactory::new(root, credentials)
        .with_runtime_builder(move |_| Ok(AgentRuntime::builder(test_model(), provider.clone())));
    SessionCoordinator::new(store).with_runtime_factory(factory)
}

async fn run_session_prompt(
    sessions: &SessionCoordinator<MemorySessionStore>,
    prompt: &str,
    mcp_servers: Vec<SessionMcpServerRuntimeSettings>,
) -> waltr_agent::Result<AgentTestSession> {
    let session_id = sessions.create().await?.id().clone();
    sessions
        .set_runtime_settings(&session_id, Some(test_runtime_settings(mcp_servers)))
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
            headers: Default::default(),
            oauth: None,
            reasoning_effort: None,
        },
        mcp_servers,
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
        let temp = tempfile::tempdir()?;
        let address_file = temp.path().join("mcp-http-address.txt");
        let mut child = std::process::Command::new(test_stdio_server_command())
            .arg("--http")
            .arg("127.0.0.1:0")
            .arg(&address_file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
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

fn test_model() -> ModelRef {
    ModelRef::new(
        ProviderId::OpenAi,
        OPENAI_TEST_MODEL,
        ModelApi::OpenAiResponses,
    )
}
