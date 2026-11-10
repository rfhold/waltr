use super::{harness::HostSession, sandbox::TestSandbox, scripted_provider::ScriptedProvider};
use crate::agent_host::tools::filesystem::{FilesystemConfig, FilesystemToolProfile};
#[cfg(feature = "shell")]
use crate::agent_host::tools::shell::ShellConfig;
use crate::{
    agent_host::{
        AgentsPromptConfig, Capability, FilesystemPromptConfig, HostRuntimeBuilder, SkillsConfig,
    },
    interaction::InteractionResponse,
    permission::*,
};
use futures_util::StreamExt;
use serde_json::json;
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use waltr_agent::*;

fn model() -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses)
}
fn settings() -> InferenceSettings {
    InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".into(),
            api: ModelApi::OpenAiResponses,
            url: "https://unused.invalid".into(),
            model_id: "test-model".into(),
            model_cost: None,
            headers: Default::default(),
            oauth: None,
            reasoning_effort: None,
        },
    }
}
fn write(root: &std::path::Path, path: &str, value: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value).unwrap();
}
fn provider(calls: Vec<(&'static str, serde_json::Value)>) -> Arc<ScriptedProvider> {
    Arc::new(ScriptedProvider::new("host", move |request| {
        let index = request.tool_results().count();
        calls.get(index).map_or_else(
            || vec![Ok(CompletionEvent::Finished)],
            |(name, args)| {
                vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    format!("call-{index}"),
                    *name,
                    args.clone(),
                )))]
            },
        )
    }))
}
async fn events(host: &HostSession, prompt: impl Into<String>) -> Vec<AgentTimelineEvent> {
    host.prompt(prompt)
        .stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect()
}
fn results(events: &[AgentTimelineEvent]) -> Vec<&ToolResult> {
    events
        .iter()
        .filter_map(|event| match &event.content {
            TimelineContent::ToolResult { result } => Some(result),
            _ => None,
        })
        .collect()
}
fn approve(host: &HostSession, pending: &crate::interaction::InteractionSnapshot) {
    super::respond(
        &host.broker,
        pending,
        InteractionResponse::Permission(PermissionResponse::AllowOnce),
    )
    .unwrap();
}

#[tokio::test]
async fn filesystem_operations_and_structured_patch_use_the_worker() {
    for patch in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let worker = TestSandbox::new(root.path()).await;
        let calls = if patch {
            vec![
                (
                    "apply_patch",
                    json!({"patchText":"*** Begin Patch\n*** Add File: note.txt\n+alpha\n+beta\n*** End Patch"}),
                ),
                ("read", json!({"filePath":"note.txt"})),
            ]
        } else {
            vec![
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
            ]
        };
        let count = calls.len();
        let host = HostSession::new(
            HostRuntimeBuilder::new(model(), provider(calls))
                .with_sandbox(worker.handle())
                .with_filesystem_tools(FilesystemConfig::new(root.path()).with_profile(if patch {
                    FilesystemToolProfile::StructuredPatch
                } else {
                    FilesystemToolProfile::ExactEdit
                })),
        )
        .await;
        let output = events(&host, "exercise tools").await;
        let output = results(&output);
        assert_eq!(output.len(), count);
        assert!(output.iter().all(|r| !r.is_error), "{output:?}");
        assert!(output[1].content.contains("alpha"));
        if patch {
            assert_eq!(
                fs::read_to_string(root.path().join("note.txt")).unwrap(),
                "alpha\nbeta\n"
            );
        } else {
            assert!(output[2].content.contains("note.txt"));
            assert!(output[3].content.contains("alpha"));
            assert!(output[5].content.contains("gamma"));
            assert!(!root.path().join("nested/note.txt").exists());
        }
        host.close().await;
        worker.shutdown().await;
    }
}

#[tokio::test]
async fn templates_keep_arguments_errors_and_input_denials_before_effects() {
    for mode in ["ok", "error", "deny", "input"] {
        let effects = Arc::new(AtomicUsize::new(0));
        let count = effects.clone();
        let tool = Capability::new(
            ToolDefinition::new("effect", "effect", json!({"type":"object"})),
            move |ctx| {
                count.fetch_add(1, Ordering::SeqCst);
                if mode == "error" {
                    Ok(ctx.result("boom").with_error())
                } else {
                    Ok(ctx.result("output marker"))
                }
            },
        );
        let policy = match mode {
            "deny" => PermissionPolicy::default().with_tool_rule(
                PermissionToolSelector::exact("effect"),
                PermissionEffect::deny(),
            ),
            "input" => PermissionPolicy::default().with_input_rule(
                PermissionToolSelector::exact("effect"),
                PermissionInputRule::new(
                    vec![PermissionInputCondition {
                        pointer: PermissionJsonPointer::parse("/options/dryRun").unwrap(),
                        value: PermissionInputValueMatcher::Exact(PermissionInputScalar::Bool(
                            false,
                        )),
                    }],
                    PermissionEffect::deny(),
                )
                .unwrap(),
            ),
            _ => PermissionPolicy::default(),
        };
        let provider = Arc::new(ScriptedProvider::new("template", move |request| {
            assert!(request.prompt.contains("<arguments>"));
            assert!(request.prompt.contains("\"dryRun\":false"));
            assert!(request.prompt.contains(if mode == "ok" {
                "output marker"
            } else if mode == "error" {
                "boom"
            } else {
                "status=\"error\""
            }));
            vec![Ok(CompletionEvent::Finished)]
        }));
        let host = HostSession::new(
            HostRuntimeBuilder::new(model(), provider)
                .with_tool(tool)
                .with_permission_policy(policy),
        )
        .await;
        events(
            &host,
            "before @tool(effect, {\"options\":{\"dryRun\":false}}) after",
        )
        .await;
        assert_eq!(
            effects.load(Ordering::SeqCst),
            usize::from(mode == "ok" || mode == "error")
        );
        host.close().await;
    }
}

#[tokio::test]
async fn template_occurrences_resume_the_same_render_once_and_cancellation_does_not_grant() {
    for decision in ["allow", "deny", "cancel"] {
        let effects = Arc::new(AtomicUsize::new(0));
        let observed = effects.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let tool = Capability::new(
            ToolDefinition::new("repeat", "repeat", json!({"type":"object"})),
            move |ctx| {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(ctx.result("rendered"))
            },
        );
        let provider = Arc::new(ScriptedProvider::new("live-render", move |request| {
            seen.fetch_add(1, Ordering::SeqCst);
            assert!(!request.prompt.is_empty());
            if decision == "allow" {
                assert_eq!(request.prompt.matches("rendered").count(), 2);
            }
            vec![Ok(CompletionEvent::Finished)]
        }));
        let host = HostSession::new(
            HostRuntimeBuilder::new(model(), provider)
                .with_tool(tool)
                .with_permission_policy(PermissionPolicy::default().with_tool_rule(
                    PermissionToolSelector::exact("repeat"),
                    PermissionEffect::ask(),
                )),
        )
        .await;
        let owner = host.start("Please @tool(repeat, {}) and @tool(repeat, {}) today");
        let first = super::pending(&host.broker, &host.id).await;
        assert!(!owner.is_finished());
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        if decision == "allow" {
            approve(&host, &first);
            let second = super::pending(&host.broker, &host.id).await;
            assert_ne!(first.pending[0].id, second.pending[0].id);
            assert_eq!(effects.load(Ordering::SeqCst), 1);
            approve(&host, &second);
        } else if decision == "cancel" {
            host.cancellation.cancel();
        } else {
            super::respond(
                &host.broker,
                &first,
                InteractionResponse::Permission(PermissionResponse::Deny {
                    message: Some("no".into()),
                    mode: PermissionDenyMode::CancelWork,
                }),
            )
            .unwrap();
        }
        drop(
            tokio::time::timeout(Duration::from_secs(5), owner)
                .await
                .expect("render owner did not stop")
                .unwrap(),
        );
        assert_eq!(
            effects.load(Ordering::SeqCst),
            if decision == "allow" { 2 } else { 0 }
        );
        assert!(host.broker.snapshot(&host.id).pending.is_empty());
        if decision == "allow" {
            let record = host.sessions.load(&host.id).await.unwrap();
            assert_eq!(record.record.snapshot().prompts.len(), 1);
            assert_eq!(requests.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(requests.load(Ordering::SeqCst), 0);
            events(&host, "replacement").await;
            assert_eq!(effects.load(Ordering::SeqCst), 0);
        }
        host.close().await;
    }
}

#[tokio::test]
async fn worker_template_claims_and_nested_file_renders_keep_exact_ownership() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let worker = TestSandbox::new(root.path()).await;
    write(
        root.path(),
        "nested.md",
        "@tool(write, {\"filePath\":\"marker\",\"content\":\"first\"})",
    );
    let host = HostSession::new(
        HostRuntimeBuilder::new(model(), provider(vec![]))
            .with_sandbox(worker.handle())
            .with_filesystem_tools(FilesystemConfig::new(root.path()))
            .with_template_base_dir(root.path())
            .with_permission_policy(PermissionPolicy::filesystem_external(
                fs::canonicalize(root.path()).unwrap(),
                PermissionEffect::deny(),
            )),
    )
    .await;
    events(&host, "@nested.md").await;
    assert_eq!(
        fs::read_to_string(root.path().join("marker")).unwrap(),
        "first"
    );
    write(
        root.path(),
        "nested.md",
        "@tool(write, {\"filePath\":\"marker\",\"content\":\"second\"})",
    );
    events(&host, "@nested.md").await;
    assert_eq!(
        fs::read_to_string(root.path().join("marker")).unwrap(),
        "second"
    );
    let forbidden = outside.path().join("forbidden");
    events(
        &host,
        format!(
            "@tool(write, {{\"filePath\":{:?},\"content\":\"no\"}})",
            forbidden.display().to_string()
        ),
    )
    .await;
    assert!(!forbidden.exists());
    let record = host.sessions.load(&host.id).await.unwrap();
    assert!(
        record
            .record
            .journal
            .iter()
            .any(|e| matches!(e, SessionJournalEvent::MiddlewareStateUpdated { namespace, state } if namespace == "host-dispatch" && state.get("active") == Some(&json!(false))))
    );
    host.close().await;
    worker.shutdown().await;
}

#[tokio::test]
async fn completed_render_is_not_replayed_when_a_rendering_waiter_is_dropped() {
    let root = tempfile::tempdir().unwrap();
    let worker = TestSandbox::new(root.path()).await;
    let host = HostSession::new(
        HostRuntimeBuilder::new(model(), provider(vec![]))
            .with_sandbox(worker.handle())
            .with_filesystem_tools(FilesystemConfig::new(root.path())),
    )
    .await;
    let prompt = "@tool(write, {\"filePath\":\"once\",\"content\":\"first\"}) @tool(write, {\"filePath\":\"once\",\"content\":\"second\"})";
    let mut stream = host.prompt(prompt).stream();
    while let Some(event) = stream.next().await {
        if event.unwrap().status == Some(TimelineStatus::Finished) {
            break;
        }
    }
    drop(stream);
    write(root.path(), "once", "externally changed");
    drop(
        host.sessions
            .resume(&host.runtime, host.id.clone(), ResumeInput::Continue)
            .stream()
            .collect::<Vec<_>>()
            .await,
    );
    assert_eq!(
        fs::read_to_string(root.path().join("once")).unwrap(),
        "externally changed"
    );
    host.close().await;
    worker.shutdown().await;
}

#[tokio::test]
async fn instruction_templates_hold_the_same_live_guard_through_approval() {
    let root = tempfile::tempdir().unwrap();
    let worker = TestSandbox::new(root.path()).await;
    write(
        root.path(),
        "AGENTS.md",
        "Inspect @tool(read, {\"filePath\":\"src/main.rs\"})\n",
    );
    write(root.path(), "src/main.rs", "approved fixture");
    let provider = Arc::new(ScriptedProvider::new("instruction", |r| {
        let system = r.system_prompt.unwrap();
        assert_eq!(system.matches("Inspect ").count(), 1);
        assert!(system.contains("approved fixture"));
        vec![Ok(CompletionEvent::Finished)]
    }));
    let host = HostSession::new(
        HostRuntimeBuilder::new(model(), provider)
            .with_sandbox(worker.handle())
            .with_agents_system_prompt(AgentsPromptConfig::new(root.path()))
            .with_filesystem_tools(FilesystemConfig::new(root.path()).with_agents_read_context())
            .with_permission_policy(PermissionPolicy::default().with_tool_rule(
                PermissionToolSelector::exact("read"),
                PermissionEffect::ask(),
            )),
    )
    .await;
    let owner = host.start("hello");
    let pending = super::pending(&host.broker, &host.id).await;
    assert!(!owner.is_finished());
    approve(&host, &pending);
    owner.await.unwrap();
    let state = host
        .sessions
        .load(&host.id)
        .await
        .unwrap()
        .record
        .snapshot()
        .middleware_state;
    assert_eq!(state["agents"]["loaded"].as_array().map(Vec::len), Some(1));
    host.close().await;
    worker.shutdown().await;
}

#[tokio::test]
async fn file_references_use_formatted_bounded_reads_and_persist_the_rendered_prompt() {
    let root = tempfile::tempdir().unwrap();
    let worker = TestSandbox::new(root.path()).await;
    write(root.path(), "long.md", &"line\n".repeat(2002));
    write(root.path(), "AGENTS.md", "Follow @long.md\n");
    let provider = Arc::new(ScriptedProvider::new("file", |r| {
        assert!(r.prompt.contains("2000: line"));
        assert!(!r.prompt.contains("2001: line"));
        assert!(r.prompt.contains("Use offset=2001 to continue."));
        assert!(
            r.system_prompt
                .unwrap()
                .contains("<template-result kind=\"file\"")
        );
        vec![Ok(CompletionEvent::Finished)]
    }));
    let host = HostSession::new(
        HostRuntimeBuilder::new(model(), provider)
            .with_sandbox(worker.handle())
            .with_filesystem_tools(FilesystemConfig::new(root.path()))
            .with_agents_system_prompt(AgentsPromptConfig::new(root.path()))
            .with_template_base_dir(root.path()),
    )
    .await;
    events(&host, "@long.md").await;
    let record = host.sessions.load(&host.id).await.unwrap();
    assert!(
        record
            .record
            .snapshot()
            .current_prompt
            .unwrap()
            .contains("<type>file</type>")
    );
    host.close().await;
    worker.shutdown().await;
}

#[tokio::test]
async fn snapshots_and_read_instruction_injection_are_bounded_captured_and_opt_in() {
    for (read_context, static_root) in [(false, false), (true, false), (true, true)] {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "AGENTS.md", "Root instructions.");
        write(root.path(), "src/AGENTS.md", "Nested instructions.");
        write(root.path(), "src/main.rs", "fixture");
        #[cfg(unix)]
        let workspace = {
            let p = root.path().join("alias");
            std::os::unix::fs::symlink(root.path(), &p).unwrap();
            p
        };
        #[cfg(not(unix))]
        let workspace = root.path().to_path_buf();
        let worker = TestSandbox::new(&workspace).await;
        assert_eq!(
            worker.handle().canonical_root().unwrap(),
            root.path().canonicalize().unwrap()
        );
        let mut builder = HostRuntimeBuilder::new(
            model(),
            provider(vec![
                ("read", json!({"filePath":"<path>src/main.rs</path>"})),
                ("read", json!({"filePath":"src/main.rs"})),
            ]),
        )
        .with_sandbox(worker.handle())
        .with_filesystem_system_prompt(
            FilesystemPromptConfig::new(&workspace)
                .with_snapshot()
                .with_max_snapshot_entries(2),
        );
        let config = FilesystemConfig::new(&workspace);
        builder = builder.with_filesystem_tools(if read_context {
            config.with_agents_read_context()
        } else {
            config
        });
        if static_root {
            builder = builder.with_agents_system_prompt(AgentsPromptConfig::new(&workspace));
        }
        let host = HostSession::new(builder).await;
        let output = events(&host, "read twice").await;
        let output = results(&output);
        assert_eq!(output.len(), 2);
        assert_eq!(
            output[0].content.contains("Nested instructions."),
            read_context
        );
        assert_eq!(
            output[0].content.contains("Root instructions."),
            read_context && !static_root
        );
        assert!(!output[1].content.contains("<system-reminder>"));
        let before = host
            .sessions
            .load(&host.id)
            .await
            .unwrap()
            .record
            .snapshot()
            .middleware_state["filesystem_snapshot"]
            .clone();
        assert!(before.as_str().unwrap().ends_with("- <snapshot truncated>"));
        write(root.path(), "new.txt", "later");
        worker.disconnect();
        events(&host, "again").await;
        assert_eq!(
            host.sessions
                .load(&host.id)
                .await
                .unwrap()
                .record
                .snapshot()
                .middleware_state["filesystem_snapshot"],
            before
        );
        host.close().await;
        worker.shutdown().await;
    }
}

#[tokio::test]
async fn skills_enforce_whole_tool_consent_catalog_roots_discovery_and_loaded_state() {
    for dynamic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let worker = TestSandbox::new(root.path()).await;
        write(
            root.path(),
            ".agents/skills/example/SKILL.md",
            "---\nname: example\ndescription: Root example.\n---\n# Root Example\n",
        );
        write(
            root.path(),
            ".agents/skills/example/references/guide.md",
            "guide",
        );
        write(
            root.path(),
            "packages/app/.agents/skills/example/SKILL.md",
            "---\nname: example\ndescription: App example.\n---\n# App Example\n",
        );
        write(root.path(), "packages/app/src/main.rs", "fixture");
        write(root.path(), "packages/app/.ignore", "!.agents/\n");
        let calls = vec![
            ("glob", json!({"pattern":"packages/app/**"})),
            ("glob", json!({"pattern":"packages/app/**"})),
            ("load_skill", json!({"source":"local","name":"example"})),
            (
                "load_skill",
                json!({"source":"local","name":"example","root":fs::canonicalize(root.path().join("packages/app")).unwrap()}),
            ),
            (
                "load_skill",
                json!({"source":"local","name":"example","root":fs::canonicalize(root.path().join("packages/app")).unwrap()}),
            ),
        ];
        let config = FilesystemConfig::new(root.path());
        let host = HostSession::new(
            HostRuntimeBuilder::new(model(), provider(calls))
                .with_sandbox(worker.handle())
                .with_skills(SkillsConfig::new(root.path()))
                .with_filesystem_tools(if dynamic {
                    config.with_skills_context()
                } else {
                    config
                })
                .with_permission_policy(PermissionPolicy::default().with_tool_rule(
                    PermissionToolSelector::exact("load_skill"),
                    PermissionEffect::ask(),
                )),
        )
        .await;
        let owner = host.start("discover and load");
        let mut approvals = 0;
        let mut last = None;
        while !owner.is_finished() {
            tokio::select! {_ = tokio::time::sleep(Duration::from_millis(5))=>{},pending=super::pending(&host.broker,&host.id)=>{if last.as_ref()!=Some(&pending.pending[0].id){approve(&host,&pending);last=Some(pending.pending[0].id.clone());approvals+=1;}}}
        }
        let output = owner
            .await
            .unwrap()
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        let output = results(&output);
        assert_eq!(output.len(), 5);
        assert_eq!(
            output[0].content.contains("New skills are available"),
            dynamic
        );
        assert!(!output[1].content.contains("New skills are available"));
        if dynamic {
            assert!(output[2].is_error);
            assert!(
                output[3].content.contains("# App Example"),
                "{:?}",
                output[3]
            );
            assert!(output[4].content.contains("already loaded"));
        } else {
            assert!(output[2].content.contains("# Root Example"));
            assert!(output[2].content.contains("references/guide.md"));
            assert!(
                !output[2].content.contains(
                    &root
                        .path()
                        .join(".agents/skills/example/references/guide.md")
                        .display()
                        .to_string()
                )
            );
        }
        assert!(approvals >= 1);
        host.close().await;
        worker.shutdown().await;
    }
    let root = tempfile::tempdir().unwrap();
    let worker = TestSandbox::new(root.path()).await;
    write(
        root.path(),
        ".agents/skills/outer/SKILL.md",
        "---\nname: outer\ndescription: Outer.\n---\nOuter",
    );
    write(
        root.path(),
        "packages/app/.agents/skills/inner/SKILL.md",
        "---\nname: inner\ndescription: Inner.\n---\nInner",
    );
    let p = Arc::new(ScriptedProvider::new("root", |r| {
        let s = r.system_prompt.unwrap();
        assert!(s.contains("inner: Inner."));
        assert!(!s.contains("outer: Outer."));
        vec![Ok(CompletionEvent::Finished)]
    }));
    let host = HostSession::new(
        HostRuntimeBuilder::new(model(), p)
            .with_sandbox(worker.handle())
            .with_skills(SkillsConfig::new(root.path().join("packages/app"))),
    )
    .await;
    events(&host, "catalog").await;
    host.close().await;
    worker.shutdown().await;
}

#[tokio::test]
async fn template_search_keeps_files_tools_skills_limits_and_startup_failure_isolation() {
    use crate::agent_host::{
        AgentDefinitionRegistry,
        runtime_factory::{McpConnectionKind, McpResolvedConnection, ServerAgentRuntimeFactory},
    };
    use crate::{auth::FileCredentialStore, interaction::InteractionBroker};
    let root = tempfile::tempdir().unwrap();
    let worker = TestSandbox::new(root.path()).await;
    write(root.path(), "src/two words.md", "notes");
    write(root.path(), ".hidden.rs", "hidden");
    for n in 0..1030 {
        write(root.path(), &format!("early-{n:04}.txt"), "");
    }
    write(root.path(), "z-target-beyond-cap.rs", "");
    write(
        root.path(),
        ".agents/skills/example/SKILL.md",
        "---\nname: example\ndescription: Example.\n---\nGuide",
    );
    let mut registry = AgentDefinitionRegistry::new();
    registry.load_yaml("apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: test}\nspec:\n  features: [{filesystem: {root: '$(session.workspace.root)', profile: auto, defaultPermissions: true}}, {skills: {root: '$(session.workspace.root)', defaultPermissions: true}}, {mcp: {}}]\n").unwrap();
    let factory = ServerAgentRuntimeFactory::new(
        worker.handle(),
        root.path().to_path_buf(),
        FileCredentialStore::new(root.path().join("auth.json")),
        registry.resolve_agent("test").unwrap(),
    )
    .with_capability_policy(ServerPermissionPolicy::sessions(InteractionBroker::new()))
    .with_runtime_provider(|_| Ok(AgentRuntimeProvider::new(model(), provider(vec![]))))
    .with_mcp_connection_resolver(|_| {
        Box::pin(async {
            Ok(vec![McpResolvedConnection {
                key: "missing".into(),
                kind: McpConnectionKind::Stdio,
                command: "definitely-not-installed-waltr-mcp".into(),
                url: String::new(),
            }])
        })
    });
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let sessions = sessions
        .clone()
        .with_runtime_factory(factory.session_factory(sessions));
    let mut create = SessionCreateRequest::new(SessionId::generate());
    create.inference_settings = Some(settings());
    let id = sessions.create(create).await.unwrap().id().clone();
    for (query, kinds, target, limit) in [
        (
            "words",
            vec![SessionTemplateKind::File],
            SessionTemplateSearchTarget::Resources,
            10,
        ),
        (
            "target-beyond-cap",
            vec![SessionTemplateKind::File],
            SessionTemplateSearchTarget::Resources,
            2,
        ),
        (
            "read",
            vec![SessionTemplateKind::Tool],
            SessionTemplateSearchTarget::Resources,
            10,
        ),
        (
            "skill:example",
            vec![SessionTemplateKind::Prompt],
            SessionTemplateSearchTarget::Prompts,
            10,
        ),
        (
            "early",
            vec![SessionTemplateKind::File],
            SessionTemplateSearchTarget::Resources,
            usize::MAX,
        ),
        ("", vec![], SessionTemplateSearchTarget::Resources, 10),
    ] {
        let options = sessions
            .search_templates(
                &id,
                SessionTemplateSearchRequest {
                    query: query.into(),
                    kinds,
                    target,
                    limit,
                    cursor: None,
                },
            )
            .await
            .unwrap();
        match query {
            "words" => assert!(
                options
                    .iter()
                    .any(|o| o.template == "@\"src/two words.md\"")
            ),
            "target-beyond-cap" => assert_eq!(options[0].detail, "z-target-beyond-cap.rs"),
            "read" => assert!(
                options
                    .iter()
                    .any(|o| o.template == "@tool(read, {\"filePath\":\"\"})"),
                "{options:?}"
            ),
            "skill:example" => assert_eq!(options[0].template, "/skill:example"),
            "early" => assert_eq!(options.len(), 100),
            _ => assert!(options.is_empty()),
        };
        assert!(!options.iter().any(|o| o.detail == ".hidden.rs"));
    }
    sessions.shutdown().await;
    worker.shutdown().await;
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn shell_root_workdir_exit_timeout_cancellation_and_template_stages() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let worker = TestSandbox::new(root.path()).await;
    let host = HostSession::new(
        HostRuntimeBuilder::new(
            model(),
            provider(vec![
                ("shell", json!({"command":"pwd","description":"root"})),
                (
                    "shell",
                    json!({"command":"printf error; exit 7","description":"exit"}),
                ),
                (
                    "shell",
                    json!({"command":"sleep 3","description":"timeout","timeout":1}),
                ),
            ]),
        )
        .with_sandbox(worker.handle())
        .with_shell_tool(ShellConfig::new(outside.path())),
    )
    .await;
    let output = events(&host, "shell").await;
    let output = results(&output);
    assert!(
        output[0]
            .content
            .contains(&root.path().canonicalize().unwrap().display().to_string())
    );
    assert!(output[1].content.contains("7"));
    assert!(
        output[2]
            .content
            .to_string()
            .to_lowercase()
            .contains("timed out")
    );
    host.close().await;
    let host = HostSession::new(
        HostRuntimeBuilder::new(model(), provider(vec![]))
            .with_sandbox(worker.handle())
            .with_shell_tool(ShellConfig::new(root.path()).with_default_workdir(outside.path()))
            .with_permission_policy(
                PermissionPolicy::filesystem_external(root.path(), PermissionEffect::ask())
                    .with_tool_rule(
                        PermissionToolSelector::exact("shell"),
                        PermissionEffect::ask(),
                    ),
            ),
    )
    .await;
    let owner = host.start(
        "@tool(shell, {\"command\":\"printf approved > marker\",\"description\":\"marker\"})",
    );
    let first = super::pending(&host.broker, &host.id).await;
    assert!(!outside.path().join("marker").exists());
    approve(&host, &first);
    let second = super::pending(&host.broker, &host.id).await;
    assert_ne!(first.pending[0].id, second.pending[0].id);
    assert!(!outside.path().join("marker").exists());
    approve(&host, &second);
    owner.await.unwrap();
    assert_eq!(
        fs::read_to_string(outside.path().join("marker")).unwrap(),
        "approved"
    );
    host.close().await;
    let host = HostSession::new(
        HostRuntimeBuilder::new(
            model(),
            provider(vec![(
                "shell",
                json!({"command":"sleep 30; touch should-not-exist","description":"cancel"}),
            )]),
        )
        .with_sandbox(worker.handle())
        .with_shell_tool(ShellConfig::new(root.path())),
    )
    .await;
    let owner = host.start("cancel");
    tokio::time::sleep(Duration::from_millis(100)).await;
    host.cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(10), owner)
        .await
        .unwrap()
        .unwrap();
    assert!(!root.path().join("should-not-exist").exists());
    host.close().await;
    worker.shutdown().await;
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn unknown_worker_dispatch_never_retries_and_legacy_history_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let worker = TestSandbox::new(root.path()).await;
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    let p = Arc::new(ScriptedProvider::new("unknown", move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
            "unknown",
            "shell",
            json!({"command":"sleep 30","description":"wait"}),
        )))]
    }));
    let host = HostSession::new(
        HostRuntimeBuilder::new(model(), p)
            .with_sandbox(worker.handle())
            .with_shell_tool(ShellConfig::new(root.path())),
    )
    .await;
    let owner = host.start("run");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if host
                .sessions
                .load(&host.id)
                .await
                .unwrap()
                .record
                .journal
                .iter()
                .any(|e| matches!(e, SessionJournalEvent::MiddlewareStateUpdated { namespace, state } if namespace == "host-dispatch" && state["active"] == true))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    worker.disconnect();
    drop(
        tokio::time::timeout(Duration::from_secs(10), owner)
            .await
            .unwrap()
            .unwrap(),
    );
    let resumed = host
        .sessions
        .resume(&host.runtime, host.id.clone(), ResumeInput::Continue)
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(resumed.is_empty() || resumed.iter().any(Result::is_err));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(
        serde_json::from_value::<TimelineContent>(json!({"ExternalRequest":{"request":{}}}))
            .is_err()
    );
    host.close().await;
    worker.shutdown().await;
}

#[tokio::test]
async fn an_earlier_template_result_is_not_replayed_after_a_later_ask() {
    let effects = Arc::new(AtomicUsize::new(0));
    let count = effects.clone();
    let p = Arc::new(ScriptedProvider::new("batch", |request| {
        assert!(request.prompt.contains("completed-first"));
        assert!(request.prompt.contains("approved-second"));
        vec![Ok(CompletionEvent::Finished)]
    }));
    let host = HostSession::new(
        HostRuntimeBuilder::new(model(), p)
            .with_tool(Capability::new(
                ToolDefinition::new("first", "first", json!({"type":"object"})),
                move |ctx| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(ctx.result("completed-first"))
                },
            ))
            .with_tool(Capability::new(
                ToolDefinition::new("second", "second", json!({"type":"object"})),
                |ctx| Ok(ctx.result("approved-second")),
            ))
            .with_permission_policy(PermissionPolicy::default().with_tool_rule(
                PermissionToolSelector::exact("second"),
                PermissionEffect::ask(),
            )),
    )
    .await;
    let owner = host.start("@tool(first, {}) then @tool(second, {})");
    let pending = super::pending(&host.broker, &host.id).await;
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert!(!owner.is_finished());
    approve(&host, &pending);
    assert!(owner.await.unwrap().iter().all(Result::is_ok));
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    host.close().await;
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn template_shell_timeout_is_an_inline_result_before_inference() {
    let root = tempfile::tempdir().unwrap();
    let worker = TestSandbox::new(root.path()).await;
    let p = Arc::new(ScriptedProvider::new("template-timeout", |request| {
        assert!(request.prompt.to_lowercase().contains("timed out"));
        vec![Ok(CompletionEvent::Finished)]
    }));
    let host = HostSession::new(
        HostRuntimeBuilder::new(model(), p)
            .with_sandbox(worker.handle())
            .with_shell_tool(ShellConfig::new(root.path())),
    )
    .await;
    events(
        &host,
        "@tool(shell, {\"command\":\"sleep 3\",\"description\":\"timeout\",\"timeout\":1})",
    )
    .await;
    host.close().await;
    worker.shutdown().await;
}

#[tokio::test]
async fn prompt_fragments_keep_declared_order() {
    let root = tempfile::tempdir().unwrap();
    let worker = TestSandbox::new(root.path()).await;
    write(root.path(), "AGENTS.md", "instruction-fragment");
    let p = Arc::new(ScriptedProvider::new("fragments", |request| {
        let system = request.system_prompt.unwrap();
        assert!(
            system.find("first-fragment").unwrap() < system.find("instruction-fragment").unwrap()
        );
        assert!(
            system.find("last-fragment").unwrap() < system.find("instruction-fragment").unwrap()
        );
        vec![Ok(CompletionEvent::Finished)]
    }));
    let host = HostSession::new(
        HostRuntimeBuilder::new(model(), p)
            .with_sandbox(worker.handle())
            .with_system_prompt("first-fragment")
            .with_agents_system_prompt(AgentsPromptConfig::new(root.path()))
            .with_system_prompt("last-fragment"),
    )
    .await;
    events(&host, "hello").await;
    host.close().await;
    worker.shutdown().await;
}
