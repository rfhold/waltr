//! Deterministic agent loop integration tests.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{StreamExt, stream};
use waltr_agent::{
    AGENTS_INSTRUCTION_FILE, AgentRuntime, AgentTimelineEvent, AgentTool, AgentsPromptConfig,
    BASE_SYSTEM_PROMPT, BoxCompletionStream, BoxStreamFuture, CHANNEL_ASSISTANT_TEXT,
    CHANNEL_EXTERNAL_REQUEST, CHANNEL_THINKING, CHANNEL_TOOL_CALL, CHANNEL_TOOL_PROGRESS_INFO,
    CHANNEL_TOOL_RESULT, CHANNEL_USER_TEXT, CompletionEvent, CompletionReplayItem,
    CompletionRequest, ExternalRequestId, ExternalRequestKind, ExternalResponse,
    FileCredentialStore, FilesystemPromptConfig, InferenceProviderSettings, InferenceSettings,
    LlmError, LlmStream, McpResolvedConnection, MemorySessionStore, ModelApi, ModelRef,
    PendingTimelineEvent, PendingTimelineItemId, ProviderId, ProviderRetryConfig,
    ProviderTurnMetadata, ResumeInput, SessionCoordinator, SessionCopy, SessionCreateRequest,
    SessionId, SessionStart, SessionStartMessage, SessionStatus, SessionStore, SessionTemplateKind,
    SessionTemplateSearchRequest, SessionTemplateSearchTarget, SkillsConfig, ThinkingBlock,
    ThinkingBlockKind, ThinkingDelta, TimelineContent, TimelineEventKind, TimelineQuery,
    TimelineStatus, TokenUsage, ToolCall, ToolDefinition, TurnItem,
    tools::filesystem::{FilesystemConfig, FilesystemToolProfile},
    tools::question::QUESTION_TOOL_NAME,
    tools::question::QuestionConfig,
};
#[cfg(feature = "shell")]
use waltr_agent::{PermissionCommandPattern, tools::shell::ShellConfig};
use waltr_agent::{
    PermissionEffect, PermissionInputCondition, PermissionInputRule, PermissionInputScalar,
    PermissionInputValueMatcher, PermissionJsonPointer, PermissionPolicy, PermissionToolSelector,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::sandbox::TestSandbox;
use crate::scripted_provider::ScriptedProvider;

#[cfg(feature = "shell")]
#[tokio::test]
async fn sandbox_disconnect_after_dispatch_suspends_without_provider_continuation() {
    use waltr_agent::{ExternalRequest, ExternalRequestStatus, SessionJournalEvent, Suspension};

    let root = tempfile::tempdir().unwrap();
    let sandbox = TestSandbox::new(root.path()).await;
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let requests = count.clone();
    let provider = Arc::new(ScriptedProvider::new("sandbox-fence", move |_| {
        let index = requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match index {
            0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "slow-call",
                "shell",
                json!({ "command": "sleep 30", "description": "wait" }),
            )))],
            1 => vec![Ok(CompletionEvent::Finished)],
            _ => panic!("unexpected provider continuation {index}"),
        }
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_permission_policy(PermissionPolicy::default())
        .with_shell_tool(ShellConfig::new(root.path()))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = create_session_id(&sessions).await;
    let mut stream = sessions.prompt(&agent, id.clone(), "run").stream();
    let event = stream.next().await.unwrap().unwrap();
    assert!(crate::session::channel_is(&event, CHANNEL_TOOL_CALL));
    let remaining = tokio::spawn(async move { stream.collect::<Vec<_>>().await });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if store.load(&id).await.unwrap().journal.iter().any(|event| matches!(event,
                SessionJournalEvent::SandboxDispatchStarted { tool_call_id, .. } if tool_call_id == "slow-call")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    sandbox.disconnect();
    let events = tokio::time::timeout(Duration::from_secs(10), remaining)
        .await
        .unwrap()
        .unwrap();
    assert!(events.iter().all(|event| !matches!(
        event.as_ref().unwrap().content,
        TimelineContent::ToolResult { .. }
    )));
    let record = store.load(&id).await.unwrap();
    assert!(matches!(
        record.snapshot().status,
        SessionStatus::Suspended(Suspension::AwaitingToolResults { .. })
    ));
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

    let resumed = sessions
        .resume(&agent, id.clone(), ResumeInput::Continue)
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(
        resumed.is_empty(),
        "no retry or provider continuation: {resumed:?}"
    );
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    let record = store.load(&id).await.unwrap();
    let request_id = ExternalRequestId::new("stale-sandbox-approval");
    store
        .append_timeline(
            &id,
            record.revision,
            vec![PendingTimelineEvent::new(
                id.clone(),
                0,
                PendingTimelineItemId::New,
                TimelineEventKind::ExternalRequestOpened,
                Some(waltr_agent::TimelineChannel::from(CHANNEL_EXTERNAL_REQUEST)),
                TimelineContent::ExternalRequest {
                    request: ExternalRequest {
                        id: request_id.clone(),
                        kind: ExternalRequestKind::Permission,
                        tool_call_id: Some("slow-call".into()),
                        prompt: "stale approval".into(),
                        payload: json!({}),
                        status: ExternalRequestStatus::Pending,
                    },
                },
            )],
        )
        .await
        .unwrap();
    let approval = sessions
        .resume(
            &agent,
            id.clone(),
            ResumeInput::ExternalResponse {
                request_id: request_id.clone(),
                response: ExternalResponse::new(json!("allow-once")),
            },
        )
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(
        approval.is_empty(),
        "stale approval must not reissue: {approval:?}"
    );
    let record = store.load(&id).await.unwrap();
    assert!(record.snapshot().external_requests.iter().any(
        |request| request.id == request_id && request.status == ExternalRequestStatus::Pending
    ));
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    let superseded = sessions
        .prompt(&agent, id.clone(), "ignore previous work")
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(superseded.iter().all(Result::is_ok), "{superseded:?}");
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn legacy_pending_sandbox_call_without_dispatch_marker_fails_closed() {
    use waltr_agent::{SessionJournalEvent, Suspension};
    let root = tempfile::tempdir().unwrap();
    let sandbox = TestSandbox::new(root.path()).await;
    let provider = Arc::new(ScriptedProvider::from_turns(
        "legacy-sandbox",
        vec![vec![CompletionEvent::Finished]],
    ));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(FilesystemConfig::new(root.path()))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = create_session_id(&sessions).await;
    let record = store.load(&id).await.unwrap();
    store
        .append_timeline(
            &id,
            record.revision,
            vec![PendingTimelineEvent::tool_call(
                id.clone(),
                0,
                PendingTimelineItemId::New,
                ToolCall::new(
                    "legacy-call",
                    "write",
                    json!({"filePath":"note", "content":"unsafe"}),
                ),
            )],
        )
        .await
        .unwrap();
    let events = sessions
        .resume(&agent, id.clone(), ResumeInput::Continue)
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(
        events.is_empty(),
        "legacy call must not be replayed: {events:?}"
    );
    let record = store.load(&id).await.unwrap();
    assert!(matches!(
        record.snapshot().status,
        SessionStatus::Suspended(Suspension::AwaitingToolResults { .. })
    ));
    assert!(
        record
            .journal
            .iter()
            .any(|event| matches!(event, SessionJournalEvent::Suspended { .. }))
    );
    assert!(!root.path().join("note").exists());
    let reconciled = sessions
        .resume(
            &agent,
            id.clone(),
            ResumeInput::ToolResults(vec![waltr_agent::ToolResult::new(
                "legacy-call",
                "write",
                "externally reconciled",
            )]),
        )
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(reconciled.iter().all(Result::is_ok), "{reconciled:?}");
    assert_eq!(
        store.load(&id).await.unwrap().snapshot().status,
        SessionStatus::Finished
    );
    assert!(!root.path().join("note").exists());
}

#[tokio::test]
async fn snapshot_is_bounded_and_replayed_from_durable_middleware_state() {
    let root = TempRoot::new("waltr-agent-snapshot");
    root.write("Cargo.toml", "");
    root.write("src/lib.rs", "");
    let sandbox = TestSandbox::new(root.path()).await;
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let seen = prompts.clone();
    let provider = Arc::new(ScriptedProvider::new("snapshot", move |request| {
        seen.lock()
            .unwrap()
            .push(request.system_prompt.unwrap_or_default().to_owned());
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_filesystem_system_prompt(
            FilesystemPromptConfig::new(root.path())
                .with_snapshot()
                .with_max_snapshot_entries(2),
        )
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = create_session_id(&sessions).await;
    drain(sessions.prompt(&agent, id.clone(), "hello").stream()).await;
    let saved = store.load(&id).await.unwrap().snapshot();
    let snapshot = saved.middleware_state["filesystem_snapshot"]
        .as_str()
        .unwrap();
    assert!(snapshot.starts_with("Current filesystem snapshot:\n"));
    assert!(snapshot.ends_with("- <snapshot truncated>"));
    assert!(snapshot.contains("- Cargo.toml") || snapshot.contains("- src/"));
    assert!(prompts.lock().unwrap()[0].contains(snapshot));

    root.write("new.txt", "changed after snapshot");
    sandbox.disconnect();
    drain(sessions.prompt(&agent, id.clone(), "again").stream()).await;
    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains(snapshot));
    assert!(!prompts[1].contains("new.txt"));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn session_template_search_finds_files_and_tools() {
    let root = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(root.path().join("src/nested")).expect("created dirs");
    fs::write(root.path().join("src/nested/resource.rs"), "resource").expect("wrote file");
    fs::write(root.path().join("src/two words.md"), "notes").expect("wrote file");
    fs::write(root.path().join(".hidden.rs"), "hidden").expect("wrote hidden file");
    for index in 0..1030 {
        fs::write(root.path().join(format!("early-{index:04}.txt")), "").expect("wrote file");
    }
    fs::write(root.path().join("z-target-beyond-cap.rs"), "").expect("wrote target");
    let sandbox = TestSandbox::new(root.path()).await;

    let provider = Arc::new(ScriptedProvider::from_turns("resource-search", Vec::new()));
    let runtime_factory = waltr_agent::AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.path().join("host-placeholder-that-does-not-exist"),
        FileCredentialStore::new(root.path().join("auth.json")),
        crate::agent_definitions::filesystem_agent(),
    )
    .with_runtime_provider(move |_| {
        Ok(waltr_agent::AgentRuntimeProvider::new(
            test_model(),
            provider.clone(),
        ))
    });
    let sessions =
        SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(runtime_factory);
    let mut request = SessionCreateRequest::new(SessionId::generate());
    request.inference_settings = Some(test_inference_settings());
    let session_id = sessions
        .create(request)
        .await
        .expect("created session")
        .id()
        .clone();

    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: "re".to_owned(),
                limit: 10,
                kinds: Vec::new(),
                target: SessionTemplateSearchTarget::Resources,
                cursor: None,
            },
        )
        .await
        .expect("searched resources");

    assert!(options.iter().any(|option| {
        option.kind == SessionTemplateKind::File
            && option.detail == "src/nested/resource.rs"
            && option.template == "@src/nested/resource.rs"
    }));
    assert!(options.iter().any(|option| {
        option.kind == SessionTemplateKind::Tool
            && option.label == "read"
            && option.template == "@tool(read, {\"filePath\":\"\"})"
    }));
    assert!(!options.iter().any(|option| option.detail == ".hidden.rs"));

    let beyond_cap = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: "target-beyond-cap".into(),
                limit: 2,
                kinds: vec![SessionTemplateKind::File],
                target: SessionTemplateSearchTarget::Resources,
                cursor: None,
            },
        )
        .await
        .expect("searched beyond first 1024 paths");
    assert_eq!(beyond_cap.len(), 1);
    assert_eq!(beyond_cap[0].detail, "z-target-beyond-cap.rs");

    let quoted = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: "words".to_owned(),
                limit: 10,
                kinds: vec![SessionTemplateKind::File],
                target: SessionTemplateSearchTarget::Resources,
                cursor: None,
            },
        )
        .await
        .expect("searched files");
    assert!(quoted.iter().any(|option| {
        option.detail == "src/two words.md" && option.template == "@\"src/two words.md\""
    }));
}

#[tokio::test]
async fn session_template_search_finds_sandbox_skills() {
    let root = tempfile::tempdir().expect("temp dir");
    let root_path = root.path().canonicalize().expect("canonical root");
    let skill_dir = root_path.join(".agents/skills/documenting");
    fs::create_dir_all(&skill_dir).expect("skill directory");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: documenting\ndescription: Write repository documentation\n---\n# Guide\n",
    )
    .expect("skill file");
    let sandbox = TestSandbox::new(&root_path).await;
    let mut registry = waltr_agent::AgentDefinitionRegistry::new();
    registry
        .load_yaml("apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: test }\nspec:\n  features: [{ skills: { root: $(session.workspace.root) } }]\n")
        .expect("agent definition");
    let factory = waltr_agent::AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root_path.clone(),
        FileCredentialStore::new(root_path.join("auth.json")),
        registry.resolve_agent("test").expect("resolved agent"),
    )
    .with_runtime_provider(|_| {
        Ok(waltr_agent::AgentRuntimeProvider::new(
            test_model(),
            Arc::new(ScriptedProvider::from_turns("skill-search", Vec::new())),
        ))
    });
    let sessions = SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory);
    let mut request = SessionCreateRequest::new(SessionId::generate());
    request.inference_settings = Some(test_inference_settings());
    let session_id = sessions
        .create(request)
        .await
        .expect("session")
        .id()
        .clone();
    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: "skill:doc".to_owned(),
                limit: 10,
                kinds: vec![SessionTemplateKind::Prompt],
                target: SessionTemplateSearchTarget::Prompts,
                cursor: None,
            },
        )
        .await
        .expect("skill search");
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].label, "skill:documenting");
    assert_eq!(options[0].template, "/skill:documenting");
    assert_eq!(options[0].metadata["promptType"], "skill");
}

#[tokio::test]
async fn filesystem_tools_execute_through_sandbox_across_write_read_search_edit_and_delete() {
    let root = tempfile::tempdir().expect("workspace");
    let root_path = root.path().canonicalize().expect("canonical workspace");
    let sandbox = TestSandbox::new(&root_path).await;
    let calls = [
        (
            "write",
            json!({"filePath":"nested/note.txt","content":"alpha\nbeta\n"}),
        ),
        ("read", json!({"filePath":"nested/note.txt"})),
        ("glob", json!({"pattern":"**/*.txt"})),
        ("grep", json!({"pattern":"alpha"})),
        (
            "edit",
            json!({"filePath":"nested/note.txt","oldString":"beta","newString":"gamma"}),
        ),
        ("read", json!({"filePath":"nested/note.txt"})),
        ("delete", json!({"filePath":"nested/note.txt"})),
    ];
    let provider = Arc::new(ScriptedProvider::new(
        "sandbox-filesystem-operations",
        move |request| {
            let index = request.tool_results().count();
            if let Some((name, arguments)) = calls.get(index) {
                vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    format!("fs-{index}"),
                    *name,
                    arguments.clone(),
                )))]
            } else {
                vec![Ok(CompletionEvent::Finished)]
            }
        },
    ));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(
            FilesystemConfig::new(&root_path).with_profile(FilesystemToolProfile::ExactEdit),
        )
        .build();
    let events = collect_events(agent.prompt("exercise workspace tools").stream()).await;
    let results = events
        .iter()
        .filter_map(|event| match &event.content {
            TimelineContent::ToolResult { result } => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 7, "{results:#?}");
    assert!(
        results.iter().all(|result| !result.is_error),
        "{results:#?}"
    );
    assert!(results[1].content.contains("alpha"), "{results:#?}");
    assert!(results[2].content.contains("note.txt"), "{results:#?}");
    assert!(results[3].content.contains("alpha"), "{results:#?}");
    assert!(results[5].content.contains("gamma"), "{results:#?}");
    assert!(!root.path().join("nested/note.txt").exists());
}

#[tokio::test]
async fn structured_patch_executes_through_sandbox() {
    let root = tempfile::tempdir().expect("workspace");
    let root_path = root.path().canonicalize().expect("canonical workspace");
    fs::write(root_path.join("existing.txt"), "before\nold\nafter\n").expect("fixture");
    let sandbox = TestSandbox::new(&root_path).await;
    let provider = Arc::new(ScriptedProvider::new("sandbox-patch", |request| {
        if request.tool_results().next().is_some() {
            vec![Ok(CompletionEvent::Finished)]
        } else {
            vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "patch-1",
                "apply_patch",
                json!({"patchText":"*** Begin Patch\n*** Update File: existing.txt\n@@\n before\n-old\n+new\n after\n*** Add File: added.txt\n+created\n*** End Patch"}),
            )))]
        }
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(
            FilesystemConfig::new(&root_path).with_profile(FilesystemToolProfile::StructuredPatch),
        )
        .build();
    let events = collect_events(agent.prompt("apply patch").stream()).await;
    let result = events
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ToolResult { result } => Some(result),
            _ => None,
        })
        .expect("patch result");
    assert!(!result.is_error, "{result:#?}");
    assert_eq!(
        fs::read_to_string(root_path.join("existing.txt")).unwrap(),
        "before\nnew\nafter\n"
    );
    assert_eq!(
        fs::read_to_string(root_path.join("added.txt")).unwrap(),
        "created\n"
    );
}

#[tokio::test]
async fn session_template_search_clamps_oversized_limits() {
    let root = tempfile::tempdir().expect("temp dir");
    for index in 0..150 {
        fs::write(
            root.path().join(format!("resource-{index:03}.rs")),
            "resource",
        )
        .expect("wrote file");
    }
    let sandbox = TestSandbox::new(root.path()).await;

    let provider = Arc::new(ScriptedProvider::from_turns("resource-limit", Vec::new()));
    let runtime_factory = waltr_agent::AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.path().canonicalize().expect("canonical root"),
        FileCredentialStore::new(root.path().join("auth.json")),
        crate::agent_definitions::filesystem_agent(),
    )
    .with_runtime_provider(move |_| {
        Ok(waltr_agent::AgentRuntimeProvider::new(
            test_model(),
            provider.clone(),
        ))
    });
    let sessions =
        SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(runtime_factory);
    let mut request = SessionCreateRequest::new(SessionId::generate());
    request.inference_settings = Some(test_inference_settings());
    let session_id = sessions
        .create(request)
        .await
        .expect("created session")
        .id()
        .clone();

    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: "resource".to_owned(),
                limit: usize::MAX,
                kinds: vec![SessionTemplateKind::File],
                target: SessionTemplateSearchTarget::Resources,
                cursor: None,
            },
        )
        .await
        .expect("searched resources");

    assert_eq!(options.len(), 100);
}

#[tokio::test]
async fn session_template_search_ignores_mcp_startup_failures() {
    let root = tempfile::tempdir().expect("temp dir");
    let sandbox = TestSandbox::new(root.path()).await;
    let runtime_factory = waltr_agent::AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.path().canonicalize().expect("canonical root"),
        FileCredentialStore::new(root.path().join("auth.json")),
        crate::agent_definitions::filesystem_mcp_agent(),
    )
    .with_mcp_connection_resolver(|_| {
        Box::pin(async {
            Ok(vec![McpResolvedConnection {
                key: "missing".to_owned(),
                kind: waltr_agent::McpConnectionKind::Stdio,
                command: "definitely-not-installed-waltr-mcp".to_owned(),
                url: String::new(),
            }])
        })
    });
    let sessions =
        SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(runtime_factory);
    let mut request = SessionCreateRequest::new(SessionId::generate());
    request.inference_settings = Some(test_inference_settings());
    let session_id = sessions
        .create(request)
        .await
        .expect("created session")
        .id()
        .clone();

    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: "read".to_owned(),
                limit: 10,
                kinds: vec![SessionTemplateKind::Tool],
                target: SessionTemplateSearchTarget::Resources,
                cursor: None,
            },
        )
        .await
        .expect("searched resources");

    assert!(options.iter().any(|option| {
        option.kind == SessionTemplateKind::Tool
            && option.label == "read"
            && option.template == "@tool(read, {\"filePath\":\"\"})"
    }));
}

#[tokio::test]
async fn session_template_search_empty_query_returns_no_options() {
    let root = tempfile::tempdir().expect("temp dir");
    fs::write(root.path().join("visible.rs"), "visible").expect("wrote file");
    let sandbox = TestSandbox::new(root.path()).await;
    let provider = Arc::new(ScriptedProvider::from_turns("resource-empty", Vec::new()));
    let runtime_factory = waltr_agent::AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.path().canonicalize().expect("canonical root"),
        FileCredentialStore::new(root.path().join("auth.json")),
        crate::agent_definitions::filesystem_agent(),
    )
    .with_runtime_provider(move |_| {
        Ok(waltr_agent::AgentRuntimeProvider::new(
            test_model(),
            provider.clone(),
        ))
    });
    let sessions =
        SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(runtime_factory);
    let mut request = SessionCreateRequest::new(SessionId::generate());
    request.inference_settings = Some(test_inference_settings());
    let session_id = sessions
        .create(request)
        .await
        .expect("created session")
        .id()
        .clone();

    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: String::new(),
                limit: 10,
                kinds: Vec::new(),
                target: SessionTemplateSearchTarget::Resources,
                cursor: None,
            },
        )
        .await
        .expect("searched resources");

    assert!(options.is_empty());
}

#[tokio::test]
async fn durable_session_resume_continues_same_agent_loop() {
    let provider = Arc::new(DurableResumeProvider::default());
    let agent = AgentRuntime::builder(test_model(), provider.clone())
        .with_question_tool(QuestionConfig::new())
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .expect("created session")
        .id()
        .clone();

    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "ask then continue")
        .stream();
    let mut request_id = None;
    while let Some(event) = stream.next().await {
        let event = event.expect("agent event");
        if let TimelineContent::ExternalRequest { request } = &event.content {
            assert_eq!(request.kind, ExternalRequestKind::Question);
            assert!(crate::session::channel_is(&event, CHANNEL_EXTERNAL_REQUEST));
            request_id = Some(request.id.as_str().to_owned());
        }
    }
    let request_id = request_id.expect("question request");
    let snapshot = store
        .load(&session_id)
        .await
        .expect("stored session")
        .snapshot();
    assert!(
        snapshot
            .external_requests
            .iter()
            .any(|request| request.kind == ExternalRequestKind::Question)
    );

    let mut resumed = sessions
        .resume(
            &agent,
            session_id.clone(),
            ResumeInput::ExternalResponse {
                request_id: ExternalRequestId::new(request_id),
                response: ExternalResponse::new(json!("resume-answer-123")),
            },
        )
        .stream();
    let mut text = String::new();
    while let Some(event) = resumed.next().await {
        let event = event.expect("agent event");
        if let TimelineContent::TextDelta { delta } = &event.content
            && crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT)
        {
            text.push_str(delta);
        }
    }

    assert_eq!(text, "resumed-with-answer");
    assert!(provider.saw_resumed_tool_result());
    let snapshot = store
        .load(&session_id)
        .await
        .expect("stored session")
        .snapshot();
    assert!(snapshot.tool_results().any(|result| {
        result.name == QUESTION_TOOL_NAME && result.content == "resume-answer-123"
    }));
}

#[tokio::test]
async fn provider_projection_and_turn_metadata_are_journaled() {
    let agent =
        AgentRuntime::builder(test_model(), Arc::new(MetadataEmittingProvider::default())).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    drain(
        sessions
            .prompt(&agent, session_id.clone(), "hello")
            .stream(),
    )
    .await;

    let record = store.load(&session_id).await.expect("stored session");
    let request = record
        .provider_requests()
        .next()
        .expect("journaled request record");
    assert_eq!(
        request.projected_request,
        json!({ "wire": "projected", "prompt": "hello" }),
        "projected request must be the provider wire payload"
    );
    assert_ne!(request.projected_request, request.canonical_request);
    assert_eq!(
        record
            .snapshot()
            .turns
            .first()
            .and_then(|turn| turn.metadata.clone()),
        Some(expected_turn_metadata())
    );
}

#[tokio::test]
async fn provider_turn_metadata_is_forwarded_without_agent_filtering() {
    let agent =
        AgentRuntime::builder(test_model(), Arc::new(MetadataEmittingProvider::default())).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    drain(
        sessions
            .prompt(&agent, session_id.clone(), "first prompt")
            .stream(),
    )
    .await;
    drain(
        sessions
            .prompt(&agent, session_id.clone(), "second prompt")
            .stream(),
    )
    .await;
    let foreign_agent =
        AgentRuntime::builder(test_model(), Arc::new(ForeignMetadataProvider)).build();
    drain(
        sessions
            .prompt(&foreign_agent, session_id, "third prompt")
            .stream(),
    )
    .await;
}

#[tokio::test]
async fn provider_request_open_failure_retries_from_checkpoint() {
    let provider = Arc::new(RequestOpenRetryProvider::default());
    let agent = AgentRuntime::builder(test_model(), provider.clone())
        .with_provider_retry_config(test_retry_config(3))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    let events = collect_events(
        sessions
            .prompt(&agent, session_id.clone(), "retry request")
            .stream(),
    )
    .await;

    assert_eq!(provider.requests(), 2);
    assert_eq!(assistant_text(&events), "request retry recovered");
    assert!(events.iter().any(|event| {
        event.kind == TimelineEventKind::SessionStatusChanged
            && event.status == Some(TimelineStatus::RetryingProvider)
    }));
    let record = store.load(&session_id).await.expect("stored session");
    assert_eq!(record.provider_requests().count(), 1);
}

#[tokio::test]
async fn provider_stream_failure_rolls_back_partial_output_before_retry() {
    let requests = Mutex::new(0);
    let provider = Arc::new(ScriptedProvider::new("retry-stream", move |_| {
        let mut requests = requests.lock().expect("request count");
        let request = *requests;
        *requests += 1;
        drop(requests);
        match request {
            0 => vec![
                Ok(CompletionEvent::Delta("partial".to_owned())),
                Err(retryable_api_error("stream rate limited")),
            ],
            1 => vec![
                Ok(CompletionEvent::Delta("final".to_owned())),
                Ok(CompletionEvent::Finished),
            ],
            _ => panic!("unexpected provider request {request}"),
        }
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_provider_retry_config(test_retry_config(3))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    let events = collect_events(
        sessions
            .prompt(&agent, session_id.clone(), "retry stream")
            .stream(),
    )
    .await;

    assert_eq!(assistant_text(&events), "partialfinal");
    let snapshot = store
        .load(&session_id)
        .await
        .expect("stored session")
        .snapshot();
    assert_eq!(snapshot.assistant_texts().collect::<Vec<_>>(), ["final"]);
    assert!(matches!(snapshot.status, SessionStatus::Finished));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn copied_context_is_replayed_once_across_retry_and_next_turn() {
    let sequences = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(0));
    let provider = Arc::new(ScriptedProvider::new("copy-retry", {
        let sequences = Arc::clone(&sequences);
        let requests = Arc::clone(&requests);
        move |request| {
            sequences
                .lock()
                .expect("sequence lock")
                .push(copy_replay_sequence(request));
            let mut requests = requests.lock().expect("request lock");
            let request = *requests;
            *requests += 1;
            drop(requests);
            match request {
                0 => vec![Err(retryable_api_error("retry copied request"))],
                1 => vec![
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "missing-call",
                        "missing",
                        json!({}),
                    ))),
                    Ok(CompletionEvent::Finished),
                ],
                2 => vec![
                    Ok(CompletionEvent::Delta("done".to_owned())),
                    Ok(CompletionEvent::Finished),
                ],
                request => panic!("unexpected provider request {request}"),
            }
        }
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_provider_retry_config(test_retry_config(3))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let source_id = SessionId::new("copy-source");
    let mut source_request = SessionCreateRequest::new(source_id.clone());
    source_request.start = SessionStart::Fresh {
        messages: vec![SessionStartMessage::user("source prompt").unwrap()],
    };
    let source = store.create(source_request).await.unwrap();
    store
        .append_timeline(
            &source_id,
            source.revision,
            vec![PendingTimelineEvent::assistant_text_snapshot(
                source_id.clone(),
                0,
                PendingTimelineItemId::New,
                "source answer",
            )],
        )
        .await
        .unwrap();
    store
        .append_journal(
            &source_id,
            source.revision.next(),
            vec![waltr_agent::SessionJournalEvent::TurnFinished { turn: 0 }],
        )
        .await
        .unwrap();
    let source = store.load(&source_id).await.unwrap();
    let session_id = SessionId::new("copied-replay");
    let mut create = SessionCreateRequest::new(session_id.clone());
    create.start = SessionStart::Copy {
        copy: SessionCopy::from_record(&source, waltr_agent::TimelineCursor::new(2)).unwrap(),
        messages: vec![SessionStartMessage::user("destination prompt").unwrap()],
    };

    let created = store.create(create).await.expect("copied session");

    assert_eq!(created.revision, waltr_agent::Revision::ZERO);
    assert_eq!(created.timeline_cursor_high_water().get(), 3);
    assert_eq!(
        created.snapshot().prompts,
        ["source prompt", "destination prompt"]
    );
    assert_eq!(created.snapshot().prompt_turns[1].turn, 1);

    drain(
        sessions
            .resume(&agent, session_id.clone(), ResumeInput::Continue)
            .stream(),
    )
    .await;

    let sequences = sequences.lock().expect("sequence lock").clone();
    assert_eq!(sequences.len(), 3);
    for sequence in &sequences {
        assert_eq!(
            sequence
                .iter()
                .filter(|item| item.as_str() == "user:source prompt")
                .count(),
            1
        );
        assert_eq!(
            sequence
                .iter()
                .filter(|item| item.as_str() == "assistant:source answer")
                .count(),
            1
        );
        assert_eq!(
            sequence
                .iter()
                .filter(|item| item.as_str() == "user:destination prompt")
                .count(),
            1
        );
    }
    assert_eq!(sequences[0], sequences[1]);
    assert!(
        sequences[2]
            .iter()
            .any(|item| item == "tool-call:missing-call")
    );
    assert!(
        sequences[2]
            .iter()
            .any(|item| item == "tool-result:missing-call")
    );
    let snapshot = store.load(&session_id).await.unwrap().snapshot();
    assert_eq!(snapshot.prompt_turns[0].prompt, "source prompt");
    assert_eq!(snapshot.prompt_turns[1].prompt, "destination prompt");
    assert_eq!(
        snapshot.turns[0].assistant_texts().collect::<Vec<_>>(),
        ["source answer"]
    );
}

#[tokio::test]
async fn provider_retry_exhaustion_records_failed_status() {
    let provider = Arc::new(AlwaysFailingProvider::default());
    let agent = AgentRuntime::builder(test_model(), provider.clone())
        .with_provider_retry_config(test_retry_config(2))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "retry exhausted")
        .stream();
    let mut final_error = None;
    while let Some(event) = stream.next().await {
        if let Err(error) = event {
            final_error = Some(error);
        }
    }

    assert!(final_error.is_some());
    assert_eq!(provider.requests(), 2);
    let snapshot = store
        .load(&session_id)
        .await
        .expect("stored session")
        .snapshot();
    assert!(matches!(snapshot.status, SessionStatus::Failed { .. }));
}

#[tokio::test]
async fn durable_multi_prompt_request_replays_user_prompts_in_order() {
    let provider = Arc::new(ChronologyProvider::default());
    let agent = AgentRuntime::builder(test_model(), provider.clone()).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    drain(
        sessions
            .prompt(&agent, session_id.clone(), "first prompt")
            .stream(),
    )
    .await;
    drain(
        sessions
            .prompt(&agent, session_id.clone(), "second prompt")
            .stream(),
    )
    .await;

    assert_eq!(
        provider.replay_sequences(),
        vec![
            vec!["user:first prompt".to_owned()],
            vec![
                "user:first prompt".to_owned(),
                "assistant:first answer".to_owned(),
                "user:second prompt".to_owned(),
            ],
        ]
    );
    let snapshot = store
        .load(&session_id)
        .await
        .expect("stored session")
        .snapshot();
    assert_eq!(
        snapshot
            .prompt_turns
            .iter()
            .map(|prompt| (prompt.turn, prompt.prompt.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "first prompt"), (1, "second prompt")]
    );
}

#[tokio::test]
async fn superseded_prompt_closes_turn_and_keeps_distinct_user_timeline_items() {
    let provider = Arc::new(PendingSupersedeProvider::default());
    let agent = AgentRuntime::builder(test_model(), provider.clone()).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    let mut first = sessions
        .prompt(&agent, session_id.clone(), "first prompt")
        .stream();
    while provider.replay_sequences().is_empty() {
        first
            .next()
            .await
            .expect("first prompt event")
            .expect("first prompt event");
    }

    drain(
        sessions
            .prompt(&agent, session_id.clone(), "second prompt")
            .stream(),
    )
    .await;
    drop(first);

    assert_eq!(
        provider.replay_sequences(),
        vec![
            vec!["user:first prompt".to_owned()],
            vec![
                "user:first prompt".to_owned(),
                "user:second prompt".to_owned(),
            ],
        ]
    );

    let record = store.load(&session_id).await.expect("stored session");
    let snapshot = record.snapshot();
    assert_eq!(
        snapshot
            .prompt_turns
            .iter()
            .map(|prompt| (prompt.turn, prompt.prompt.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "first prompt"), (1, "second prompt")]
    );

    let user_events = store
        .timeline(&session_id, TimelineQuery::forward(None, 20))
        .await
        .expect("timeline events")
        .into_iter()
        .filter(|event| crate::session::channel_is(event, CHANNEL_USER_TEXT))
        .collect::<Vec<_>>();
    assert_eq!(user_events.len(), 2);
    assert_eq!(user_events[0].turn, 0);
    assert_eq!(user_events[1].turn, 1);
    assert_ne!(user_events[0].item_id, user_events[1].item_id);
    assert!(matches!(
        &user_events[0].content,
        TimelineContent::TextSnapshot { text } if text == "first prompt"
    ));
    assert!(matches!(
        &user_events[1].content,
        TimelineContent::TextSnapshot { text } if text == "second prompt"
    ));
}

#[tokio::test]
async fn durable_session_timeline_ranges_preserve_cursor_order() {
    let agent =
        AgentRuntime::builder(test_model(), Arc::new(MetadataEmittingProvider::default())).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    drain(
        sessions
            .prompt(&agent, session_id.clone(), "range prompt")
            .stream(),
    )
    .await;

    let all = store
        .timeline(&session_id, TimelineQuery::forward(None, 10))
        .await
        .expect("timeline events");
    assert!(
        all.len() >= 2,
        "expected content and finished status events"
    );
    assert!(
        all.windows(2)
            .all(|events| events[0].cursor < events[1].cursor)
    );
    assert!(matches!(
        &all[0].content,
        TimelineContent::TextSnapshot { text } if text == "range prompt"
    ));
    let first_assistant = all
        .iter()
        .find(|event| crate::session::channel_is(event, CHANNEL_ASSISTANT_TEXT))
        .expect("assistant event");
    assert!(matches!(
        &first_assistant.content,
        TimelineContent::TextDelta { delta } if delta == "first"
    ));
    assert_eq!(
        all.last().map(|event| (event.kind, event.status)),
        Some((
            TimelineEventKind::SessionStatusChanged,
            Some(TimelineStatus::Finished)
        ))
    );

    let after_first = store
        .timeline(&session_id, TimelineQuery::forward(Some(all[0].cursor), 10))
        .await
        .expect("timeline after first cursor");
    assert_eq!(after_first, all[1..].to_vec());

    let before_last = store
        .timeline(
            &session_id,
            TimelineQuery::backward(all.last().map(|event| event.cursor), 10),
        )
        .await
        .expect("timeline before last cursor");
    assert_eq!(before_last, all[..all.len() - 1].to_vec());

    let limited = store
        .timeline(&session_id, TimelineQuery::forward(None, 1))
        .await
        .expect("limited timeline range");
    assert_eq!(limited, vec![all[0].clone()]);
}

#[tokio::test]
async fn assistant_and_thinking_deltas_use_stable_items_and_reconstruct_text() {
    let provider = ScriptedProvider::from_turns(
        "streaming_text_provider",
        vec![vec![
            CompletionEvent::ThinkingDelta(ThinkingDelta {
                text: "think-1 ".to_owned(),
            }),
            CompletionEvent::ThinkingDelta(ThinkingDelta {
                text: "think-2".to_owned(),
            }),
            CompletionEvent::Delta("answer-1 ".to_owned()),
            CompletionEvent::Delta("answer-2".to_owned()),
            CompletionEvent::Finished,
        ]],
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider)).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    let events = sessions
        .prompt(&agent, session_id.clone(), "stream text")
        .stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("agent event"))
        .collect::<Vec<_>>();

    let assistant_events = events
        .iter()
        .filter(|event| crate::session::channel_is(event, CHANNEL_ASSISTANT_TEXT))
        .collect::<Vec<_>>();
    let thinking_events = events
        .iter()
        .filter(|event| crate::session::channel_is(event, CHANNEL_THINKING))
        .collect::<Vec<_>>();

    assert_eq!(assistant_events.len(), 3);
    assert_eq!(thinking_events.len(), 2);
    assert!(
        assistant_events
            .iter()
            .all(|event| event.item_id == assistant_events[0].item_id)
    );
    assert!(
        thinking_events
            .iter()
            .all(|event| event.item_id == thinking_events[0].item_id)
    );
    assert_eq!(assistant_events[0].kind, TimelineEventKind::ContentDelta);
    assert_eq!(assistant_events[1].kind, TimelineEventKind::ContentDelta);
    assert_eq!(assistant_events[2].kind, TimelineEventKind::ItemFinished);
    assert!(matches!(
        &assistant_events[2].content,
        TimelineContent::TextSnapshot { text } if text == "answer-1 answer-2"
    ));
    assert!(
        thinking_events
            .iter()
            .all(|event| event.kind == TimelineEventKind::ContentDelta)
    );

    let assistant_text = store
        .timeline_item_text(&session_id, &assistant_events[0].item_id)
        .await
        .expect("assistant item text");
    let thinking_text = store
        .timeline_item_text(&session_id, &thinking_events[0].item_id)
        .await
        .expect("thinking item text");

    assert_eq!(assistant_text.text, "answer-1 answer-2");
    assert_eq!(thinking_text.text, "think-1 think-2");
}

#[tokio::test]
async fn tool_progress_streams_between_call_preview_and_result() {
    let provider = ScriptedProvider::from_turns(
        "progress_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "progress-call",
                "progress_tool",
                json!({}),
            ))],
            vec![CompletionEvent::Finished],
        ],
    );
    let tool = AgentTool::new_async(
        ToolDefinition::new(
            "progress_tool",
            "Emit progress before finishing.",
            json!({}),
        ),
        |context| {
            Box::pin(async move {
                context.progress().info("progress-started")?;
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok(context.result("progress-finished"))
            })
        },
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider))
        .with_tool(tool)
        .build();

    let events = agent
        .prompt("run progress tool")
        .stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("agent event"))
        .collect::<Vec<_>>();

    let call_index = events
        .iter()
        .position(|event| crate::session::channel_is(event, CHANNEL_TOOL_CALL))
        .expect("tool-call preview");
    let progress_index = events
        .iter()
        .position(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_PROGRESS_INFO)
                && matches!(&event.content, TimelineContent::TextDelta { delta } if delta == "progress-started")
        })
        .expect("tool progress");
    let result_index = events
        .iter()
        .position(|event| crate::session::channel_is(event, CHANNEL_TOOL_RESULT))
        .expect("tool result");

    assert!(call_index < progress_index);
    assert!(progress_index < result_index);
    assert_eq!(
        events[progress_index].parent_id,
        Some(events[call_index].item_id.clone())
    );
    assert_eq!(
        events[result_index].parent_id,
        Some(events[call_index].item_id.clone())
    );
    assert!(events[call_index].cursor < events[progress_index].cursor);
    assert!(events[progress_index].cursor < events[result_index].cursor);
}

#[tokio::test]
async fn provider_turn_can_execute_multiple_tool_calls() {
    let requests = Arc::new(Mutex::new(0usize));
    let provider = ScriptedProvider::new("multi_tool_provider", {
        let requests = requests.clone();
        move |request| {
            let mut count = requests.lock().expect("request count");
            let index = *count;
            *count += 1;
            drop(count);

            match index {
                0 => vec![
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "multi-call-1",
                        "multi_tool",
                        json!({ "value": 1 }),
                    ))),
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "multi-call-2",
                        "multi_tool",
                        json!({ "value": 2 }),
                    ))),
                    Ok(CompletionEvent::Finished),
                ],
                1 => {
                    let results = request.tool_results().collect::<Vec<_>>();
                    assert_eq!(results.len(), 2, "provider should receive both results");
                    assert_eq!(results[0].call_id, "multi-call-1");
                    assert_eq!(results[0].content, "result-multi-call-1");
                    assert_eq!(results[1].call_id, "multi-call-2");
                    assert_eq!(results[1].content, "result-multi-call-2");
                    vec![Ok(CompletionEvent::Finished)]
                }
                _ => panic!("unexpected provider request {index}"),
            }
        }
    });
    let tool = AgentTool::new(
        ToolDefinition::new("multi_tool", "Return the call id.", json!({})),
        |context| Ok(context.result(format!("result-{}", context.call().id))),
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider))
        .with_tool(tool)
        .build();

    let events = agent
        .prompt("run multiple tools")
        .stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("agent event"))
        .collect::<Vec<_>>();

    let call_ids = events
        .iter()
        .filter_map(|event| match &event.content {
            TimelineContent::ToolCall { call }
                if crate::session::channel_is(event, CHANNEL_TOOL_CALL) =>
            {
                Some(call.id.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let result_ids = events
        .iter()
        .filter_map(|event| match &event.content {
            TimelineContent::ToolResult { result }
                if crate::session::channel_is(event, CHANNEL_TOOL_RESULT) =>
            {
                Some(result.call_id.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(call_ids, vec!["multi-call-1", "multi-call-2"]);
    assert_eq!(result_ids, vec!["multi-call-1", "multi-call-2"]);
    assert_eq!(*requests.lock().expect("request count"), 2);
}

#[tokio::test]
async fn interrupting_multi_tool_turn_preserves_finished_results() {
    let provider = ScriptedProvider::from_turns(
        "multi_tool_interrupt_provider",
        vec![vec![
            CompletionEvent::ToolCall(ToolCall::new("finished-call", "fast_tool", json!({}))),
            CompletionEvent::ToolCall(ToolCall::new("pending-call", "slow_tool", json!({}))),
            CompletionEvent::Finished,
        ]],
    );
    let fast_tool = AgentTool::new(
        ToolDefinition::new("fast_tool", "Finish immediately.", json!({})),
        |context| Ok(context.result("fast-result")),
    );
    let slow_tool = AgentTool::new_async(
        ToolDefinition::new("slow_tool", "Wait for cancellation.", json!({})),
        |context| {
            Box::pin(async move {
                context.cancellation_token().cancelled().await;
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok(context.result("should-not-finish"))
            })
        },
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider))
        .with_tool(fast_tool)
        .with_tool(slow_tool)
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;
    let cancellation = CancellationToken::new();
    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "run partial batch")
        .with_cancellation_token(cancellation.clone())
        .stream();

    let mut saw_finished_result = false;
    while let Some(event) = stream.next().await {
        let event = event.expect("agent event");
        if let TimelineContent::ToolResult { result } = &event.content
            && result.call_id == "finished-call"
        {
            saw_finished_result = true;
            cancellation.cancel();
            break;
        }
    }
    assert!(
        saw_finished_result,
        "first tool should finish before cancellation"
    );
    drain(stream).await;

    let snapshot = store
        .load(&session_id)
        .await
        .expect("stored session")
        .snapshot();
    assert_eq!(snapshot.turn, 1, "interrupted batch must close turn");
    assert!(snapshot.pending_work.iter().any(|work| {
        work.id == "finished-call"
            && matches!(
                &work.status,
                waltr_agent::PendingWorkStatus::Finished(result)
                    if result.content == "fast-result"
            )
    }));
    assert!(snapshot.pending_work.iter().any(|work| {
        work.id == "pending-call"
            && matches!(
                work.status,
                waltr_agent::PendingWorkStatus::Canceled(
                    waltr_agent::CancelReason::SessionInterrupted
                )
            )
    }));
}

#[tokio::test]
async fn cancellation_token_stops_running_tool_without_finishing_result() {
    let provider = ScriptedProvider::from_turns(
        "cancel_tool_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "cancel-call",
                "cancel_tool",
                json!({}),
            ))],
            vec![CompletionEvent::Finished],
        ],
    );
    let tool = AgentTool::new_async(
        ToolDefinition::new("cancel_tool", "Wait for cancellation.", json!({})),
        |context| {
            Box::pin(async move {
                context.cancellation_token().cancelled().await;
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok(context.result("should-not-finish"))
            })
        },
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider))
        .with_tool(tool)
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .expect("created session")
        .id()
        .clone();
    let cancellation = CancellationToken::new();
    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "run cancel tool")
        .with_cancellation_token(cancellation.clone())
        .stream();

    let first = stream
        .next()
        .await
        .expect("tool call event")
        .expect("tool call event");
    assert!(crate::session::channel_is(&first, CHANNEL_TOOL_CALL));
    cancellation.cancel();
    let remaining = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("agent event"))
        .collect::<Vec<_>>();

    assert!(
        remaining
            .iter()
            .all(|event| !crate::session::channel_is(event, CHANNEL_TOOL_RESULT)),
        "canceled run should not finish the running tool: {remaining:#?}"
    );
    let record = store.load(&session_id).await.expect("stored session");
    assert!(record.snapshot().pending_work.iter().any(|work| {
        work.id == "cancel-call"
            && matches!(
                work.status,
                waltr_agent::PendingWorkStatus::Canceled(
                    waltr_agent::CancelReason::SessionInterrupted
                )
            )
    }));
    assert!(matches!(
        record.snapshot().status,
        waltr_agent::SessionStatus::Suspended(waltr_agent::Suspension::Interrupted { .. })
    ));
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn shell_worker_root_replaces_implicit_config_workdir() {
    let root = tempfile::tempdir().unwrap();
    let sandbox = TestSandbox::new(root.path()).await;
    let provider = Arc::new(ScriptedProvider::new("shell-root-precedence", |request| {
        if request.tool_results().next().is_some() {
            vec![Ok(CompletionEvent::Finished)]
        } else {
            vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "shell-root",
                "shell",
                json!({"command":"pwd", "description":"check root"}),
            )))]
        }
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_permission_policy(PermissionPolicy::default())
        .with_shell_tool(ShellConfig::new(root.path().join("incorrect-root")))
        .build();
    let events = collect_events(agent.prompt("check root").stream()).await;
    let canonical = root.path().canonicalize().unwrap();
    assert!(events.iter().any(|event| matches!(&event.content,
        TimelineContent::ToolResult { result } if result.content.contains(&canonical.display().to_string()) && !result.is_error
    )), "{events:#?}");
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn shell_default_workdir_claims_gate_execution() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sandbox = TestSandbox::new(root.path()).await;
    let provider = Arc::new(ScriptedProvider::new("shell-default-claims", |_| {
        vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
            "shell-claim",
            "shell",
            json!({"command":"printf denied > ./note", "description":"check claim"}),
        )))]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_permission_policy(PermissionPolicy::filesystem_external(
            root.path(),
            waltr_agent::PermissionEffect::ask(),
        ))
        .with_shell_tool(ShellConfig::new(root.path()).with_default_workdir(outside.path()))
        .build();
    let events = collect_events(agent.prompt("check shell permissions").stream()).await;
    let expected = outside.path().canonicalize().unwrap().join("note");
    assert!(events.iter().any(|event| matches!(&event.content,
        TimelineContent::ExternalRequest { request } if request.prompt.contains(&expected.display().to_string())
    )), "{events:#?}");
    assert!(!expected.exists());
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn shell_executes_in_sandbox_with_workdir_timeout_and_exit_status() {
    let root = tempfile::tempdir().expect("workspace");
    let root_path = root.path().canonicalize().expect("canonical workspace");
    fs::create_dir(root_path.join("subdir")).expect("workdir");
    let sandbox = TestSandbox::new(&root_path).await;
    let calls = [
        json!({"command":"pwd; printf waltr-shell-ok","description":"check default workdir"}),
        json!({"command":"pwd; exit 7","description":"check root-relative override","workdir":"."}),
        json!({"command":"sleep 3","description":"check default timeout"}),
        json!({"command":"printf '%01000d' 1","description":"check output cap","workdir":".","timeout":2}),
    ];
    let provider = Arc::new(ScriptedProvider::new(
        "sandbox-shell-operations",
        move |request| {
            let index = request.tool_results().count();
            calls.get(index).map_or_else(
                || vec![Ok(CompletionEvent::Finished)],
                |arguments| {
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        format!("shell-{index}"),
                        "shell",
                        arguments.clone(),
                    )))]
                },
            )
        },
    ));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_permission_policy(PermissionPolicy::default())
        .with_shell_tool(
            ShellConfig::new(&root_path)
                .with_default_workdir("subdir")
                .with_default_timeout(Duration::from_millis(500))
                .with_max_output_bytes(256),
        )
        .build();
    let events = collect_events(agent.prompt("run sandbox shell tools").stream()).await;
    let results = events
        .iter()
        .filter_map(|event| match &event.content {
            TimelineContent::ToolResult { result } => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 4, "{results:#?}");
    assert!(
        results[0]
            .content
            .contains(&root_path.join("subdir").display().to_string()),
        "{results:#?}"
    );
    assert!(results[0].content.contains("waltr-shell-ok"), "{results:#?}");
    assert!(results[1].content.contains("Exit code: 7"), "{results:#?}");
    assert!(
        results[1]
            .content
            .contains(&format!("{}\n", root_path.display())),
        "{results:#?}"
    );
    assert!(
        results[2].content.contains("Timed out: true"),
        "{results:#?}"
    );
    assert!(
        results[3].content.contains("[output truncated]"),
        "{results:#?}"
    );
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn shell_cancellation_kills_running_command() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sandbox = TestSandbox::new(temp.path()).await;
    let marker = temp.path().join("shell-cancel-marker.txt");
    let command = format!("(sleep 5; printf foo > {}) & wait", marker.display());
    let provider = ScriptedProvider::from_turns(
        "shell_cancel_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "shell-cancel-call",
                "shell",
                json!({
                    "command": command,
                    "description": "delayed marker write",
                    "workdir": temp.path().display().to_string(),
                }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider))
        .with_sandbox(sandbox.handle())
        .with_permission_policy(PermissionPolicy::default())
        .with_shell_tool(ShellConfig::new(temp.path()))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .expect("created session")
        .id()
        .clone();
    let cancellation = CancellationToken::new();
    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "run shell cancel command")
        .with_cancellation_token(cancellation.clone())
        .stream();

    let first = stream
        .next()
        .await
        .expect("tool call event")
        .expect("tool call event");
    assert!(crate::session::channel_is(&first, CHANNEL_TOOL_CALL));

    tokio::select! {
        event = stream.next() => panic!("shell command finished before cancellation: {event:#?}"),
        () = tokio::time::sleep(Duration::from_secs(1)) => cancellation.cancel(),
    }

    let remaining = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("agent event"))
        .collect::<Vec<_>>();
    tokio::time::sleep(Duration::from_secs(5)).await;

    assert!(
        remaining
            .iter()
            .all(|event| !crate::session::channel_is(event, CHANNEL_TOOL_RESULT)),
        "canceled run should not finish the shell tool: {remaining:#?}"
    );
    assert!(
        !marker.exists() || fs::read_to_string(&marker).expect("marker read").is_empty(),
        "canceled shell command should not write marker"
    );
    let record = store.load(&session_id).await.expect("stored session");
    assert!(
        record.snapshot().pending_work.iter().any(|work| {
            work.id == "shell-cancel-call"
                && matches!(
                    work.status,
                    waltr_agent::PendingWorkStatus::Canceled(
                        waltr_agent::CancelReason::SessionInterrupted
                    )
                )
        }),
        "{:?}",
        record.snapshot().pending_work
    );
}

#[tokio::test]
async fn agents_system_prompt_is_sent_to_provider() {
    let temp = TempRoot::new("waltr-agent-static-agents-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write(AGENTS_INSTRUCTION_FILE, "Prefer concise answers.\n");
    let provider = AgentsSystemPromptProvider {
        expected_path: temp.path().join(AGENTS_INSTRUCTION_FILE),
    };
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider))
        .with_sandbox(sandbox.handle())
        .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
        .build();

    let mut events = agent.prompt("hello").stream();
    let mut text = String::new();
    while let Some(event) = events.next().await {
        let event = event.expect("agent event");
        if let TimelineContent::TextDelta { delta } = &event.content
            && crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT)
        {
            text.push_str(delta);
        }
    }

    assert_eq!(text, "ok");
}

#[tokio::test]
async fn agents_system_prompt_expands_file_references() {
    let temp = TempRoot::new("waltr-agent-agents-template-file-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write(AGENTS_INSTRUCTION_FILE, "Follow @CONTRIBUTING.md\n");
    temp.write("CONTRIBUTING.md", "template included instructions\n");
    let provider = Arc::new(ScriptedProvider::new("agents-template-file", |request| {
        let system_prompt = request.system_prompt.expect("system prompt");
        assert!(
            system_prompt.contains(
                "<template-result kind=\"file\" tool=\"read\" source=\"CONTRIBUTING.md\">"
            ) && system_prompt.contains("template included instructions"),
            "{system_prompt}"
        );
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(FilesystemConfig::new(temp.path()))
        .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
        .build();

    drain(agent.prompt("hello").stream()).await;
}

#[tokio::test]
async fn user_prompt_expands_file_references_and_persists_rendered_prompt() {
    let temp = TempRoot::new("waltr-agent-user-template-file-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write("prompt-fragment.md", "rendered user file\n");
    let file_ref = "@prompt-fragment.md";
    let provider = Arc::new(ScriptedProvider::new("user-template-file", |request| {
        assert!(
            request
                .prompt
                .contains("<template-result kind=\"file\" tool=\"read\""),
            "{request:?}"
        );
        assert!(request.prompt.contains("rendered user file"), "{request:?}");
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(FilesystemConfig::new(temp.path()))
        .with_template_base_dir(temp.path())
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    drain(
        sessions
            .prompt(&agent, session_id.clone(), file_ref)
            .stream(),
    )
    .await;

    let record = store.load(&session_id).await.expect("stored session");
    let snapshot = record.snapshot();
    let prompt = snapshot.current_prompt.as_deref().expect("stored prompt");
    assert!(
        prompt.contains("<template-result kind=\"file\" tool=\"read\""),
        "{prompt}"
    );
    assert!(prompt.contains("rendered user file"), "{prompt}");
}

#[tokio::test]
async fn user_file_reference_uses_formatted_worker_read_not_raw_instructions() {
    let temp = TempRoot::new("waltr-agent-formatted-template-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write("long.md", &"line\n".repeat(2002));
    let provider = Arc::new(ScriptedProvider::new(
        "formatted-file-template",
        |request| {
            assert!(
                request.prompt.contains("<type>file</type>"),
                "{:?}",
                request.prompt
            );
            assert!(request.prompt.contains("2000: line"));
            assert!(!request.prompt.contains("2001: line"));
            assert!(request.prompt.contains("Use offset=2001 to continue."));
            vec![Ok(CompletionEvent::Finished)]
        },
    ));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(FilesystemConfig::new(temp.path()))
        .with_template_base_dir(temp.path())
        .build();
    drain(agent.prompt("@long.md").stream()).await;
}

#[tokio::test]
async fn user_prompt_keeps_non_file_at_mentions_literal() {
    let provider = Arc::new(ScriptedProvider::new("user-template-mentions", |request| {
        assert!(request.prompt.contains("foo@example.com"), "{request:?}");
        assert!(request.prompt.contains("@username"), "{request:?}");
        assert!(!request.prompt.contains("<template-error>"), "{request:?}");
        assert!(!request.prompt.contains("<template-result"), "{request:?}");
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider).build();

    drain(
        agent
            .prompt("email foo@example.com and mention @username")
            .stream(),
    )
    .await;
}

#[tokio::test]
async fn user_prompt_template_tool_errors_are_rendered_inline() {
    let provider = Arc::new(ScriptedProvider::new(
        "user-template-tool-error",
        |request| {
            assert!(
                request
                    .prompt
                    .contains("<template-result kind=\"tool\" tool=\"fail\" status=\"error\">")
            );
            assert!(
                request.prompt.contains("<arguments>{}</arguments>"),
                "{request:?}"
            );
            assert!(request.prompt.contains("Tool error:"), "{request:?}");
            assert!(request.prompt.contains("boom"), "{request:?}");
            vec![Ok(CompletionEvent::Finished)]
        },
    ));
    let tool = AgentTool::new(
        ToolDefinition::new("fail", "Fail template call.", json!({ "type": "object" })),
        |_call| {
            Err(LlmError::Protocol {
                provider: "test",
                message: "boom".to_owned(),
            })
        },
    );
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_tool(tool)
        .build();

    drain(agent.prompt("before @tool(fail, {}) after").stream()).await;
}

#[tokio::test]
async fn user_prompt_template_tool_respects_permission_denies() {
    let provider = Arc::new(ScriptedProvider::new(
        "user-template-permission-deny",
        |request| {
            assert!(
                request
                    .prompt
                    .contains("<template-result kind=\"tool\" tool=\"blocked\" status=\"error\">")
            );
            assert!(
                request.prompt.contains("<arguments>{}</arguments>"),
                "{request:?}"
            );
            assert!(
                request.prompt.contains("tool is not permitted: blocked"),
                "{request:?}"
            );
            assert!(!request.prompt.contains("blocked output"), "{request:?}");
            vec![Ok(CompletionEvent::Finished)]
        },
    ));
    let tool = AgentTool::new(
        ToolDefinition::new("blocked", "Blocked tool.", json!({ "type": "object" })),
        |call| Ok(call.result("blocked output")),
    );
    let policy = PermissionPolicy::default().with_tool_rule(
        PermissionToolSelector::exact("blocked"),
        PermissionEffect::deny(),
    );
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_tool(tool)
        .with_permission_policy(policy)
        .build();

    let events = collect_events(agent.prompt("@tool(blocked, {})").stream()).await;

    assert!(!events.iter().any(|event| matches!(
        event.content,
        TimelineContent::ToolCall { .. } | TimelineContent::ToolResult { .. }
    )));
}

#[tokio::test]
async fn user_prompt_template_tool_applies_nested_input_deny_before_invocation() {
    let invoked = Arc::new(Mutex::new(false));
    let observed = Arc::clone(&invoked);
    let provider = Arc::new(ScriptedProvider::new("template-input-deny", |request| {
        assert!(
            request
                .prompt
                .contains("tool input does not satisfy policy"),
            "{request:?}"
        );
        vec![Ok(CompletionEvent::Finished)]
    }));
    let tool = AgentTool::new(
        ToolDefinition::new("generic", "Generic tool.", json!({ "type": "object" })),
        move |call| {
            *observed.lock().unwrap() = true;
            Ok(call.result("should not run"))
        },
    );
    let policy = PermissionPolicy::default().with_input_rule(
        PermissionToolSelector::exact("generic"),
        PermissionInputRule::new(
            vec![PermissionInputCondition {
                pointer: PermissionJsonPointer::parse("/options/dryRun").unwrap(),
                value: PermissionInputValueMatcher::Exact(PermissionInputScalar::Bool(false)),
            }],
            PermissionEffect::deny(),
        )
        .unwrap(),
    );
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_tool(tool)
        .with_permission_policy(policy)
        .build();

    drain(
        agent
            .prompt("@tool(generic, {\"options\":{\"dryRun\":false}})")
            .stream(),
    )
    .await;
    assert!(!*invoked.lock().unwrap());
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn user_prompt_template_shell_asks_before_dispatch_and_runs_after_approval() {
    let root = TempRoot::new("waltr-agent-user-template-shell-root");
    let sandbox = TestSandbox::new(root.path()).await;
    let outside = TempRoot::new("waltr-agent-user-template-shell-outside");
    let side_effect = outside.path().join("unapproved.txt");
    let command = "touch ./unapproved.txt";
    let provider = Arc::new(ScriptedProvider::new(
        "user-template-permission-ask",
        |request| {
            assert!(
                request
                    .prompt
                    .contains("<template-result kind=\"tool\" tool=\"shell\">")
            );
            vec![Ok(CompletionEvent::Finished)]
        },
    ));
    let policy = PermissionPolicy::filesystem_external(root.path(), PermissionEffect::ask())
        .with_command_rule(
            PermissionToolSelector::exact("shell"),
            PermissionCommandPattern::Name("touch".to_owned()),
            PermissionEffect::ask(),
        );
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_shell_tool(ShellConfig::new(root.path()).with_default_workdir(outside.path()))
        .with_permission_policy(policy)
        .build();

    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let session_id = create_session_id(&sessions).await;
    let events = collect_events(
        sessions
            .prompt(&agent, session_id.clone(), format!(
                "@tool(shell, {{\"command\":{command:?},\"description\":\"Read the test fixture.\"}})"
            ))
            .stream(),
    )
    .await;

    let request = events
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request),
            _ => None,
        })
        .expect("contextual permission request");
    assert_eq!(request.kind, ExternalRequestKind::Permission);
    assert!(
        request.payload["claims"]
            .as_array()
            .is_some_and(|claims| !claims.is_empty())
    );
    assert!(!side_effect.exists(), "unapproved shell command ran");
    drain(
        sessions
            .resume(
                &agent,
                session_id,
                ResumeInput::ExternalResponse {
                    request_id: request.id.clone(),
                    response: ExternalResponse::new(json!("allow-once")),
                },
            )
            .stream(),
    )
    .await;
    assert!(side_effect.exists(), "approved shell command did not run");
}

#[tokio::test]
async fn user_prompt_template_workspace_write_ask_can_be_denied() {
    let root = TempRoot::new("waltr-agent-user-template-write-root");
    let sandbox = TestSandbox::new(root.path()).await;
    let target = root.path().join("unapproved.txt");
    let provider = Arc::new(ScriptedProvider::new("template-write-ask", |_| {
        panic!("provider must not see a denied template submission")
    }));
    let policy = PermissionPolicy::default().with_tool_rule(
        PermissionToolSelector::exact("write"),
        PermissionEffect::ask(),
    );
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(FilesystemConfig::new(root.path()))
        .with_permission_policy(policy)
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;
    let events = collect_events(
        sessions
            .prompt(
                &agent,
                session_id.clone(),
                "@tool(write, {\"filePath\":\"unapproved.txt\",\"content\":\"unsafe\"})",
            )
            .stream(),
    )
    .await;
    let request_id = events
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request.id.clone()),
            _ => None,
        })
        .expect("whole-tool permission request");
    assert!(!target.exists(), "unapproved write ran");
    drain(
        sessions
            .resume(
                &agent,
                session_id,
                ResumeInput::ExternalResponse {
                    request_id,
                    response: ExternalResponse::new(json!({"decision":"deny","message":"no"})),
                },
            )
            .stream(),
    )
    .await;
    assert!(!target.exists(), "denied write ran");
}

#[tokio::test]
async fn sandbox_template_worker_claim_deny_prevents_write() {
    let root = TempRoot::new("waltr-agent-template-claim-deny");
    let outside = TempRoot::new("waltr-agent-template-claim-deny-outside");
    let target = outside.path().join("denied.txt");
    let sandbox = TestSandbox::new(root.path()).await;
    let provider = Arc::new(ScriptedProvider::new("template-claim-deny", |request| {
        assert!(request.prompt.contains("status=\"error\""), "{request:?}");
        assert!(request.prompt.contains("permission denied"), "{request:?}");
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(FilesystemConfig::new(root.path()))
        .with_permission_policy(PermissionPolicy::filesystem_external(
            root.path(),
            PermissionEffect::deny(),
        ))
        .build();
    let prompt = format!(
        "@tool(write, {{\"filePath\":{:?},\"content\":\"denied\"}})",
        target.display().to_string()
    );
    drain(agent.prompt(prompt).stream()).await;
    assert!(!target.exists());
}

#[tokio::test]
async fn completed_sandbox_template_write_is_not_replayed_on_partial_turn_resume() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let root = tempfile::tempdir().unwrap();
    let sandbox = TestSandbox::new(root.path()).await;
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let provider = Arc::new(ScriptedProvider::new("template-drop", move |request| {
        observed.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            request
                .prompt
                .matches("<template-result kind=\"tool\" tool=\"write\">")
                .count(),
            2
        );
        vec![
            Ok(CompletionEvent::Delta("started".to_owned())),
            Ok(CompletionEvent::Finished),
        ]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(FilesystemConfig::new(root.path()))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = create_session_id(&sessions).await;
    let prompt = "@tool(write, {\"filePath\":\"once\",\"content\":\"first\"}) @tool(write, {\"filePath\":\"once\",\"content\":\"second\"})";
    let mut stream = sessions.prompt(&agent, id.clone(), prompt).stream();
    while let Some(event) = stream.next().await {
        let event = event.unwrap();
        if matches!(event.content, TimelineContent::TextDelta { .. }) {
            break;
        }
    }
    drop(stream);
    let resumed = collect_events(
        sessions
            .resume(&agent, id.clone(), ResumeInput::Continue)
            .stream(),
    )
    .await;
    assert!(
        resumed
            .iter()
            .all(|event| !matches!(event.content, TimelineContent::ToolResult { .. }))
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        fs::read_to_string(root.path().join("once")).unwrap(),
        "second"
    );
    let record = store.load(&id).await.unwrap();
    assert_eq!(record.snapshot().prompts.len(), 1);
    assert!(!record.journal.iter().any(|event| matches!(
        event,
        waltr_agent::SessionJournalEvent::SandboxDispatchStarted { .. }
    )));
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn disconnected_sandbox_template_renders_error_and_fresh_render_can_execute() {
    let root = TempRoot::new("waltr-agent-template-disconnect");
    let disconnected = TestSandbox::new(root.path()).await;
    let provider = Arc::new(ScriptedProvider::new("template-disconnected", |request| {
        assert!(request.prompt.contains("status=\"error\""), "{request:?}");
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(disconnected.handle())
        .with_shell_tool(ShellConfig::new(root.path()))
        .build();
    disconnected.disconnect();
    let prompt = "@tool(shell, {\"command\":\"pwd\",\"description\":\"Print workdir.\"})";
    drain(agent.prompt(prompt).stream()).await;

    let connected = TestSandbox::new(root.path()).await;
    let provider = Arc::new(ScriptedProvider::new("template-reconnected", |request| {
        assert!(
            request
                .prompt
                .contains("<template-result kind=\"tool\" tool=\"shell\">"),
            "{request:?}"
        );
        assert!(!request.prompt.contains("status=\"error\""), "{request:?}");
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(connected.handle())
        .with_shell_tool(ShellConfig::new(root.path()))
        .build();
    drain(agent.prompt(prompt).stream()).await;
}

#[tokio::test]
async fn nested_file_sandbox_template_executes_and_rerender_is_new_invocation() {
    let root = TempRoot::new("waltr-agent-template-nested-rerender");
    root.write(
        "nested.md",
        "@tool(write, {\"filePath\":\"marker\",\"content\":\"first\"})",
    );
    let sandbox = TestSandbox::new(root.path()).await;
    let provider = Arc::new(ScriptedProvider::new("nested-template", |request| {
        assert!(
            request.prompt.contains("<template-result kind=\"file\""),
            "{request:?}"
        );
        assert!(
            request
                .prompt
                .contains("<template-result kind=\"tool\" tool=\"write\">"),
            "{request:?}"
        );
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(FilesystemConfig::new(root.path()))
        .build();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = create_session_id(&sessions).await;
    let reference = format!("@\"{}\"", root.path().join("nested.md").display());
    drain(sessions.prompt(&agent, id.clone(), &reference).stream()).await;
    assert_eq!(
        fs::read_to_string(root.path().join("marker")).unwrap(),
        "first"
    );
    root.write(
        "nested.md",
        "@tool(write, {\"filePath\":\"marker\",\"content\":\"second\"})",
    );
    drain(sessions.prompt(&agent, id, &reference).stream()).await;
    assert_eq!(
        fs::read_to_string(root.path().join("marker")).unwrap(),
        "second"
    );
}

#[tokio::test]
async fn agents_instruction_template_read_does_not_reload_its_own_instruction() {
    let root = TempRoot::new("waltr-agent-instruction-template-reentrancy");
    root.write("src/main.rs", "fixture content\n");
    root.write(
        AGENTS_INSTRUCTION_FILE,
        "Inspect @tool(read, {\"filePath\":\"src/main.rs\"})\n",
    );
    let sandbox = TestSandbox::new(root.path()).await;
    let provider = Arc::new(ScriptedProvider::new("instruction-reentrancy", |request| {
        let system = request.system_prompt.expect("instruction prompt");
        assert_eq!(system.matches("Inspect ").count(), 1, "{system}");
        assert!(system.contains("fixture content"), "{system}");
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_agents_system_prompt(AgentsPromptConfig::new(root.path()))
        .with_filesystem_tools(FilesystemConfig::new(root.path()).with_agents_read_context())
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = create_session_id(&sessions).await;
    tokio::time::timeout(
        Duration::from_secs(5),
        drain(sessions.prompt(&agent, id.clone(), "hello").stream()),
    )
    .await
    .expect("instruction render should not recurse");
    let state = &store.load(&id).await.unwrap().snapshot().middleware_state["agents"];
    assert_eq!(state["loaded"].as_array().map(Vec::len), Some(1));
    assert_eq!(state["loaded"][0]["prompt"], true);
}

#[tokio::test]
async fn agents_instruction_template_releases_render_guard_on_permission_suspend() {
    let root = TempRoot::new("waltr-agent-instruction-template-suspend");
    root.write("src/main.rs", "approved fixture\n");
    root.write(
        AGENTS_INSTRUCTION_FILE,
        "Read @tool(read, {\"filePath\":\"src/main.rs\"})\n",
    );
    let sandbox = TestSandbox::new(root.path()).await;
    let provider = Arc::new(ScriptedProvider::new("instruction-after-ask", |request| {
        let system = request.system_prompt.expect("instruction prompt");
        assert_eq!(system.matches("Read ").count(), 1, "{system}");
        assert!(system.contains("approved fixture"), "{system}");
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_agents_system_prompt(AgentsPromptConfig::new(root.path()))
        .with_filesystem_tools(FilesystemConfig::new(root.path()).with_agents_read_context())
        .with_permission_policy(PermissionPolicy::default().with_tool_rule(
            PermissionToolSelector::exact("read"),
            PermissionEffect::ask(),
        ))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = create_session_id(&sessions).await;
    let events = collect_events(sessions.prompt(&agent, id.clone(), "hello").stream()).await;
    let request_id = events
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request.id.clone()),
            _ => None,
        })
        .expect("read approval request");
    drain(
        sessions
            .resume(
                &agent,
                id.clone(),
                ResumeInput::ExternalResponse {
                    request_id,
                    response: ExternalResponse::new(json!("allow-once")),
                },
            )
            .stream(),
    )
    .await;
    let state = &store.load(&id).await.unwrap().snapshot().middleware_state["agents"];
    assert_eq!(state["loaded"].as_array().map(Vec::len), Some(1));
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn sandbox_template_shell_timeout_renders_error() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let root = TempRoot::new("waltr-agent-template-timeout");
    let sandbox = TestSandbox::new(root.path()).await;
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let provider = Arc::new(ScriptedProvider::new("template-timeout", move |request| {
        observed.fetch_add(1, Ordering::SeqCst);
        assert!(
            request
                .prompt
                .contains("<template-result kind=\"tool\" tool=\"shell\" status=\"error\">"),
            "{request:?}"
        );
        assert!(
            request.prompt.to_lowercase().contains("timed out"),
            "{request:?}"
        );
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_shell_tool(ShellConfig::new(root.path()))
        .build();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = create_session_id(&sessions).await;
    let prompt = "@tool(shell, {\"command\":\"printf x >> marker; sleep 3\",\"description\":\"Wait.\",\"timeout\":1})";
    drain(sessions.prompt(&agent, id.clone(), prompt).stream()).await;
    drain(sessions.prompt(&agent, id, prompt).stream()).await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(
        fs::read_to_string(root.path().join("marker")).unwrap(),
        "xx"
    );
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn sandbox_template_result_survives_later_prompt_ask_in_same_batch() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let root = TempRoot::new("waltr-agent-template-batch-ask");
    let sandbox = TestSandbox::new(root.path()).await;
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = requests.clone();
    let provider = Arc::new(ScriptedProvider::new(
        "template-batch-ask",
        move |request| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                assert!(
                    request
                        .prompt
                        .contains("<template-result kind=\"tool\" tool=\"ordinary\">")
                );
            }
            assert!(
                request.prompt_turns.iter().any(|turn| turn
                    .prompt
                    .contains("<template-result kind=\"tool\" tool=\"shell\">")),
                "{request:?}"
            );
            vec![Ok(CompletionEvent::Finished)]
        },
    ));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_shell_tool(ShellConfig::new(root.path()))
        .with_tool(AgentTool::new(
            ToolDefinition::new("ordinary", "Test tool", json!({"type":"object"})),
            |call| Ok(call.result("approved")),
        ))
        .with_permission_policy(PermissionPolicy::default().with_tool_rule(
            PermissionToolSelector::exact("ordinary"),
            PermissionEffect::ask(),
        ))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = create_session_id(&sessions).await;
    let shell = "@tool(shell, {\"command\":\"printf x >> marker\",\"description\":\"Mark call.\"})";
    let first = collect_events(
        sessions
            .resume(
                &agent,
                id.clone(),
                ResumeInput::Prompts(vec![shell.to_owned(), "@tool(ordinary, {})".to_owned()]),
            )
            .stream(),
    )
    .await;
    let request_id = first
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request.id.clone()),
            _ => None,
        })
        .expect("second prompt asks permission");
    assert_eq!(fs::read_to_string(root.path().join("marker")).unwrap(), "x");
    assert!(store.load(&id).await.unwrap().snapshot().prompts.is_empty());
    drain(
        sessions
            .resume(
                &agent,
                id.clone(),
                ResumeInput::ExternalResponse {
                    request_id,
                    response: ExternalResponse::new(json!("allow-once")),
                },
            )
            .stream(),
    )
    .await;
    assert_eq!(fs::read_to_string(root.path().join("marker")).unwrap(), "x");
    assert_eq!(store.load(&id).await.unwrap().snapshot().prompts.len(), 2);
    drain(sessions.prompt(&agent, id, shell).stream()).await;
    assert_eq!(
        fs::read_to_string(root.path().join("marker")).unwrap(),
        "xx"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn user_prompt_template_ask_replays_submitted_prompt_once_after_approval() {
    let invoked = Arc::new(Mutex::new(false));
    let observed = invoked.clone();
    let provider = Arc::new(ScriptedProvider::new("ordinary-template-ask", |request| {
        assert!(request.prompt.starts_with("Please "), "{request:?}");
        assert!(request.prompt.contains("ordinary tool ran"), "{request:?}");
        assert!(request.prompt.ends_with(" today"), "{request:?}");
        vec![Ok(CompletionEvent::Finished)]
    }));
    let tool = AgentTool::new(
        ToolDefinition::new("ordinary", "Test tool", json!({"type":"object"})),
        move |call| {
            *observed.lock().unwrap() = true;
            Ok(call.result("ordinary tool ran"))
        },
    );
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_tool(tool)
        .with_permission_policy(PermissionPolicy::default().with_tool_rule(
            PermissionToolSelector::exact("ordinary"),
            PermissionEffect::ask(),
        ))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = create_session_id(&sessions).await;
    let original = "Please @tool(ordinary, {}) today";
    let events = collect_events(sessions.prompt(&agent, id.clone(), original).stream()).await;
    assert!(!*invoked.lock().unwrap());
    let request_id = events
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request.id.clone()),
            _ => None,
        })
        .expect("permission request");
    let pending = store.load(&id).await.unwrap().snapshot();
    assert!(pending.prompts.is_empty());
    assert_eq!(
        pending.middleware_state["pending_user_template"]["prompts"][0],
        original
    );
    assert!(
        collect_events(
            sessions
                .resume(&agent, id.clone(), ResumeInput::Continue)
                .stream()
        )
        .await
        .is_empty()
    );
    assert!(!*invoked.lock().unwrap());
    let approved = collect_events(
        sessions
            .resume(
                &agent,
                id.clone(),
                ResumeInput::ExternalResponse {
                    request_id,
                    response: ExternalResponse::new(json!("allow-once")),
                },
            )
            .stream(),
    )
    .await;
    assert!(
        *invoked.lock().unwrap(),
        "approved events: {approved:?}, snapshot: {:?}",
        store.load(&id).await.unwrap().snapshot()
    );
    let snapshot = store.load(&id).await.unwrap().snapshot();
    assert_eq!(snapshot.prompts.len(), 1);
    assert_eq!(snapshot.prompt_turns.len(), 1);
    assert!(snapshot.current_prompt.unwrap().starts_with("Please "));
}

#[tokio::test]
async fn identical_template_tool_occurrences_require_distinct_approvals() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let invoked = calls.clone();
    let tool = AgentTool::new(
        ToolDefinition::new("repeat", "Repeat.", json!({"type":"object"})),
        move |call| {
            invoked.fetch_add(1, Ordering::SeqCst);
            Ok(call.result("done"))
        },
    );
    let provider = Arc::new(ScriptedProvider::new("repeat-template", |request| {
        assert_eq!(
            request
                .prompt
                .matches("<template-result kind=\"tool\"")
                .count(),
            2
        );
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_tool(tool)
        .with_permission_policy(PermissionPolicy::default().with_tool_rule(
            PermissionToolSelector::exact("repeat"),
            PermissionEffect::ask(),
        ))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = create_session_id(&sessions).await;
    let prompt = "@tool(repeat, {}) and @tool(repeat, {})";
    let first = collect_events(sessions.prompt(&agent, id.clone(), prompt).stream()).await;
    let first_id = first
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request.id.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let second = collect_events(
        sessions
            .resume(
                &agent,
                id.clone(),
                ResumeInput::ExternalResponse {
                    request_id: first_id.clone(),
                    response: ExternalResponse::new(json!("allow-once")),
                },
            )
            .stream(),
    )
    .await;
    let second_id = second
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request.id.clone()),
            _ => None,
        })
        .expect("second occurrence needs approval");
    assert_ne!(first_id, second_id);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drain(
        sessions
            .resume(
                &agent,
                id.clone(),
                ResumeInput::ExternalResponse {
                    request_id: second_id,
                    response: ExternalResponse::new(json!("allow-once")),
                },
            )
            .stream(),
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(store.load(&id).await.unwrap().snapshot().prompts.len(), 1);
}

#[tokio::test]
async fn denied_template_approval_does_not_send_empty_prompt_or_authorize_later_input() {
    let count = Arc::new(Mutex::new(0));
    let seen = count.clone();
    let provider = Arc::new(ScriptedProvider::new("denied-template", move |request| {
        assert_eq!(request.prompt, "replacement");
        *seen.lock().unwrap() += 1;
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_tool(AgentTool::new(
            ToolDefinition::new("ask", "Ask.", json!({"type":"object"})),
            |_call| panic!("denied tool must not run"),
        ))
        .with_permission_policy(PermissionPolicy::default().with_tool_rule(
            PermissionToolSelector::exact("ask"),
            PermissionEffect::ask(),
        ))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = create_session_id(&sessions).await;
    let first = collect_events(
        sessions
            .prompt(&agent, id.clone(), "@tool(ask, {})")
            .stream(),
    )
    .await;
    let request_id = first
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request.id.clone()),
            _ => None,
        })
        .unwrap();
    collect_events(
        sessions
            .resume(
                &agent,
                id.clone(),
                ResumeInput::ExternalResponse {
                    request_id,
                    response: ExternalResponse::new(json!({"decision":"deny","message":"no"})),
                },
            )
            .stream(),
    )
    .await;
    assert!(
        collect_events(
            sessions
                .resume(&agent, id.clone(), ResumeInput::Continue)
                .stream()
        )
        .await
        .is_empty()
    );
    assert_eq!(*count.lock().unwrap(), 0);
    assert!(store.load(&id).await.unwrap().snapshot().prompts.is_empty());
    drain(sessions.prompt(&agent, id.clone(), "replacement").stream()).await;
    assert_eq!(*count.lock().unwrap(), 1);
    assert_eq!(
        store.load(&id).await.unwrap().snapshot().prompts,
        ["replacement"]
    );
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn runtime_prompt_sandbox_template_asks_for_contextual_permission() {
    let root = TempRoot::new("waltr-agent-runtime-template-shell-root");
    let sandbox = TestSandbox::new(root.path()).await;
    let outside = TempRoot::new("waltr-agent-runtime-template-shell-outside");
    outside.write("secret.txt", "runtime ask should pause\n");
    let command = format!("cat {}", outside.path().join("secret.txt").display());
    root.write(
        AGENTS_INSTRUCTION_FILE,
        &format!(
            "Use fixture: @tool(shell, {{\"command\":{command:?},\"description\":\"Read the runtime fixture.\"}})\n"
        ),
    );
    let provider = Arc::new(ScriptedProvider::new(
        "runtime-template-permission-ask",
        |_| panic!("provider should wait for permission"),
    ));
    let policy = PermissionPolicy::default().with_command_rule(
        PermissionToolSelector::exact("shell"),
        PermissionCommandPattern::Name("cat".to_owned()),
        PermissionEffect::ask(),
    );
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_agents_system_prompt(AgentsPromptConfig::new(root.path()))
        .with_shell_tool(ShellConfig::new(root.path()))
        .with_permission_policy(policy)
        .build();

    let events = collect_events(agent.prompt("hello").stream()).await;

    let request = events
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request),
            _ => None,
        })
        .expect("contextual permission request");
    assert_eq!(request.kind, ExternalRequestKind::Permission);
    assert!(request.prompt.contains("cat"), "{request:?}");
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn runtime_prompt_sandbox_template_runs_after_approval() {
    let root = TempRoot::new("waltr-agent-runtime-template-resume-root");
    let sandbox = TestSandbox::new(root.path()).await;
    let outside = TempRoot::new("waltr-agent-runtime-template-resume-outside");
    outside.write("secret.txt", "runtime template approved\n");
    let command = format!("cat {}", outside.path().join("secret.txt").display());
    root.write(
        AGENTS_INSTRUCTION_FILE,
        &format!(
            "Use fixture: @tool(shell, {{\"command\":{command:?},\"description\":\"Read the runtime fixture.\"}})\n"
        ),
    );
    let provider = Arc::new(ScriptedProvider::new(
        "runtime-template-permission-resume",
        |request| {
            let system_prompt = request.system_prompt.expect("system prompt");
            assert!(
                system_prompt.contains("runtime template approved"),
                "{system_prompt}"
            );
            vec![Ok(CompletionEvent::Finished)]
        },
    ));
    let policy = PermissionPolicy::default().with_command_rule(
        PermissionToolSelector::exact("shell"),
        PermissionCommandPattern::Name("cat".to_owned()),
        PermissionEffect::ask(),
    );
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_agents_system_prompt(AgentsPromptConfig::new(root.path()))
        .with_shell_tool(ShellConfig::new(root.path()))
        .with_permission_policy(policy)
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    let events = collect_events(
        sessions
            .prompt(&agent, session_id.clone(), "hello")
            .stream(),
    )
    .await;
    let request_id = events
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request.id.clone()),
            _ => None,
        })
        .expect("contextual permission request");
    drain(
        sessions
            .resume(
                &agent,
                session_id.clone(),
                ResumeInput::ExternalResponse {
                    request_id,
                    response: ExternalResponse::new(json!("allow-once")),
                },
            )
            .stream(),
    )
    .await;
    assert_eq!(
        store
            .load(&session_id)
            .await
            .unwrap()
            .snapshot()
            .prompts
            .len(),
        1
    );
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn runtime_sandbox_template_whole_tool_and_contextual_ask_need_two_approvals() {
    let root = TempRoot::new("waltr-agent-runtime-template-two-stage-root");
    let sandbox = TestSandbox::new(root.path()).await;
    let marker = root.path().join("template-approved.txt");
    let command = format!("printf approved > {}", marker.display());
    root.write(
        AGENTS_INSTRUCTION_FILE,
        &format!(
            "Run fixture: @tool(shell, {{\"command\":{command:?},\"description\":\"Write the approval marker.\"}})\n"
        ),
    );
    let provider = Arc::new(ScriptedProvider::new(
        "runtime-template-two-stage-permission",
        |request| {
            assert!(request.system_prompt.unwrap().contains("Run fixture:"));
            vec![Ok(CompletionEvent::Finished)]
        },
    ));
    let policy = PermissionPolicy::default()
        .with_tool_rule(
            PermissionToolSelector::exact("shell"),
            PermissionEffect::ask(),
        )
        .with_command_rule(
            PermissionToolSelector::exact("shell"),
            PermissionCommandPattern::Name("printf".to_owned()),
            PermissionEffect::ask(),
        );
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_agents_system_prompt(AgentsPromptConfig::new(root.path()))
        .with_shell_tool(ShellConfig::new(root.path()))
        .with_permission_policy(policy)
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    let first = collect_events(
        sessions
            .prompt(&agent, session_id.clone(), "hello")
            .stream(),
    )
    .await;
    let whole = first
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request),
            _ => None,
        })
        .expect("whole-tool permission request");
    assert_eq!(whole.payload["claims"], json!([]));
    assert!(!marker.exists());
    let second = collect_events(
        sessions
            .resume(
                &agent,
                session_id.clone(),
                ResumeInput::ExternalResponse {
                    request_id: whole.id.clone(),
                    response: ExternalResponse::new(json!("allow-once")),
                },
            )
            .stream(),
    )
    .await;
    let contextual = second
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request),
            _ => None,
        })
        .expect("contextual permission request");
    assert_ne!(contextual.id, whole.id);
    assert!(
        contextual.payload["claims"]
            .as_array()
            .is_some_and(|claims| !claims.is_empty())
    );
    assert!(!marker.exists());
    drain(
        sessions
            .resume(
                &agent,
                session_id.clone(),
                ResumeInput::ExternalResponse {
                    request_id: contextual.id.clone(),
                    response: ExternalResponse::new(json!("allow-once")),
                },
            )
            .stream(),
    )
    .await;
    assert_eq!(fs::read_to_string(&marker).unwrap(), "approved");
    assert_eq!(
        store
            .load(&session_id)
            .await
            .unwrap()
            .snapshot()
            .prompts
            .len(),
        1
    );
}

#[tokio::test]
async fn user_prompt_template_tool_result_includes_arguments() {
    let provider = Arc::new(ScriptedProvider::new(
        "user-template-tool-arguments",
        |request| {
            assert!(
                request
                    .prompt
                    .contains("<template-result kind=\"tool\" tool=\"shell\">")
            );
            assert!(request.prompt.contains("<arguments>"), "{request:?}");
            assert!(
                request.prompt.contains("\"command\":\"pwd\""),
                "{request:?}"
            );
            assert!(
                request
                    .prompt
                    .contains("\"description\":\"Show the current working directory.\""),
                "{request:?}"
            );
            assert!(request.prompt.contains("<content>"), "{request:?}");
            assert!(request.prompt.contains("pwd output marker"), "{request:?}");
            vec![Ok(CompletionEvent::Finished)]
        },
    ));
    let tool = AgentTool::new(
        ToolDefinition::new("shell", "Run shell command.", json!({ "type": "object" })),
        |context| {
            assert_eq!(context.call().arguments["command"], "pwd");
            assert_eq!(
                context.call().arguments["description"],
                "Show the current working directory."
            );
            Ok(context.result("Exit code: 0\n\npwd output marker"))
        },
    );
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_tool(tool)
        .build();

    drain(
        agent
            .prompt(
                "@tool(shell, {\"command\":\"pwd\",\"description\":\"Show the current working directory.\"})",
            )
            .stream(),
    )
    .await;
}

#[tokio::test]
async fn filesystem_read_agents_context_is_opt_in_and_loaded_once() {
    let temp = TempRoot::new("waltr-agent-read-agents-test");
    temp.write("AGENTS.md", "Root instructions.\n");
    temp.write("src/AGENTS.md", "Nested instructions.\n");
    temp.write("src/main.rs", "fn main() {}\n");
    #[cfg(unix)]
    let workspace = {
        let link = temp.path().join("alias");
        std::os::unix::fs::symlink(temp.path(), &link).unwrap();
        link
    };
    #[cfg(not(unix))]
    let workspace = temp.path().to_path_buf();
    let sandbox = TestSandbox::new(&workspace).await;
    assert_eq!(
        sandbox.handle().canonical_root().unwrap(),
        temp.path().canonicalize().unwrap()
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(RepeatedReadProvider::default()))
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(FilesystemConfig::new(&workspace).with_agents_read_context())
        .build();

    let mut events = agent.prompt("read twice").stream();
    let mut tool_results = Vec::new();
    while let Some(event) = events.next().await {
        let event = event.expect("agent event");
        if let TimelineContent::ToolResult { result } = &event.content
            && crate::session::channel_is(&event, CHANNEL_TOOL_RESULT)
        {
            tool_results.push(result.content.clone());
        }
    }

    assert_eq!(tool_results.len(), 2);
    assert!(tool_results[0].contains("<system-reminder>"));
    assert!(tool_results[0].contains("Nested instructions."));
    assert!(tool_results[0].contains("Root instructions."));
    assert!(!tool_results[1].contains("<system-reminder>"));
}

#[tokio::test]
async fn filesystem_read_agents_context_normalizes_wrapped_model_path() {
    let temp = TempRoot::new("waltr-agent-read-agents-wrapped-path");
    temp.write("src/AGENTS.md", "Wrapped path instructions.\n");
    temp.write("src/main.rs", "fn main() {}\n");
    let sandbox = TestSandbox::new(temp.path()).await;
    let provider = Arc::new(ScriptedProvider::new("wrapped-read", |request| {
        if request.tool_results().next().is_some() {
            vec![Ok(CompletionEvent::Finished)]
        } else {
            vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "wrapped-read",
                "read",
                json!({"filePath": "<path>src/main.rs</path>"}),
            )))]
        }
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_agents_read_context())
        .build();
    let result = single_tool_result(agent.prompt("read wrapped path").stream()).await;
    assert!(result.contains("Wrapped path instructions."), "{result}");
}

#[tokio::test]
async fn filesystem_read_agents_context_is_disabled_by_default() {
    let temp = TempRoot::new("waltr-agent-read-agents-disabled-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write("AGENTS.md", "Root instructions.\n");
    temp.write("src/AGENTS.md", "Nested instructions.\n");
    temp.write("src/main.rs", "fn main() {}\n");
    let agent = AgentRuntime::builder(test_model(), Arc::new(SingleReadProvider))
        .with_sandbox(sandbox.handle())
        .with_filesystem_tools(FilesystemConfig::new(temp.path()))
        .build();

    let tool_result = single_tool_result(agent.prompt("read once").stream()).await;

    assert!(!tool_result.contains("<system-reminder>"));
    assert!(!tool_result.contains("Root instructions."));
    assert!(!tool_result.contains("Nested instructions."));
}

#[tokio::test]
async fn filesystem_read_agents_context_skips_static_root_instruction() {
    let temp = TempRoot::new("waltr-agent-read-static-agents-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write("AGENTS.md", "Root instructions.\n");
    temp.write("src/AGENTS.md", "Nested instructions.\n");
    temp.write("src/main.rs", "fn main() {}\n");
    let agent = AgentRuntime::builder(test_model(), Arc::new(SingleReadProvider))
        .with_sandbox(sandbox.handle())
        .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
        .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_agents_read_context())
        .build();

    let tool_result = single_tool_result(agent.prompt("read once").stream()).await;

    assert!(tool_result.contains("Nested instructions."));
    assert!(!tool_result.contains("Root instructions."));
}

#[tokio::test]
async fn skills_system_prompt_lists_root_skills_and_exposes_loader() {
    let temp = TempRoot::new("waltr-agent-skills-system-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write_skill(
        "example",
        "Use for example tasks.",
        "# Example\nFollow example steps.\n",
    );
    let provider = SkillsSystemPromptProvider {
        expected_root: temp.path().canonicalize().expect("canonical temp root"),
    };
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider))
        .with_sandbox(sandbox.handle())
        .with_skills(SkillsConfig::new(temp.path()))
        .build();

    drain(agent.prompt("hello").stream()).await;
}

#[tokio::test]
async fn configured_non_root_skills_are_initially_visible_only_under_configured_root() {
    let temp = TempRoot::new("waltr-agent-configured-skills-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write_skill("outer", "Outer skill.", "# Outer\n");
    temp.write(
        "packages/app/.agents/skills/inner/SKILL.md",
        "---\nname: inner\ndescription: Inner skill.\n---\n# Inner\n",
    );
    let app = temp.path().join("packages/app");
    let provider = Arc::new(ScriptedProvider::new("configured-skills", |request| {
        let prompt = request.system_prompt.expect("system prompt");
        assert!(prompt.contains("inner: Inner skill."), "{prompt}");
        assert!(!prompt.contains("outer: Outer skill."), "{prompt}");
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_sandbox(sandbox.handle())
        .with_skills(SkillsConfig::new(app))
        .build();
    drain(agent.prompt("hello").stream()).await;
}

#[tokio::test]
async fn load_skill_loads_content_and_lists_resources_once() {
    let temp = TempRoot::new("waltr-agent-load-skill-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write_skill(
        "example",
        "Use for example tasks.",
        "# Example\nFollow example steps.\n",
    );
    temp.write(".agents/skills/example/references/guide.md", "details\n");
    let agent = AgentRuntime::builder(
        test_model(),
        Arc::new(LoadSkillProvider::new(
            json!({ "source": "local", "name": "example" }),
        )),
    )
    .with_sandbox(sandbox.handle())
    .with_skills(SkillsConfig::new(temp.path()))
    .build();

    let tool_results = tool_results(agent.prompt("load skill").stream()).await;

    assert_eq!(tool_results.len(), 2);
    assert!(tool_results[0].contains("<skill_content name=\"example\">"));
    assert!(tool_results[0].contains("Follow example steps."));
    assert!(tool_results[0].contains("- references/guide.md"));
    assert!(
        !tool_results[0].contains(
            &temp
                .path()
                .join(".agents/skills/example/references/guide.md")
                .display()
                .to_string()
        )
    );
    assert!(tool_results[1].contains("already loaded"));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn load_skill_whole_tool_permission_precedes_middleware_execution() {
    let temp = TempRoot::new("waltr-agent-load-skill-permission-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write_skill(
        "example",
        "Use for example tasks.",
        "# Example\nSecret skill content.\n",
    );
    let provider = ScriptedProvider::new("load_skill_permission_provider", |request| {
        if request
            .tool_results()
            .any(|result| result.call_id == "load_ask")
        {
            vec![Ok(CompletionEvent::Finished)]
        } else {
            vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "load_ask",
                "load_skill",
                json!({ "source": "local", "name": "example" }),
            )))]
        }
    });
    let ask_agent = AgentRuntime::builder(test_model(), Arc::new(provider))
        .with_sandbox(sandbox.handle())
        .with_permission_policy(PermissionPolicy::default().with_tool_rule(
            PermissionToolSelector::exact("load_skill"),
            PermissionEffect::ask(),
        ))
        .with_skills(SkillsConfig::new(temp.path()))
        .build();

    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .expect("create session")
        .id()
        .clone();
    let events = sessions
        .prompt(&ask_agent, session_id.clone(), "load skill")
        .stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("agent event"))
        .collect::<Vec<_>>();
    assert!(events.iter().any(|event| matches!(
        &event.content,
        TimelineContent::ExternalRequest { request }
            if request.kind == ExternalRequestKind::Permission
                && request.tool_call_id.as_deref() == Some("load_ask")
    )));
    assert!(events.iter().all(|event| !matches!(
        &event.content,
        TimelineContent::ToolResult { result } if result.content.contains("Secret skill content")
    )));

    let request = events
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ExternalRequest { request } => Some(request),
            _ => None,
        })
        .expect("permission request");
    let resumed = sessions
        .resume(
            &ask_agent,
            session_id,
            ResumeInput::ExternalResponse {
                request_id: request.id.clone(),
                response: ExternalResponse::new(json!(waltr_agent::PermissionResponse::AllowOnce)),
            },
        )
        .stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("resumed event"))
        .collect::<Vec<_>>();
    assert_eq!(
        resumed
            .iter()
            .filter(|event| matches!(
                &event.content,
                TimelineContent::ToolResult { result }
                    if result.content.contains("Secret skill content")
            ))
            .count(),
        1
    );
    assert!(
        resumed
            .iter()
            .all(|event| !matches!(event.content, TimelineContent::ExternalRequest { .. }))
    );

    let allow_agent = AgentRuntime::builder(
        test_model(),
        Arc::new(LoadSkillProvider::new(
            json!({ "source": "local", "name": "example" }),
        )),
    )
    .with_sandbox(sandbox.handle())
    .with_permission_policy(PermissionPolicy::default().with_tool_rule(
        PermissionToolSelector::exact("load_skill"),
        PermissionEffect::allow(),
    ))
    .with_skills(SkillsConfig::new(temp.path()))
    .build();
    let results = tool_results(allow_agent.prompt("load skill").stream()).await;

    assert!(results[0].contains("Secret skill content"));
}

#[tokio::test]
async fn duplicate_skill_names_require_root() {
    let temp = TempRoot::new("waltr-agent-duplicate-skills-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write("packages/app/.ignore", "!.agents/\n");
    temp.write_skill("example", "Use root example.", "# Root Example\n");
    temp.write(
        "packages/app/.agents/skills/example/SKILL.md",
        "---\nname: example\ndescription: Use app example.\n---\n# App Example\n",
    );
    temp.write("src/main.rs", "fn main() {}\n");
    temp.write("packages/app/src/main.rs", "fn main() {}\n");
    let agent = AgentRuntime::builder(
        test_model(),
        Arc::new(DuplicateSkillProvider {
            root: temp.path().to_path_buf(),
        }),
    )
    .with_sandbox(sandbox.handle())
    .with_skills(SkillsConfig::new(temp.path()))
    .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
    .build();

    let tool_results = tool_results(agent.prompt("read and load").stream()).await;

    assert_eq!(tool_results.len(), 2);
    assert!(
        tool_results[0].contains("New skills are available"),
        "{tool_results:#?}"
    );
    assert!(tool_results[0].contains("Use app example."));
    assert!(tool_results[1].contains("Tool error:"));
    assert!(
        tool_results[1].contains("skill is not visible"),
        "{tool_results:#?}"
    );
}

#[tokio::test]
async fn duplicate_skill_names_load_with_root() {
    let temp = TempRoot::new("waltr-agent-rooted-skill-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write("packages/app/.ignore", "!.agents/\n");
    temp.write_skill("example", "Use root example.", "# Root Example\n");
    temp.write(
        "packages/app/.agents/skills/example/SKILL.md",
        "---\nname: example\ndescription: Use app example.\n---\n# App Example\n",
    );
    temp.write("packages/app/src/main.rs", "fn main() {}\n");
    let app_root = temp
        .path()
        .join("packages/app")
        .canonicalize()
        .expect("canonical app root");
    let agent = AgentRuntime::builder(
        test_model(),
        Arc::new(RootedSkillProvider { root: app_root }),
    )
    .with_sandbox(sandbox.handle())
    .with_skills(SkillsConfig::new(temp.path()))
    .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
    .build();

    let tool_results = tool_results(agent.prompt("read and load").stream()).await;

    assert_eq!(tool_results.len(), 2);
    assert!(
        tool_results[1].contains("<skill_content name=\"example\">"),
        "{tool_results:#?}"
    );
    assert!(tool_results[1].contains("# App Example"));
    assert!(!tool_results[1].contains("# Root Example"));
}

#[tokio::test]
async fn glob_directory_walk_reveals_skills_once() {
    let temp = TempRoot::new("waltr-agent-glob-skills-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write(
        "packages/app/.agents/skills/app-skill/SKILL.md",
        "---\nname: app-skill\ndescription: Use app skill.\n---\n# App Skill\n",
    );
    temp.write("packages/app/src/main.rs", "fn main() {}\n");
    let agent = AgentRuntime::builder(test_model(), Arc::new(RepeatedGlobProvider::default()))
        .with_sandbox(sandbox.handle())
        .with_skills(SkillsConfig::new(temp.path()))
        .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
        .build();

    let tool_results = tool_results(agent.prompt("glob twice").stream()).await;

    assert_eq!(tool_results.len(), 2);
    assert!(
        tool_results[0].contains("New skills are available"),
        "{tool_results:#?}"
    );
    assert!(tool_results[0].contains("app-skill: Use app skill."));
    assert!(!tool_results[1].contains("New skills are available"));
}

#[tokio::test]
async fn dynamic_skills_discovery_is_opt_in() {
    let temp = TempRoot::new("waltr-agent-skills-disabled-test");
    let sandbox = TestSandbox::new(temp.path()).await;
    temp.write(
        "packages/app/.agents/skills/example/SKILL.md",
        "---\nname: example\ndescription: Use app example.\n---\n# App Example\n",
    );
    temp.write("src/main.rs", "fn main() {}\n");
    temp.write("packages/app/src/main.rs", "fn main() {}\n");
    let agent = AgentRuntime::builder(test_model(), Arc::new(SingleReadProvider))
        .with_sandbox(sandbox.handle())
        .with_skills(SkillsConfig::new(temp.path()))
        .with_filesystem_tools(FilesystemConfig::new(temp.path()))
        .build();

    let tool_result = single_tool_result(agent.prompt("read once").stream()).await;

    assert!(!tool_result.contains("New skills are available"));
    assert!(!tool_result.contains("Use app example."));
}

#[tokio::test]
async fn agent_system_prompt_fragments_are_sent_in_order() {
    let agent = AgentRuntime::builder(test_model(), Arc::new(SystemPromptProvider))
        .with_base_system_prompt()
        .with_system_date("2026-06-02")
        .with_filesystem_system_prompt(FilesystemPromptConfig::new("/tmp/waltr-root"))
        .with_system_prompt("custom fragment")
        .build();

    let mut events = agent
        .prompt("hello")
        .with_system_prompt_fragment("run fragment")
        .stream();
    let mut text = String::new();
    while let Some(event) = events.next().await {
        let event = event.expect("agent event");
        if let TimelineContent::TextDelta { delta } = &event.content
            && crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT)
        {
            text.push_str(delta);
        }
    }

    assert_eq!(text, "ok");
}

#[tokio::test]
async fn tool_errors_are_reported_to_agent_for_recovery() {
    let tool = AgentTool::new(
        ToolDefinition::new(
            "fail_once",
            "Always fails.",
            json!({ "type": "object", "additionalProperties": false }),
        ),
        |_call| {
            Err(LlmError::Protocol {
                provider: "test_tool",
                message: "validation failed".to_owned(),
            })
        },
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(RecoveringProvider))
        .with_tool(tool)
        .build();

    let mut events = agent.prompt("recover").stream();
    let mut saw_error_result = false;
    let mut final_text = String::new();
    let mut timeline = Vec::new();

    while let Some(event) = events.next().await {
        let event = event.expect("agent event");
        match &event.content {
            TimelineContent::ToolResult { result }
                if crate::session::channel_is(&event, CHANNEL_TOOL_RESULT) =>
            {
                saw_error_result = result.content.contains("Tool error:")
                    && result.content.contains("validation failed");
            }
            TimelineContent::TextDelta { delta }
                if crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT) =>
            {
                final_text.push_str(delta);
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
        timeline.push(event);
    }

    assert!(saw_error_result);
    assert_eq!(final_text, "recovered");
    assert_tool_call_precedes_parented_result(&timeline, "call_1");
}

#[tokio::test]
async fn agent_surfaces_and_stores_thinking_without_final_text_pollution() {
    let agent = AgentRuntime::builder(test_model(), Arc::new(ThinkingProvider)).build();
    let mut events = agent.prompt("think").stream();
    let mut saw_thinking_delta = false;
    let mut final_text = String::new();
    let mut saw_finished = false;

    while let Some(event) = events.next().await {
        let event = event.expect("agent event");
        match &event.content {
            TimelineContent::TextDelta { delta }
                if crate::session::channel_is(&event, CHANNEL_THINKING) =>
            {
                saw_thinking_delta = delta == "hidden thought";
            }
            TimelineContent::TextDelta { delta }
                if crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT) =>
            {
                final_text.push_str(delta);
            }
            TimelineContent::Empty
                if event.kind == TimelineEventKind::SessionStatusChanged
                    && event.status == Some(TimelineStatus::Finished) =>
            {
                saw_finished = true;
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

    assert!(saw_thinking_delta);
    assert_eq!(final_text, "final answer");
    assert!(
        saw_finished,
        "stream should emit finished status timeline event"
    );
}

#[tokio::test]
async fn agent_replays_thinking_blocks_on_tool_continuation() {
    let provider = Arc::new(ReplayThinkingProvider::default());
    let tool = AgentTool::new(
        ToolDefinition::new(
            "lookup",
            "Look up test data.",
            json!({ "type": "object", "additionalProperties": false }),
        ),
        |call| Ok(call.result("tool output")),
    );
    let agent = AgentRuntime::builder(test_model(), provider.clone())
        .with_tool(tool)
        .build();

    drain(agent.prompt("use tool").stream()).await;

    assert!(provider.saw_replayed_thinking());
}

#[tokio::test]
async fn agent_replays_assistant_text_on_tool_continuation() {
    let provider = Arc::new(ReplayAssistantTextProvider::default());
    let tool = AgentTool::new(
        ToolDefinition::new(
            "lookup",
            "Look up test data.",
            json!({ "type": "object", "additionalProperties": false }),
        ),
        |call| Ok(call.result("tool output")),
    );
    let agent = AgentRuntime::builder(test_model(), provider.clone())
        .with_tool(tool)
        .build();

    drain(agent.prompt("use tool").stream()).await;

    assert!(provider.saw_replayed_assistant_text());
}

fn test_model() -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses)
}

fn test_inference_settings() -> InferenceSettings {
    InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: "test-model".to_owned(),
            model_cost: None,
            headers: std::collections::BTreeMap::default(),
            oauth: None,
            reasoning_effort: None,
        },
    }
}

async fn create_session_id<S>(sessions: &waltr_agent::SessionCoordinator<S>) -> waltr_agent::SessionId
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .expect("created session")
        .id()
        .clone()
}

async fn drain<S>(mut stream: S)
where
    S: futures_util::Stream<Item = waltr_agent::Result<AgentTimelineEvent>> + Unpin,
{
    while let Some(event) = stream.next().await {
        event.expect("agent event");
    }
}

async fn collect_events<S>(mut stream: S) -> Vec<AgentTimelineEvent>
where
    S: futures_util::Stream<Item = waltr_agent::Result<AgentTimelineEvent>> + Unpin,
{
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.expect("agent event"));
    }
    events
}

fn copy_replay_sequence(request: &CompletionRequest<'_>) -> Vec<String> {
    request
        .replay_items()
        .into_iter()
        .map(|item| match item {
            CompletionReplayItem::Prompt(prompt) => format!("user:{prompt}"),
            CompletionReplayItem::Turn(TurnItem::AssistantText(text)) => {
                format!("assistant:{text}")
            }
            CompletionReplayItem::Turn(TurnItem::ToolCall(call)) => {
                format!("tool-call:{}", call.id)
            }
            CompletionReplayItem::Turn(TurnItem::ToolResult(result)) => {
                format!("tool-result:{}", result.call_id)
            }
            CompletionReplayItem::Turn(TurnItem::Thinking(_)) => "thinking".to_owned(),
        })
        .collect()
}

fn assistant_text(events: &[AgentTimelineEvent]) -> String {
    events
        .iter()
        .filter_map(|event| {
            if crate::session::channel_is(event, CHANNEL_ASSISTANT_TEXT)
                && let TimelineContent::TextDelta { delta } = &event.content
            {
                return Some(delta.as_str());
            }
            None
        })
        .collect()
}

fn test_retry_config(max_attempts: u32) -> ProviderRetryConfig {
    ProviderRetryConfig {
        max_attempts,
        initial_delay: Duration::ZERO,
        fallback_delay_cap: Duration::ZERO,
        provider_delay_cap: Duration::ZERO,
        ..ProviderRetryConfig::default()
    }
}

fn retryable_api_error(message: &str) -> LlmError {
    LlmError::Api {
        provider: "test",
        message: message.to_owned(),
        status: Some(429),
        headers: Vec::new(),
        code: None,
        body: None,
    }
}

#[derive(Default)]
struct RequestOpenRetryProvider {
    requests: Mutex<u32>,
}

impl RequestOpenRetryProvider {
    fn requests(&self) -> u32 {
        *self.requests.lock().expect("request count")
    }
}

impl LlmStream for RequestOpenRetryProvider {
    fn name(&self) -> &'static str {
        "request-open-retry"
    }

    fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let mut requests = self.requests.lock().expect("request count");
            let request = *requests;
            *requests += 1;
            drop(requests);
            if request == 0 {
                return Err(retryable_api_error("request rate limited"));
            }
            Ok(Box::pin(stream::iter([
                Ok(CompletionEvent::Delta("request retry recovered".to_owned())),
                Ok(CompletionEvent::Finished),
            ])) as BoxCompletionStream)
        })
    }
}

#[derive(Default)]
struct AlwaysFailingProvider {
    requests: Mutex<u32>,
}

impl AlwaysFailingProvider {
    fn requests(&self) -> u32 {
        *self.requests.lock().expect("request count")
    }
}

impl LlmStream for AlwaysFailingProvider {
    fn name(&self) -> &'static str {
        "always-failing"
    }

    fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            *self.requests.lock().expect("request count") += 1;
            Err(retryable_api_error("rate limited until exhausted"))
        })
    }
}

async fn tool_results<S>(mut stream: S) -> Vec<String>
where
    S: futures_util::Stream<Item = waltr_agent::Result<AgentTimelineEvent>> + Unpin,
{
    let mut results = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("agent event");
        if let TimelineContent::ToolResult { result } = &event.content
            && crate::session::channel_is(&event, CHANNEL_TOOL_RESULT)
        {
            results.push(result.content.to_string());
        }
    }
    results
}

async fn single_tool_result<S>(stream: S) -> String
where
    S: futures_util::Stream<Item = waltr_agent::Result<AgentTimelineEvent>> + Unpin,
{
    tool_results(stream).await.pop().expect("tool result")
}

fn assert_tool_call_precedes_parented_result(events: &[AgentTimelineEvent], call_id: &str) {
    let call_index = events
        .iter()
        .position(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_CALL)
                && matches!(&event.content, TimelineContent::ToolCall { call } if call.id == call_id)
        })
        .expect("tool-call preview event");
    let result_index = events
        .iter()
        .position(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_RESULT)
                && matches!(&event.content, TimelineContent::ToolResult { result } if result.call_id == call_id)
        })
        .expect("tool-result event");

    let call = &events[call_index];
    let result = &events[result_index];
    assert!(
        call_index < result_index,
        "tool call must stream before result"
    );
    assert_eq!(call.kind, TimelineEventKind::ItemStarted);
    assert_eq!(result.kind, TimelineEventKind::ItemFinished);
    assert_eq!(result.parent_id.as_ref(), Some(&call.item_id));
    assert!(call.cursor < result.cursor);
}

#[derive(Default)]
struct DurableResumeProvider {
    requests: Mutex<usize>,
    saw_resumed_tool_result: Mutex<bool>,
}

#[derive(Default)]
struct ChronologyProvider {
    requests: Mutex<usize>,
    replay_sequences: Mutex<Vec<Vec<String>>>,
}

#[derive(Default)]
struct PendingSupersedeProvider {
    requests: Mutex<usize>,
    replay_sequences: Mutex<Vec<Vec<String>>>,
}

impl ChronologyProvider {
    fn replay_sequences(&self) -> Vec<Vec<String>> {
        self.replay_sequences
            .lock()
            .expect("replay sequence lock")
            .clone()
    }
}

impl LlmStream for ChronologyProvider {
    fn name(&self) -> &'static str {
        "chronology_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let sequence = request
            .replay_items()
            .into_iter()
            .filter_map(|item| match item {
                CompletionReplayItem::Prompt(prompt) => Some(format!("user:{prompt}")),
                CompletionReplayItem::Turn(TurnItem::AssistantText(text)) => {
                    Some(format!("assistant:{text}"))
                }
                CompletionReplayItem::Turn(_) => None,
            })
            .collect::<Vec<_>>();
        self.replay_sequences
            .lock()
            .expect("replay sequence lock")
            .push(sequence);

        let mut requests = self.requests.lock().expect("request lock");
        let events = match *requests {
            0 => vec![
                Ok(CompletionEvent::Delta("first answer".to_owned())),
                Ok(CompletionEvent::Finished),
            ],
            1 => vec![
                Ok(CompletionEvent::Delta("second answer".to_owned())),
                Ok(CompletionEvent::Finished),
            ],
            request => panic!("unexpected provider request {request}"),
        };
        *requests += 1;
        drop(requests);
        Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
    }
}

impl PendingSupersedeProvider {
    fn replay_sequences(&self) -> Vec<Vec<String>> {
        self.replay_sequences
            .lock()
            .expect("replay sequence lock")
            .clone()
    }
}

impl LlmStream for PendingSupersedeProvider {
    fn name(&self) -> &'static str {
        "pending_supersede_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let sequence = request
            .replay_items()
            .into_iter()
            .filter_map(|item| match item {
                CompletionReplayItem::Prompt(prompt) => Some(format!("user:{prompt}")),
                CompletionReplayItem::Turn(_) => None,
            })
            .collect::<Vec<_>>();
        self.replay_sequences
            .lock()
            .expect("replay sequence lock")
            .push(sequence);

        let mut requests = self.requests.lock().expect("request lock");
        let request = *requests;
        *requests += 1;
        drop(requests);

        Box::pin(async move {
            match request {
                0 => Ok(Box::pin(
                    stream::iter(vec![Ok(CompletionEvent::Delta("partial".to_owned()))])
                        .chain(stream::pending()),
                ) as BoxCompletionStream),
                1 => Ok(Box::pin(stream::iter(vec![Ok(CompletionEvent::Finished)]))
                    as BoxCompletionStream),
                request => panic!("unexpected provider request {request}"),
            }
        })
    }
}

impl DurableResumeProvider {
    fn saw_resumed_tool_result(&self) -> bool {
        *self
            .saw_resumed_tool_result
            .lock()
            .expect("resume flag lock")
    }
}

impl LlmStream for DurableResumeProvider {
    fn name(&self) -> &'static str {
        "durable_resume_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let mut requests = self.requests.lock().expect("request lock");
        let events = match *requests {
            0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "question-call-1",
                QUESTION_TOOL_NAME,
                json!({
                    "questions": [{
                        "question": "Choose a path",
                        "header": "Path",
                        "options": [{
                            "label": "Resume",
                            "description": "Continue the durable session"
                        }],
                        "multiple": false
                    }]
                }),
            )))],
            1 => {
                assert!(
                    request
                        .tool_results()
                        .any(|result| result.name == QUESTION_TOOL_NAME
                            && result.content == "resume-answer-123"),
                    "resumed provider request did not include question answer: {request:?}"
                );
                *self
                    .saw_resumed_tool_result
                    .lock()
                    .expect("resume flag lock") = true;
                vec![
                    Ok(CompletionEvent::Delta("resumed-with-answer".to_owned())),
                    Ok(CompletionEvent::Finished),
                ]
            }
            turn => panic!("unexpected provider request turn {turn}"),
        };
        *requests += 1;
        drop(requests);
        Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
    }
}

fn expected_turn_metadata() -> ProviderTurnMetadata {
    ProviderTurnMetadata {
        provider: "metadata_provider".to_owned(),
        url: "memory://metadata-provider".to_owned(),
        model: "metadata-model".to_owned(),
        usage: Some(TokenUsage {
            input_tokens: Some(11),
            output_tokens: Some(3),
            ..Default::default()
        }),
        cost: None,
        resume: json!({ "response_id": "resp-1" }),
    }
}

#[derive(Default)]
struct MetadataEmittingProvider {
    requests: Mutex<usize>,
}

impl LlmStream for MetadataEmittingProvider {
    fn name(&self) -> &'static str {
        "metadata_provider"
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<serde_json::Value> {
        Some(json!({ "wire": "projected", "prompt": request.prompt }))
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let mut requests = self.requests.lock().expect("request lock");
        let events = match *requests {
            0 => vec![
                Ok(CompletionEvent::Delta("first".to_owned())),
                Ok(CompletionEvent::ProviderMetadata(expected_turn_metadata())),
                Ok(CompletionEvent::Finished),
            ],
            1 => {
                assert_eq!(
                    request
                        .turns
                        .first()
                        .and_then(|turn| turn.metadata.as_ref()),
                    Some(&expected_turn_metadata()),
                    "same-family provider should see prior turn metadata"
                );
                vec![
                    Ok(CompletionEvent::Delta("second".to_owned())),
                    Ok(CompletionEvent::Finished),
                ]
            }
            turn => panic!("unexpected provider request {turn}"),
        };
        *requests += 1;
        drop(requests);
        Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
    }
}

struct ForeignMetadataProvider;

impl LlmStream for ForeignMetadataProvider {
    fn name(&self) -> &'static str {
        "other_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        assert_eq!(
            request
                .turns
                .first()
                .and_then(|turn| turn.metadata.as_ref()),
            Some(&expected_turn_metadata()),
            "generic agent loop should not filter provider metadata: {request:?}"
        );
        Box::pin(async move {
            Ok(Box::pin(stream::iter(vec![Ok(CompletionEvent::Finished)])) as BoxCompletionStream)
        })
    }
}

struct AgentsSystemPromptProvider {
    expected_path: PathBuf,
}

impl LlmStream for AgentsSystemPromptProvider {
    fn name(&self) -> &'static str {
        "agents_system_prompt_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let system_prompt = request.system_prompt.expect("system prompt");
            assert!(system_prompt.contains(&format!(
                "Instructions from: {}",
                self.expected_path.display()
            )));
            assert!(system_prompt.contains("Prefer concise answers."));
            Ok(Box::pin(stream::iter(vec![
                Ok(CompletionEvent::Delta("ok".to_owned())),
                Ok(CompletionEvent::Finished),
            ])) as BoxCompletionStream)
        })
    }
}

struct SkillsSystemPromptProvider {
    expected_root: PathBuf,
}

impl LlmStream for SkillsSystemPromptProvider {
    fn name(&self) -> &'static str {
        "skills_system_prompt_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let system_prompt = request.system_prompt.expect("system prompt");
            assert!(system_prompt.contains("Available skills:"));
            assert!(system_prompt.contains("example: Use for example tasks."));
            assert!(system_prompt.contains(&format!("Root: {}", self.expected_root.display())));
            assert!(request.tools.iter().any(|tool| tool.name == "load_skill"));
            Ok(Box::pin(stream::iter(vec![Ok(CompletionEvent::Finished)])) as BoxCompletionStream)
        })
    }
}

struct LoadSkillProvider {
    requests: Mutex<usize>,
    arguments: serde_json::Value,
}

impl LoadSkillProvider {
    const fn new(arguments: serde_json::Value) -> Self {
        Self {
            requests: Mutex::new(0),
            arguments,
        }
    }
}

impl LlmStream for LoadSkillProvider {
    fn name(&self) -> &'static str {
        "load_skill_provider"
    }

    fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let mut requests = self.requests.lock().expect("lock requests");
        let request_index = *requests;
        *requests += 1;
        drop(requests);
        let arguments = self.arguments.clone();

        Box::pin(async move {
            let events = if request_index < 2 {
                vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    format!("load_{request_index}"),
                    "load_skill",
                    arguments,
                )))]
            } else {
                vec![Ok(CompletionEvent::Finished)]
            };
            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}

struct DuplicateSkillProvider {
    root: PathBuf,
}

impl LlmStream for DuplicateSkillProvider {
    fn name(&self) -> &'static str {
        "duplicate_skill_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            if request.tool_results().next().is_none() {
                let prompt = request.system_prompt.expect("root skill catalog");
                assert!(prompt.contains("example: Use root example."), "{prompt}");
                assert!(
                    prompt.contains(&format!("Root: {}", self.root.display())),
                    "{prompt}"
                );
            }
            let events = match request.tool_results().count() {
                0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "read_once",
                    "read",
                    json!({ "filePath": "packages/app/src/main.rs" }),
                )))],
                1 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "load_once",
                    "load_skill",
                    json!({ "source": "local", "name": "example" }),
                )))],
                _ => vec![Ok(CompletionEvent::Finished)],
            };
            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}

struct RootedSkillProvider {
    root: PathBuf,
}

impl LlmStream for RootedSkillProvider {
    fn name(&self) -> &'static str {
        "rooted_skill_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let root = self.root.display().to_string();
        Box::pin(async move {
            let events = match request.tool_results().count() {
                0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "read_once",
                    "read",
                    json!({ "filePath": "packages/app/src/main.rs" }),
                )))],
                1 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "load_once",
                    "load_skill",
                    json!({ "source": "local", "name": "example", "root": root }),
                )))],
                _ => vec![Ok(CompletionEvent::Finished)],
            };
            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}

#[derive(Default)]
struct RepeatedGlobProvider {
    requests: Mutex<usize>,
}

impl LlmStream for RepeatedGlobProvider {
    fn name(&self) -> &'static str {
        "repeated_glob_provider"
    }

    fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let mut requests = self.requests.lock().expect("lock requests");
        let request_index = *requests;
        *requests += 1;
        drop(requests);

        Box::pin(async move {
            let events = if request_index < 2 {
                vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    format!("glob_{request_index}"),
                    "glob",
                    json!({ "pattern": "**/*.rs" }),
                )))]
            } else {
                vec![Ok(CompletionEvent::Finished)]
            };
            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}

#[derive(Default)]
struct RepeatedReadProvider {
    requests: Mutex<usize>,
}

impl LlmStream for RepeatedReadProvider {
    fn name(&self) -> &'static str {
        "repeated_read_provider"
    }

    fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let mut requests = self.requests.lock().expect("lock requests");
        let request_index = *requests;
        *requests += 1;
        drop(requests);

        Box::pin(async move {
            let events = if request_index < 2 {
                vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    format!("read_{request_index}"),
                    "read",
                    json!({ "filePath": "src/main.rs" }),
                )))]
            } else {
                vec![
                    Ok(CompletionEvent::Delta("done".to_owned())),
                    Ok(CompletionEvent::Finished),
                ]
            };
            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}

struct SingleReadProvider;

impl LlmStream for SingleReadProvider {
    fn name(&self) -> &'static str {
        "single_read_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let is_continuation = request.tool_results().next().is_some();
        Box::pin(async move {
            let events = if is_continuation {
                vec![
                    Ok(CompletionEvent::Delta("done".to_owned())),
                    Ok(CompletionEvent::Finished),
                ]
            } else {
                vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "read_once",
                    "read",
                    json!({ "filePath": "src/main.rs" }),
                )))]
            };
            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}

struct SystemPromptProvider;

impl LlmStream for SystemPromptProvider {
    fn name(&self) -> &'static str {
        "system_prompt_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let system_prompt = request.system_prompt.expect("system prompt");
            assert!(system_prompt.contains(BASE_SYSTEM_PROMPT));
            assert!(system_prompt.contains("Current date: 2026-06-02."));
            assert!(system_prompt.contains("Filesystem tools are available"));
            assert!(system_prompt.contains("custom fragment"));
            assert!(system_prompt.ends_with("run fragment"));
            assert!(
                system_prompt.find(BASE_SYSTEM_PROMPT)
                    < system_prompt.find("Current date: 2026-06-02.")
            );
            assert!(
                system_prompt.find("Current date: 2026-06-02.")
                    < system_prompt.find("Filesystem tools are available")
            );
            assert!(
                system_prompt.find("Filesystem tools are available")
                    < system_prompt.find("custom fragment")
            );
            assert!(system_prompt.find("custom fragment") < system_prompt.find("run fragment"));
            Ok(Box::pin(stream::iter(vec![
                Ok(CompletionEvent::Delta("ok".to_owned())),
                Ok(CompletionEvent::Finished),
            ])) as BoxCompletionStream)
        })
    }
}

struct RecoveringProvider;

impl LlmStream for RecoveringProvider {
    fn name(&self) -> &'static str {
        "recovering_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let events = if request.tool_results().next().is_none() {
                vec![
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "call_1",
                        "fail_once",
                        json!({}),
                    ))),
                    Ok(CompletionEvent::Finished),
                ]
            } else {
                assert!(
                    request
                        .tool_results()
                        .next()
                        .expect("tool result")
                        .content
                        .contains("validation failed")
                );
                vec![
                    Ok(CompletionEvent::Delta("recovered".to_owned())),
                    Ok(CompletionEvent::Finished),
                ]
            };

            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}

struct ThinkingProvider;

impl LlmStream for ThinkingProvider {
    fn name(&self) -> &'static str {
        "thinking_provider"
    }

    fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            Ok(Box::pin(stream::iter(vec![
                Ok(CompletionEvent::ThinkingDelta(ThinkingDelta {
                    text: "hidden thought".to_owned(),
                })),
                Ok(CompletionEvent::ThinkingBlock(ThinkingBlock {
                    kind: ThinkingBlockKind::OpenAiResponses,
                    text: "hidden thought".to_owned(),
                    payload: json!({ "type": "reasoning", "id": "rs_1" }),
                })),
                Ok(CompletionEvent::Delta("final answer".to_owned())),
                Ok(CompletionEvent::Finished),
            ])) as BoxCompletionStream)
        })
    }
}

#[derive(Default)]
struct ReplayThinkingProvider {
    calls: Mutex<usize>,
    saw_replayed_thinking: Mutex<bool>,
}

impl ReplayThinkingProvider {
    fn saw_replayed_thinking(&self) -> bool {
        *self.saw_replayed_thinking.lock().expect("lock")
    }
}

impl LlmStream for ReplayThinkingProvider {
    fn name(&self) -> &'static str {
        "replay_thinking_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let mut calls = self.calls.lock().expect("lock");
            *calls += 1;
            if *calls == 1 {
                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::ThinkingBlock(ThinkingBlock {
                        kind: ThinkingBlockKind::OpenAiResponses,
                        text: "reasoning".to_owned(),
                        payload: json!({ "type": "reasoning", "id": "rs_1" }),
                    })),
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "call_1",
                        "lookup",
                        json!({}),
                    ))),
                ])) as BoxCompletionStream)
            } else {
                *self.saw_replayed_thinking.lock().expect("lock") =
                    request.thinking_blocks().count() == 1;
                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::Delta("done".to_owned())),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream)
            }
        })
    }
}

#[derive(Default)]
struct ReplayAssistantTextProvider {
    calls: Mutex<usize>,
    saw_replayed_assistant_text: Mutex<bool>,
}

impl ReplayAssistantTextProvider {
    fn saw_replayed_assistant_text(&self) -> bool {
        *self.saw_replayed_assistant_text.lock().expect("lock")
    }
}

impl LlmStream for ReplayAssistantTextProvider {
    fn name(&self) -> &'static str {
        "replay_assistant_text_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let mut calls = self.calls.lock().expect("lock");
            *calls += 1;
            if *calls == 1 {
                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::Delta("partial ".to_owned())),
                    Ok(CompletionEvent::Delta("answer".to_owned())),
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "call_1",
                        "lookup",
                        json!({}),
                    ))),
                ])) as BoxCompletionStream)
            } else {
                *self.saw_replayed_assistant_text.lock().expect("lock") =
                    request.assistant_texts().collect::<Vec<_>>() == ["partial answer"];
                Ok(Box::pin(stream::iter(vec![Ok(CompletionEvent::Finished)]))
                    as BoxCompletionStream)
            }
        })
    }
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(prefix: &str) -> Self {
        Self::new_under(std::env::temp_dir(), prefix)
    }

    fn new_under(root: impl AsRef<Path>, prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let path = root
            .as_ref()
            .join(format!("{prefix}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp root");
        Self {
            path: path.canonicalize().expect("canonical temp root"),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.path.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(path, content).expect("write temp file");
    }

    fn write_skill(&self, name: &str, description: &str, body: &str) {
        self.write(
            &format!(".agents/skills/{name}/SKILL.md"),
            &format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
        );
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.path));
    }
}
