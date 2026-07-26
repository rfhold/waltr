//! `AgentRuntime` permission integration tests.

use std::{env, fs, path::PathBuf, sync::Arc};

use futures_util::{Stream, StreamExt, stream};
use waltr_agent::{
    AgentRuntime, AgentSessionRuntime, AgentTimelineEvent, BoxCompletionStream, BoxStreamFuture,
    CHANNEL_ASSISTANT_TEXT, CHANNEL_EXTERNAL_REQUEST, CHANNEL_TOOL_RESULT, CancelReason,
    CompletionEvent, CompletionRequest, ExternalRequestId, ExternalRequestKind,
    ExternalRequestStatus, ExternalResponse, LlmStream, MemorySessionStore, ModelApi, ModelRef,
    PendingWorkStatus, PermissionEffect, PermissionOperation, PermissionPattern, PermissionPolicy,
    PermissionResourcePattern, PermissionResponse, PermissionToolSelector, ProviderId, ResumeInput,
    RuntimeResumeOutput, SessionCoordinator, SessionStatus, SessionStore, TimelineContent,
    ToolCall,
    tools::filesystem::{FilesystemConfig, FilesystemToolProfile},
};
#[cfg(feature = "shell")]
use waltr_agent::{PermissionCommandPattern, tools::shell::ShellConfig};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::scripted_provider::{CapturingProvider, ScriptedProvider};

const OPENAI_TEST_MODEL: &str = "gpt-5-nano";
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn filesystem_permissions_allow_root_and_suspend_for_external_access()
-> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let policy =
        PermissionPolicy::filesystem_external(fixture.root.path(), PermissionEffect::ask());
    #[cfg(feature = "shell")]
    let policy = policy
        .with_command_rule(
            PermissionToolSelector::exact("shell"),
            PermissionCommandPattern::Name("printf".to_owned()),
            PermissionEffect::ask(),
        )
        .with_command_rule(
            PermissionToolSelector::exact("shell"),
            PermissionCommandPattern::Name("cat".to_owned()),
            PermissionEffect::allow(),
        )
        .with_command_rule(
            PermissionToolSelector::exact("shell"),
            PermissionCommandPattern::Name("git".to_owned()),
            PermissionEffect::deny_with_message("do not interact with git"),
        );
    let provider = Arc::new(ScriptedPermissionProvider);
    let agent = AgentRuntime::builder(test_model(ModelApi::OpenAiResponses), provider)
        .with_permission_policy(policy)
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        );
    #[cfg(feature = "shell")]
    let agent = agent.with_shell_tool(ShellConfig::new(fixture.root.path()));
    let agent = agent.build();
    #[cfg(not(feature = "shell"))]
    let prompt = "exercise filesystem permission allow, ask, allow-always, and deny".to_owned();
    #[cfg(feature = "shell")]
    let prompt = format!(
        "exercise filesystem permissions, use shell printf twice, use shell cat to read {}, use shell cat to attempt denied {}, then attempt git status",
        fixture
            .outside
            .path()
            .join("nested/shell-walk.txt")
            .display(),
        fixture.outside.path().join("denied.txt").display(),
    );

    let mut stream = agent.prompt(&prompt).stream();
    let mut results = Vec::new();
    let mut external_requests = Vec::new();

    while let Some(event) = stream.next().await {
        let event = event?;
        match &event.content {
            TimelineContent::ToolResult { result }
                if crate::session::channel_is(&event, CHANNEL_TOOL_RESULT) =>
            {
                results.push(result.content.clone());
            }
            TimelineContent::ExternalRequest { request }
                if crate::session::channel_is(&event, CHANNEL_EXTERNAL_REQUEST) =>
            {
                external_requests.push(request.clone());
            }
            TimelineContent::Empty
            | TimelineContent::TextDelta { .. }
            | TimelineContent::TextSnapshot { .. }
            | TimelineContent::ToolCall { .. }
            | TimelineContent::ToolResult { .. }
            | TimelineContent::ExternalRequest { .. }
            | TimelineContent::ContentRef { .. }
            | TimelineContent::Json { .. } => {}
        }
    }

    assert!(
        results
            .iter()
            .any(|result| result.contains("root-allowed-101")),
        "{results:#?}"
    );
    let [request] = external_requests.as_slice() else {
        panic!("expected one permission external request: {external_requests:#?}");
    };
    assert_eq!(request.kind, ExternalRequestKind::Permission);
    assert_eq!(request.tool_call_id.as_deref(), Some("call_2"));
    assert!(request.prompt.contains("external.txt"), "{request:#?}");
    let patterns = serde_json::from_value::<Vec<PermissionPattern>>(
        request
            .payload
            .get("patterns")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )?;
    assert!(
        patterns.contains(&PermissionPattern {
            operation: Some(PermissionOperation::Read),
            resource: PermissionResourcePattern::FilesystemPathPrefix(fixture.outside.path()),
        }),
        "permission request should include directory-wide outside approval pattern: {patterns:#?}"
    );
    assert!(
        request
            .id
            .as_str()
            .starts_with("permission:ephemeral:1:call_2")
    );

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn permission_external_request_resumes_from_durable_session() -> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let provider = Arc::new(ScriptedProvider::new(
        "permission_resume_provider",
        |request| {
            if request.tool_results().any(|result| {
                result.call_id == "call_resume" && result.content.contains("external-once-202")
            }) {
                vec![
                    Ok(CompletionEvent::Delta(
                        "permission-flow-complete".to_owned(),
                    )),
                    Ok(CompletionEvent::Finished),
                ]
            } else {
                vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_resume",
                    "../outside/external.txt",
                )))]
            }
        },
    ));
    let agent = AgentRuntime::builder(test_model(ModelApi::OpenAiResponses), provider)
        .with_permission_policy(PermissionPolicy::filesystem_external(
            fixture.root.path(),
            PermissionEffect::ask(),
        ))
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        )
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions.create().await?.id().clone();

    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "read external file")
        .stream();
    let mut request_id = None;
    while let Some(event) = stream.next().await {
        let event = event?;
        if let TimelineContent::ExternalRequest { request } = &event.content {
            assert!(crate::session::channel_is(&event, CHANNEL_EXTERNAL_REQUEST));
            assert_eq!(request.kind, ExternalRequestKind::Permission);
            assert_eq!(request.tool_call_id.as_deref(), Some("call_resume"));
            assert!(request.prompt.contains("external.txt"), "{request:#?}");
            request_id = Some(request.id.as_str().to_owned());
        }
    }
    let request_id = request_id.expect("permission request");

    let suspended = store.load(&session_id).await.expect("stored session");
    let suspended_snapshot = suspended.snapshot();
    let persisted_request = suspended_snapshot
        .external_requests
        .iter()
        .find(|request| request.id.as_str() == request_id)
        .expect("persisted permission request");
    assert_eq!(persisted_request.kind, ExternalRequestKind::Permission);
    assert_eq!(persisted_request.status, ExternalRequestStatus::Pending);
    assert!(matches!(
        suspended_snapshot.status,
        SessionStatus::Suspended(_)
    ));
    assert!(suspended_snapshot.pending_work.iter().any(|work| {
        work.id == "call_resume" && matches!(work.status, PendingWorkStatus::Pending)
    }));

    let mut resumed = sessions
        .resume(
            &agent,
            session_id.clone(),
            ResumeInput::ExternalResponse {
                request_id: ExternalRequestId::new(request_id.clone()),
                response: ExternalResponse::new(json!("allow-once")),
            },
        )
        .stream();
    let mut text = String::new();
    let mut tool_results = Vec::new();
    while let Some(event) = resumed.next().await {
        let event = event?;
        if let TimelineContent::TextDelta { delta } = &event.content
            && crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT)
        {
            text.push_str(delta);
        }
        if let TimelineContent::ToolResult { result } = &event.content
            && crate::session::channel_is(&event, CHANNEL_TOOL_RESULT)
        {
            tool_results.push(result.clone());
        }
    }
    assert!(
        tool_results.iter().any(|result| {
            result.call_id == "call_resume" && result.content.contains("external-once-202")
        }),
        "permission approval must run the original tool call, got {tool_results:#?}"
    );
    assert_eq!(text, "permission-flow-complete");

    let completed = store.load(&session_id).await.expect("stored session");
    let completed_snapshot = completed.snapshot();
    let responded_request = completed_snapshot
        .external_requests
        .iter()
        .find(|request| request.id.as_str() == request_id)
        .expect("responded permission request");
    assert!(matches!(
        responded_request.status,
        ExternalRequestStatus::Responded(_)
    ));
    assert!(completed_snapshot.pending_work.iter().any(|work| {
        work.id == "call_resume" && matches!(work.status, PendingWorkStatus::Finished(_))
    }));

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn permission_resume_preserves_remaining_provider_tool_calls() -> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let provider = Arc::new(ScriptedProvider::new(
        "permission_multi_tool_resume_provider",
        |request| {
            let completed = request
                .tool_results()
                .map(|result| result.call_id.as_str())
                .collect::<Vec<_>>();
            if completed.contains(&"call_first") && completed.contains(&"call_second") {
                vec![
                    Ok(CompletionEvent::Delta("all-tools-complete".to_owned())),
                    Ok(CompletionEvent::Finished),
                ]
            } else {
                vec![
                    Ok(CompletionEvent::ToolCall(read_call(
                        "call_first",
                        "../outside/external.txt",
                    ))),
                    Ok(CompletionEvent::ToolCall(read_call(
                        "call_second",
                        "../outside/nested/another.txt",
                    ))),
                ]
            }
        },
    ));
    let agent = AgentRuntime::builder(test_model(ModelApi::OpenAiResponses), provider)
        .with_permission_policy(PermissionPolicy::filesystem_external(
            fixture.root.path(),
            PermissionEffect::ask(),
        ))
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        )
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions.create().await?.id().clone();

    let request_id = first_permission_request_id(
        sessions
            .prompt(
                &agent,
                session_id.clone(),
                "read two external files in one turn",
            )
            .stream(),
    )
    .await?;
    let suspended = store.load(&session_id).await.expect("stored session");
    let suspended_snapshot = suspended.snapshot();
    assert_eq!(
        suspended_snapshot
            .pending_work
            .iter()
            .filter(|work| matches!(work.status, PendingWorkStatus::Pending))
            .map(|work| work.id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_first", "call_second"]
    );

    let mut resumed = sessions
        .resume(
            &agent,
            session_id.clone(),
            ResumeInput::ExternalResponse {
                request_id,
                response: ExternalResponse::new(json!(PermissionResponse::AllowAlways {
                    patterns: vec![PermissionPattern::filesystem_prefix(
                        PermissionOperation::Read,
                        fixture.outside.path(),
                    )],
                })),
            },
        )
        .stream();
    let mut text = String::new();
    let mut tool_results = Vec::new();
    while let Some(event) = resumed.next().await {
        let event = event?;
        if let TimelineContent::TextDelta { delta } = &event.content
            && crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT)
        {
            text.push_str(delta);
        }
        if let TimelineContent::ToolResult { result } = &event.content
            && crate::session::channel_is(&event, CHANNEL_TOOL_RESULT)
        {
            tool_results.push(result.call_id.clone());
        }
    }

    assert_eq!(tool_results, vec!["call_first", "call_second"]);
    assert_eq!(text, "all-tools-complete");

    Ok(())
}

#[tokio::test]
async fn cancellation_after_permission_request_cancels_resume_path() -> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let provider = Arc::new(ScriptedProvider::new(
        "permission_cancel_resume_provider",
        |request| {
            if request.tool_results().any(|result| {
                result.call_id == "call_cancel_resume"
                    && result.content.contains("external-once-202")
            }) {
                vec![
                    Ok(CompletionEvent::Delta("resumed-after-cancel".to_owned())),
                    Ok(CompletionEvent::Finished),
                ]
            } else {
                vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_cancel_resume",
                    "../outside/external.txt",
                )))]
            }
        },
    ));
    let agent = AgentRuntime::builder(test_model(ModelApi::OpenAiResponses), provider)
        .with_permission_policy(PermissionPolicy::filesystem_external(
            fixture.root.path(),
            PermissionEffect::ask(),
        ))
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        )
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions.create().await?.id().clone();
    let cancellation = CancellationToken::new();

    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "read external file")
        .with_cancellation_token(cancellation.clone())
        .stream();
    let request_id = first_permission_request_id(&mut stream).await?;
    cancellation.cancel();
    assert!(stream.next().await.is_none());

    let suspended = store.load(&session_id).await.expect("stored session");
    let suspended_snapshot = suspended.snapshot();
    assert!(matches!(
        suspended_snapshot.status,
        SessionStatus::Suspended(_)
    ));
    assert!(suspended_snapshot.pending_work.iter().any(|work| {
        work.id == "call_cancel_resume"
            && matches!(
                work.status,
                PendingWorkStatus::Canceled(CancelReason::SessionInterrupted)
            )
    }));
    assert!(suspended_snapshot.external_requests.iter().any(|request| {
        request.id == request_id
            && matches!(
                request.status,
                ExternalRequestStatus::Canceled(CancelReason::SessionInterrupted)
            )
    }));

    let mut resumed = sessions
        .resume(
            &agent,
            session_id.clone(),
            ResumeInput::ExternalResponse {
                request_id,
                response: ExternalResponse::new(json!("allow-once")),
            },
        )
        .stream();
    let error = resumed
        .next()
        .await
        .expect("late response should fail")
        .unwrap_err();
    assert!(error.to_string().contains("canceled"));

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn permission_allow_always_patterns_resume_and_apply_to_later_calls() -> waltr_agent::Result<()>
{
    let fixture = PermissionFixture::new()?;
    let provider = Arc::new(ScriptedProvider::new(
        "permission_allow_always_provider",
        |request| {
            let has_first = request.tool_results().any(|result| {
                result.call_id == "call_first" && result.content.contains("external-once-202")
            });
            let has_second = request.tool_results().any(|result| {
                result.call_id == "call_second" && result.content.contains("external-another-404")
            });
            if has_second {
                vec![
                    Ok(CompletionEvent::Delta("allow-always-complete".to_owned())),
                    Ok(CompletionEvent::Finished),
                ]
            } else if has_first {
                vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_second",
                    "../outside/nested/another.txt",
                )))]
            } else {
                vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_first",
                    "../outside/external.txt",
                )))]
            }
        },
    ));
    let agent = AgentRuntime::builder(test_model(ModelApi::OpenAiResponses), provider)
        .with_permission_policy(PermissionPolicy::filesystem_external(
            fixture.root.path(),
            PermissionEffect::ask(),
        ))
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        )
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions.create().await?.id().clone();

    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "read two external files")
        .stream();
    let mut request_id = None;
    while let Some(event) = stream.next().await {
        let event = event?;
        if let TimelineContent::ExternalRequest { request } = &event.content {
            request_id = Some(request.id.as_str().to_owned());
        }
    }
    let request_id = request_id.expect("permission request");

    let mut resumed = sessions
        .resume(
            &agent,
            session_id.clone(),
            ResumeInput::ExternalResponse {
                request_id: ExternalRequestId::new(request_id),
                response: ExternalResponse::new(json!(PermissionResponse::AllowAlways {
                    patterns: vec![PermissionPattern::filesystem_prefix(
                        PermissionOperation::Read,
                        fixture.outside.path(),
                    )],
                })),
            },
        )
        .stream();
    let mut text = String::new();
    let mut external_requests = Vec::new();
    let mut tool_results = Vec::new();
    while let Some(event) = resumed.next().await {
        let event = event?;
        match &event.content {
            TimelineContent::TextDelta { delta }
                if crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT) =>
            {
                text.push_str(delta);
            }
            TimelineContent::ExternalRequest { request }
                if crate::session::channel_is(&event, CHANNEL_EXTERNAL_REQUEST) =>
            {
                external_requests.push(request.clone());
            }
            TimelineContent::ToolResult { result }
                if crate::session::channel_is(&event, CHANNEL_TOOL_RESULT) =>
            {
                tool_results.push(result.clone());
            }
            TimelineContent::Empty
            | TimelineContent::TextDelta { .. }
            | TimelineContent::TextSnapshot { .. }
            | TimelineContent::ToolCall { .. }
            | TimelineContent::ToolResult { .. }
            | TimelineContent::ExternalRequest { .. }
            | TimelineContent::ContentRef { .. }
            | TimelineContent::Json { .. } => {}
        }
    }

    assert!(
        external_requests.is_empty(),
        "allow-always pattern should avoid a second permission prompt: {external_requests:#?}"
    );
    assert!(
        tool_results.iter().any(|result| {
            result.call_id == "call_second" && result.content.contains("external-another-404")
        }),
        "second matching call should execute normally, got {tool_results:#?}"
    );
    assert_eq!(text, "allow-always-complete");

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn permission_allow_once_does_not_apply_to_later_matching_calls() -> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let provider = Arc::new(ScriptedProvider::new(
        "permission_allow_once_provider",
        |request| {
            if request.tool_results().any(|result| {
                result.call_id == "call_once" && result.content.contains("external-once-202")
            }) {
                vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_later",
                    "../outside/nested/another.txt",
                )))]
            } else {
                vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_once",
                    "../outside/external.txt",
                )))]
            }
        },
    ));
    let agent = AgentRuntime::builder(test_model(ModelApi::OpenAiResponses), provider)
        .with_permission_policy(PermissionPolicy::filesystem_external(
            fixture.root.path(),
            PermissionEffect::ask(),
        ))
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        )
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions.create().await?.id().clone();

    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "read external files once")
        .stream();
    let mut request_id = None;
    while let Some(event) = stream.next().await {
        let event = event?;
        if let TimelineContent::ExternalRequest { request } = &event.content {
            request_id = Some(request.id.as_str().to_owned());
        }
    }
    let request_id = request_id.expect("permission request");

    let mut resumed = sessions
        .resume(
            &agent,
            session_id.clone(),
            ResumeInput::ExternalResponse {
                request_id: ExternalRequestId::new(request_id),
                response: ExternalResponse::new(json!(PermissionResponse::AllowOnce)),
            },
        )
        .stream();
    let mut external_requests = Vec::new();
    let mut tool_results = Vec::new();
    while let Some(event) = resumed.next().await {
        let event = event?;
        match &event.content {
            TimelineContent::ExternalRequest { request }
                if crate::session::channel_is(&event, CHANNEL_EXTERNAL_REQUEST) =>
            {
                external_requests.push(request.clone());
            }
            TimelineContent::ToolResult { result }
                if crate::session::channel_is(&event, CHANNEL_TOOL_RESULT) =>
            {
                tool_results.push(result.clone());
            }
            TimelineContent::Empty
            | TimelineContent::TextDelta { .. }
            | TimelineContent::TextSnapshot { .. }
            | TimelineContent::ToolCall { .. }
            | TimelineContent::ToolResult { .. }
            | TimelineContent::ExternalRequest { .. }
            | TimelineContent::ContentRef { .. }
            | TimelineContent::Json { .. } => {}
        }
    }

    assert!(
        tool_results.iter().any(|result| {
            result.call_id == "call_once" && result.content.contains("external-once-202")
        }),
        "allow-once should execute the approved call, got {tool_results:#?}"
    );
    let [request] = external_requests.as_slice() else {
        panic!("later matching call should ask again: {external_requests:#?}");
    };
    assert_eq!(request.tool_call_id.as_deref(), Some("call_later"));

    Ok(())
}

#[tokio::test]
async fn permission_deny_can_return_tool_result() -> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let provider = Arc::new(ScriptedProvider::new(
        "permission_deny_result_provider",
        |request| {
            if request.tool_results().any(|result| {
                result.call_id == "call_deny" && result.content.contains("not this time")
            }) {
                vec![
                    Ok(CompletionEvent::Delta("denial-observed".to_owned())),
                    Ok(CompletionEvent::Finished),
                ]
            } else {
                vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_deny",
                    "../outside/external.txt",
                )))]
            }
        },
    ));
    let agent = AgentRuntime::builder(test_model(ModelApi::OpenAiResponses), provider)
        .with_permission_policy(PermissionPolicy::filesystem_external(
            fixture.root.path(),
            PermissionEffect::ask(),
        ))
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        )
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions.create().await?.id().clone();
    let request_id = first_permission_request_id(
        sessions
            .prompt(&agent, session_id.clone(), "read denied external file")
            .stream(),
    )
    .await?;

    let mut resumed = sessions
        .resume(
            &agent,
            session_id.clone(),
            ResumeInput::ExternalResponse {
                request_id,
                response: ExternalResponse::new(json!({
                    "decision": "deny",
                    "message": "not this time",
                    "mode": "return_tool_result",
                })),
            },
        )
        .stream();
    let mut text = String::new();
    while let Some(event) = resumed.next().await {
        let event = event?;
        if let TimelineContent::TextDelta { delta } = &event.content
            && crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT)
        {
            text.push_str(delta);
        }
    }

    let snapshot = store
        .load(&session_id)
        .await
        .expect("stored session")
        .snapshot();
    assert!(
        snapshot
            .tool_results()
            .any(|result| { result.call_id == "call_deny" && result.content == "not this time" }),
        "deny should return a stored denial tool result: {snapshot:#?}"
    );
    assert_eq!(text, "denial-observed");

    Ok(())
}

#[tokio::test]
async fn permission_deny_can_cancel_pending_work() -> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let provider = Arc::new(ScriptedProvider::new(
        "permission_deny_cancel_provider",
        |_| {
            vec![Ok(CompletionEvent::ToolCall(read_call(
                "call_cancel",
                "../outside/external.txt",
            )))]
        },
    ));
    let agent = AgentRuntime::builder(test_model(ModelApi::OpenAiResponses), provider)
        .with_permission_policy(PermissionPolicy::filesystem_external(
            fixture.root.path(),
            PermissionEffect::ask(),
        ))
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        )
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions.create().await?.id().clone();
    let request_id = first_permission_request_id(
        sessions
            .prompt(&agent, session_id.clone(), "read canceled external file")
            .stream(),
    )
    .await?;

    let mut resumed = sessions
        .resume(
            &agent,
            session_id.clone(),
            ResumeInput::ExternalResponse {
                request_id,
                response: ExternalResponse::new(json!({
                    "decision": "deny",
                    "message": "stop",
                    "mode": "cancel_work",
                })),
            },
        )
        .stream();
    let mut emitted = Vec::new();
    while let Some(event) = resumed.next().await {
        emitted.push(event?);
    }
    assert!(
        emitted.is_empty(),
        "cancel should not continue: {emitted:#?}"
    );

    let snapshot = store
        .load(&session_id)
        .await
        .expect("stored session")
        .snapshot();
    assert!(snapshot.pending_work.iter().any(|work| {
        work.id == "call_cancel" && matches!(work.status, PendingWorkStatus::Canceled(_))
    }));

    Ok(())
}

#[tokio::test]
async fn blanket_denied_tools_are_not_surfaced() -> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let policy = PermissionPolicy::filesystem_default(fixture.root.path())
        .with_tool_rule(
            PermissionToolSelector::exact("edit"),
            PermissionEffect::deny(),
        )
        .with_tool_rule(
            PermissionToolSelector::exact("write"),
            PermissionEffect::deny(),
        )
        .with_tool_rule(
            PermissionToolSelector::exact("delete"),
            PermissionEffect::deny(),
        )
        .with_tool_rule(
            PermissionToolSelector::exact("apply_patch"),
            PermissionEffect::deny(),
        );
    #[cfg(feature = "shell")]
    let policy = policy.with_tool_rule(
        PermissionToolSelector::exact("shell"),
        PermissionEffect::deny(),
    );
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "capture_tools_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let agent = AgentRuntime::builder(test_model(ModelApi::OpenAiResponses), provider.clone())
        .with_permission_policy(policy)
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        );
    #[cfg(feature = "shell")]
    let agent = agent.with_shell_tool(ShellConfig::new(fixture.root.path()));
    let agent = agent.build();

    let mut stream = agent.prompt("capture tools").stream();
    while let Some(event) = stream.next().await {
        drop(event?);
    }

    let tools = provider.all_tool_names();
    assert!(tools.iter().any(|name| name == "read"));
    assert!(tools.iter().any(|name| name == "glob"));
    assert!(tools.iter().any(|name| name == "grep"));
    assert!(!tools.iter().any(|name| name == "edit"));
    assert!(!tools.iter().any(|name| name == "write"));
    assert!(!tools.iter().any(|name| name == "delete"));
    assert!(!tools.iter().any(|name| name == "apply_patch"));
    #[cfg(feature = "shell")]
    assert!(!tools.iter().any(|name| name == "shell"));

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn multi_claim_ask_suspends_before_execution() -> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let denied = fixture.outside.path().join("denied.txt");
    #[cfg(feature = "shell")]
    let shell_denied = fixture.outside.path().join("shell-denied.txt");
    let provider = Arc::new(PatchDeniedProvider {
        allowed: fixture.root.path().join("allowed.txt"),
        denied,
        #[cfg(feature = "shell")]
        shell_denied: shell_denied.clone(),
    });
    let agent = AgentRuntime::builder(test_model(ModelApi::OpenAiResponses), provider)
        .with_permission_policy(PermissionPolicy::filesystem_external(
            fixture.root.path(),
            PermissionEffect::ask(),
        ))
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        );
    #[cfg(feature = "shell")]
    let agent = agent.with_shell_tool(ShellConfig::new(fixture.root.path()));
    let agent = agent.build();

    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions.create().await?.id().clone();
    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "patch denied")
        .stream();
    let mut external_requests = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event?;
        if let TimelineContent::ExternalRequest { request } = &event.content {
            assert!(crate::session::channel_is(&event, CHANNEL_EXTERNAL_REQUEST));
            external_requests.push(request.clone());
        }
    }

    let [request] = external_requests.as_slice() else {
        panic!("expected one permission external request: {external_requests:#?}");
    };
    assert_eq!(request.kind, ExternalRequestKind::Permission);
    assert_eq!(request.tool_call_id.as_deref(), Some("call_1"));
    assert!(
        request.prompt.contains("Write filesystem path"),
        "{request:#?}"
    );
    let external_request_id = request.id.clone();
    assert_eq!(fixture.root.read("allowed.txt")?, "root-allowed-101\n");
    #[cfg(feature = "shell")]
    assert!(!shell_denied.exists());

    let suspended = store.load(&session_id).await.expect("stored session");
    let suspended_snapshot = suspended.snapshot();
    assert!(
        suspended_snapshot.pending_work.iter().any(|work| {
            work.id == "call_1" && matches!(work.status, PendingWorkStatus::Pending)
        })
    );

    match AgentSessionRuntime::new(&store)
        .resume(
            &session_id,
            ResumeInput::Prompt("skip denied patch".to_owned()),
        )
        .await?
    {
        RuntimeResumeOutput::Resumed { .. } => {}
        RuntimeResumeOutput::Suspended { .. } => panic!("prompt resume should cancel pending work"),
    }

    let canceled = store.load(&session_id).await.expect("stored session");
    let canceled_snapshot = canceled.snapshot();
    let persisted_request = canceled_snapshot
        .external_requests
        .iter()
        .find(|request| request.id.as_str() == external_request_id.as_str())
        .expect("persisted permission request");
    assert!(matches!(
        persisted_request.status,
        ExternalRequestStatus::Canceled(CancelReason::SupersededByUserPrompt)
    ));
    let work = canceled_snapshot
        .pending_work
        .iter()
        .find(|work| work.id == "call_1")
        .expect("pending work record");
    assert!(matches!(
        work.status,
        PendingWorkStatus::Canceled(CancelReason::SupersededByUserPrompt)
    ));

    Ok(())
}

struct ScriptedPermissionProvider;

impl LlmStream for ScriptedPermissionProvider {
    fn name(&self) -> &'static str {
        "scripted_permission_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let completed = request
                .tool_results()
                .map(|result| result.call_id.as_str())
                .collect::<Vec<_>>();
            let events = if !completed.contains(&"call_1") {
                vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_1",
                    "allowed.txt",
                )))]
            } else if !completed.contains(&"call_2") {
                vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_2",
                    "../outside/external.txt",
                )))]
            } else if !completed.contains(&"call_3") {
                vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_3",
                    "../outside/nested/external-nested.txt",
                )))]
            } else if !completed.contains(&"call_4") {
                vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_4",
                    "../outside/nested/another.txt",
                )))]
            } else if !completed.contains(&"call_5") {
                vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_5",
                    "../outside/denied.txt",
                )))]
            } else {
                scripted_shell_or_finished(&completed)
            };
            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}

fn scripted_shell_or_finished(completed: &[&str]) -> Vec<waltr_agent::Result<CompletionEvent>> {
    #[cfg(feature = "shell")]
    {
        if !completed.contains(&"call_6") {
            return vec![Ok(CompletionEvent::ToolCall(shell_call(
                "call_6",
                "printf shell-one",
                "first shell command",
            )))];
        }
        if !completed.contains(&"call_7") {
            return vec![Ok(CompletionEvent::ToolCall(shell_call(
                "call_7",
                "printf shell-two",
                "second shell command",
            )))];
        }
        if !completed.contains(&"call_8") {
            return vec![Ok(CompletionEvent::ToolCall(shell_call(
                "call_8",
                "cat ../outside/nested/shell-walk.txt",
                "read a shell path covered by the approved nested directory",
            )))];
        }
        if !completed.contains(&"call_9") {
            return vec![Ok(CompletionEvent::ToolCall(shell_call(
                "call_9",
                "cat ../outside/denied.txt",
                "read a shell path denied by external filesystem policy",
            )))];
        }
        if !completed.contains(&"call_10") {
            return vec![Ok(CompletionEvent::ToolCall(shell_call(
                "call_10",
                "git status",
                "denied shell command",
            )))];
        }
    }

    vec![
        Ok(CompletionEvent::Delta(
            "permission-flow-complete".to_owned(),
        )),
        Ok(CompletionEvent::Finished),
    ]
}

struct PatchDeniedProvider {
    allowed: PathBuf,
    denied: PathBuf,
    #[cfg(feature = "shell")]
    shell_denied: PathBuf,
}

impl LlmStream for PatchDeniedProvider {
    fn name(&self) -> &'static str {
        "patch_denied_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let events = match request.tool_results().count() {
                0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "call_1",
                    "apply_patch",
                    json!({
                        "patchText": format!(
                            "*** Begin Patch\n*** Update File: {}\n+mutated\n*** Update File: {}\n+mutated\n*** End Patch",
                            self.allowed.display(),
                            self.denied.display()
                        )
                    }),
                )))],
                #[cfg(feature = "shell")]
                1 => vec![Ok(CompletionEvent::ToolCall(shell_call(
                    "call_2",
                    &format!("printf mutated > {}", self.shell_denied.display()),
                    "test path denial",
                )))],
                _ => vec![Ok(CompletionEvent::Finished)],
            };
            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}

fn read_call(id: &str, file_path: &str) -> ToolCall {
    ToolCall::new(id, "read", json!({ "filePath": file_path }))
}

async fn first_permission_request_id<S>(mut stream: S) -> waltr_agent::Result<ExternalRequestId>
where
    S: Stream<Item = waltr_agent::Result<AgentTimelineEvent>> + Unpin,
{
    while let Some(event) = stream.next().await {
        let event = event?;
        if let TimelineContent::ExternalRequest { request } = &event.content {
            assert_eq!(request.kind, ExternalRequestKind::Permission);
            return Ok(request.id.clone());
        }
    }
    panic!("expected permission request");
}

#[cfg(feature = "shell")]
fn shell_call(id: &str, command: &str, description: &str) -> ToolCall {
    ToolCall::new(
        id,
        "shell",
        json!({ "command": command, "description": description }),
    )
}

fn test_model(api: ModelApi) -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, OPENAI_TEST_MODEL, api)
}

struct PermissionFixture {
    _parent: TempRoot,
    root: TempRoot,
    outside: TempRoot,
}

impl PermissionFixture {
    fn new() -> waltr_agent::Result<Self> {
        let parent = TempRoot::new()?;
        let root = TempRoot::at(parent.path().join("root"))?;
        let outside = TempRoot::at(parent.path().join("outside"))?;
        root.write("allowed.txt", "root-allowed-101\n")?;
        root.write("nested/inside.txt", "root-nested-inside\n")?;
        outside.write("external.txt", "external-once-202\n")?;
        outside.write("nested/external-nested.txt", "external-nested-303\n")?;
        outside.write("nested/another.txt", "external-another-404\n")?;
        outside.write("nested/shell-walk.txt", "external-shell-walk-606\n")?;
        outside.write("denied.txt", "external-denied-505\n")?;
        Ok(Self {
            _parent: parent,
            root,
            outside,
        })
    }
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> waltr_agent::Result<Self> {
        Self::at(env::temp_dir().join(format!(
            "waltr-agent-permissions-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        )))
    }

    fn at(path: PathBuf) -> waltr_agent::Result<Self> {
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> PathBuf {
        self.path.clone()
    }

    fn write(&self, path: &str, content: &str) -> waltr_agent::Result<()> {
        let path = self.path.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(fs::write(path, content)?)
    }

    fn read(&self, path: &str) -> waltr_agent::Result<String> {
        Ok(fs::read_to_string(self.path.join(path))?)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.path));
    }
}
