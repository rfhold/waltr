//! Deterministic agent loop integration tests.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{StreamExt, stream};
use waltr_agent::{
    AGENTS_INSTRUCTION_FILE, Agent, AgentTimelineEvent, AgentTool, AgentsPromptConfig,
    BASE_SYSTEM_PROMPT, BoxCompletionStream, BoxStreamFuture, CHANNEL_ASSISTANT_TEXT,
    CHANNEL_EXTERNAL_REQUEST, CHANNEL_THINKING, CHANNEL_TOOL_CALL, CHANNEL_TOOL_PROGRESS_INFO,
    CHANNEL_TOOL_RESULT, CompletionEvent, CompletionRequest, ExternalRequestId,
    ExternalRequestKind, ExternalResponse, FilesystemPromptConfig, LlmError, LlmStream,
    MemorySessionStore, ModelApi, ModelRef, ProviderId, ProviderTurnMetadata, ResumeInput,
    SessionStore, SkillsConfig, ThinkingBlock, ThinkingBlockKind, ThinkingDelta, TimelineContent,
    TimelineEventKind, TimelineQuery, TimelineStatus, TokenUsage, ToolCall, ToolDefinition,
    tools::filesystem::FilesystemConfig, tools::question::QUESTION_TOOL_NAME,
    tools::question::QuestionConfig,
};
use serde_json::json;

use crate::scripted_provider::ScriptedProvider;

#[tokio::test]
async fn durable_session_resume_continues_same_agent_loop() {
    let provider = DurableResumeProvider::default();
    let agent = Agent::builder(test_model())
        .with_question_tool(QuestionConfig::new())
        .build();
    let store = MemorySessionStore::new();
    let sessions = agent.sessions(store.clone());
    let session_id = sessions
        .create()
        .await
        .expect("created session")
        .id()
        .clone();

    let mut stream = sessions
        .prompt(&provider, session_id.clone(), "ask then continue")
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
            &provider,
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
            text.push_str(&delta);
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
    let provider = MetadataEmittingProvider::default();
    let agent = Agent::builder(test_model()).build();
    let store = MemorySessionStore::new();
    let sessions = agent.sessions(store.clone());
    let session_id = create_session_id(&sessions).await;

    drain(
        sessions
            .prompt(&provider, session_id.clone(), "hello")
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
    let provider = MetadataEmittingProvider::default();
    let agent = Agent::builder(test_model()).build();
    let store = MemorySessionStore::new();
    let sessions = agent.sessions(store.clone());
    let session_id = create_session_id(&sessions).await;

    drain(
        sessions
            .prompt(&provider, session_id.clone(), "first prompt")
            .stream(),
    )
    .await;
    drain(
        sessions
            .prompt(&provider, session_id.clone(), "second prompt")
            .stream(),
    )
    .await;
    drain(
        sessions
            .prompt(&ForeignMetadataProvider, session_id, "third prompt")
            .stream(),
    )
    .await;
}

#[tokio::test]
async fn durable_session_timeline_ranges_preserve_cursor_order() {
    let provider = MetadataEmittingProvider::default();
    let agent = Agent::builder(test_model()).build();
    let store = MemorySessionStore::new();
    let sessions = agent.sessions(store.clone());
    let session_id = create_session_id(&sessions).await;

    drain(
        sessions
            .prompt(&provider, session_id.clone(), "range prompt")
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
        TimelineContent::TextDelta { delta } if delta == "first"
    ));
    assert!(crate::session::channel_is(&all[0], CHANNEL_ASSISTANT_TEXT));
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
    let agent = Agent::builder(test_model()).build();
    let store = MemorySessionStore::new();
    let sessions = agent.sessions(store.clone());
    let session_id = create_session_id(&sessions).await;

    let events = sessions
        .prompt(&provider, session_id.clone(), "stream text")
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

    assert_eq!(assistant_events.len(), 2);
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
    assert!(
        assistant_events
            .iter()
            .all(|event| event.kind == TimelineEventKind::ContentDelta)
    );
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
    let agent = Agent::builder(test_model()).with_tool(tool).build();

    let events = agent
        .prompt(&provider, "run progress tool")
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
async fn agents_system_prompt_is_sent_to_provider() {
    let temp = TempRoot::new("waltr-agent-static-agents-test");
    temp.write(AGENTS_INSTRUCTION_FILE, "Prefer concise answers.\n");
    let provider = AgentsSystemPromptProvider {
        expected_path: temp.path().join(AGENTS_INSTRUCTION_FILE),
    };
    let agent = Agent::builder(test_model())
        .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
        .build();

    let mut events = agent.prompt(&provider, "hello").stream();
    let mut text = String::new();
    while let Some(event) = events.next().await {
        let event = event.expect("agent event");
        if let TimelineContent::TextDelta { delta } = &event.content
            && crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT)
        {
            text.push_str(&delta);
        }
    }

    assert_eq!(text, "ok");
}

#[tokio::test]
async fn filesystem_read_agents_context_is_opt_in_and_loaded_once() {
    let temp = TempRoot::new("waltr-agent-read-agents-test");
    temp.write("AGENTS.md", "Root instructions.\n");
    temp.write("src/AGENTS.md", "Nested instructions.\n");
    temp.write("src/main.rs", "fn main() {}\n");
    let provider = RepeatedReadProvider::default();
    let agent = Agent::builder(test_model())
        .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_agents_read_context())
        .build();

    let mut events = agent.prompt(&provider, "read twice").stream();
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
async fn filesystem_read_agents_context_is_disabled_by_default() {
    let temp = TempRoot::new("waltr-agent-read-agents-disabled-test");
    temp.write("AGENTS.md", "Root instructions.\n");
    temp.write("src/AGENTS.md", "Nested instructions.\n");
    temp.write("src/main.rs", "fn main() {}\n");
    let agent = Agent::builder(test_model())
        .with_filesystem_tools(FilesystemConfig::new(temp.path()))
        .build();

    let tool_result =
        single_tool_result(agent.prompt(&SingleReadProvider, "read once").stream()).await;

    assert!(!tool_result.contains("<system-reminder>"));
    assert!(!tool_result.contains("Root instructions."));
    assert!(!tool_result.contains("Nested instructions."));
}

#[tokio::test]
async fn filesystem_read_agents_context_skips_static_root_instruction() {
    let temp = TempRoot::new("waltr-agent-read-static-agents-test");
    temp.write("AGENTS.md", "Root instructions.\n");
    temp.write("src/AGENTS.md", "Nested instructions.\n");
    temp.write("src/main.rs", "fn main() {}\n");
    let agent = Agent::builder(test_model())
        .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
        .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_agents_read_context())
        .build();

    let tool_result =
        single_tool_result(agent.prompt(&SingleReadProvider, "read once").stream()).await;

    assert!(tool_result.contains("Nested instructions."));
    assert!(!tool_result.contains("Root instructions."));
}

#[tokio::test]
async fn skills_system_prompt_lists_root_skills_and_exposes_loader() {
    let temp = TempRoot::new("waltr-agent-skills-system-test");
    temp.write_skill(
        "example",
        "Use for example tasks.",
        "# Example\nFollow example steps.\n",
    );
    let provider = SkillsSystemPromptProvider {
        expected_root: temp.path().canonicalize().expect("canonical temp root"),
    };
    let agent = Agent::builder(test_model())
        .with_skills(SkillsConfig::new(temp.path()))
        .build();

    drain(agent.prompt(&provider, "hello").stream()).await;
}

#[tokio::test]
async fn load_skill_loads_content_and_lists_resources_once() {
    let temp = TempRoot::new("waltr-agent-load-skill-test");
    temp.write_skill(
        "example",
        "Use for example tasks.",
        "# Example\nFollow example steps.\n",
    );
    temp.write(".agents/skills/example/references/guide.md", "details\n");
    let agent = Agent::builder(test_model())
        .with_skills(SkillsConfig::new(temp.path()))
        .build();

    let tool_results = tool_results(
        agent
            .prompt(
                &LoadSkillProvider::new(json!({ "name": "example" })),
                "load skill",
            )
            .stream(),
    )
    .await;

    assert_eq!(tool_results.len(), 2);
    assert!(tool_results[0].contains("<skill_content name=\"example\">"));
    assert!(tool_results[0].contains("Follow example steps."));
    assert!(tool_results[0].contains("- references/guide.md"));
    assert!(tool_results[1].contains("already loaded"));
}

#[tokio::test]
async fn duplicate_skill_names_require_root() {
    let temp = TempRoot::new("waltr-agent-duplicate-skills-test");
    temp.write_skill("example", "Use root example.", "# Root Example\n");
    temp.write(
        "packages/app/.agents/skills/example/SKILL.md",
        "---\nname: example\ndescription: Use app example.\n---\n# App Example\n",
    );
    temp.write("src/main.rs", "fn main() {}\n");
    temp.write("packages/app/src/main.rs", "fn main() {}\n");
    let agent = Agent::builder(test_model())
        .with_skills(SkillsConfig::new(temp.path()))
        .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
        .build();

    let tool_results = tool_results(
        agent
            .prompt(&DuplicateSkillProvider, "read and load")
            .stream(),
    )
    .await;

    assert_eq!(tool_results.len(), 2);
    assert!(tool_results[0].contains("New skills are available"));
    assert!(tool_results[0].contains("Use app example."));
    assert!(tool_results[1].contains("Tool error:"));
    assert!(tool_results[1].contains("ambiguous"));
}

#[tokio::test]
async fn duplicate_skill_names_load_with_root() {
    let temp = TempRoot::new("waltr-agent-rooted-skill-test");
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
    let agent = Agent::builder(test_model())
        .with_skills(SkillsConfig::new(temp.path()))
        .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
        .build();

    let tool_results = tool_results(
        agent
            .prompt(&RootedSkillProvider { root: app_root }, "read and load")
            .stream(),
    )
    .await;

    assert_eq!(tool_results.len(), 2);
    assert!(tool_results[1].contains("<skill_content name=\"example\">"));
    assert!(tool_results[1].contains("# App Example"));
    assert!(!tool_results[1].contains("# Root Example"));
}

#[tokio::test]
async fn glob_directory_walk_reveals_skills_once() {
    let temp = TempRoot::new("waltr-agent-glob-skills-test");
    temp.write(
        "packages/app/.agents/skills/app-skill/SKILL.md",
        "---\nname: app-skill\ndescription: Use app skill.\n---\n# App Skill\n",
    );
    temp.write("packages/app/src/main.rs", "fn main() {}\n");
    let agent = Agent::builder(test_model())
        .with_skills(SkillsConfig::new(temp.path()))
        .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
        .build();

    let tool_results = tool_results(
        agent
            .prompt(&RepeatedGlobProvider::default(), "glob twice")
            .stream(),
    )
    .await;

    assert_eq!(tool_results.len(), 2);
    assert!(tool_results[0].contains("New skills are available"));
    assert!(tool_results[0].contains("app-skill: Use app skill."));
    assert!(!tool_results[1].contains("New skills are available"));
}

#[tokio::test]
async fn dynamic_skills_discovery_is_opt_in() {
    let temp = TempRoot::new("waltr-agent-skills-disabled-test");
    temp.write(
        "packages/app/.agents/skills/example/SKILL.md",
        "---\nname: example\ndescription: Use app example.\n---\n# App Example\n",
    );
    temp.write("src/main.rs", "fn main() {}\n");
    temp.write("packages/app/src/main.rs", "fn main() {}\n");
    let agent = Agent::builder(test_model())
        .with_skills(SkillsConfig::new(temp.path()))
        .with_filesystem_tools(FilesystemConfig::new(temp.path()))
        .build();

    let tool_result =
        single_tool_result(agent.prompt(&SingleReadProvider, "read once").stream()).await;

    assert!(!tool_result.contains("New skills are available"));
    assert!(!tool_result.contains("Use app example."));
}

#[tokio::test]
async fn agent_system_prompt_fragments_are_sent_in_order() {
    let agent = Agent::builder(test_model())
        .with_base_system_prompt()
        .with_system_date("2026-06-02")
        .with_filesystem_system_prompt(FilesystemPromptConfig::new("/tmp/waltr-root"))
        .with_system_prompt("custom fragment")
        .build();

    let mut events = agent
        .prompt(&SystemPromptProvider, "hello")
        .with_system_prompt_fragment("run fragment")
        .stream();
    let mut text = String::new();
    while let Some(event) = events.next().await {
        let event = event.expect("agent event");
        if let TimelineContent::TextDelta { delta } = &event.content
            && crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT)
        {
            text.push_str(&delta);
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
    let agent = Agent::builder(test_model()).with_tool(tool).build();

    let mut events = agent.prompt(&RecoveringProvider, "recover").stream();
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
    let agent = Agent::builder(test_model()).build();
    let mut events = agent.prompt(&ThinkingProvider, "think").stream();
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
    let provider = ReplayThinkingProvider::default();
    let tool = AgentTool::new(
        ToolDefinition::new(
            "lookup",
            "Look up test data.",
            json!({ "type": "object", "additionalProperties": false }),
        ),
        |call| Ok(call.result("tool output")),
    );
    let agent = Agent::builder(test_model()).with_tool(tool).build();

    drain(agent.prompt(&provider, "use tool").stream()).await;

    assert!(provider.saw_replayed_thinking());
}

#[tokio::test]
async fn agent_replays_assistant_text_on_tool_continuation() {
    let provider = ReplayAssistantTextProvider::default();
    let tool = AgentTool::new(
        ToolDefinition::new(
            "lookup",
            "Look up test data.",
            json!({ "type": "object", "additionalProperties": false }),
        ),
        |call| Ok(call.result("tool output")),
    );
    let agent = Agent::builder(test_model()).with_tool(tool).build();

    drain(agent.prompt(&provider, "use tool").stream()).await;

    assert!(provider.saw_replayed_assistant_text());
}

fn test_model() -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses)
}

async fn create_session_id<S>(sessions: &waltr_agent::AgentSessions<S>) -> waltr_agent::SessionId
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    sessions
        .create()
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
            results.push(result.content.clone());
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
        }),
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

struct DuplicateSkillProvider;

impl LlmStream for DuplicateSkillProvider {
    fn name(&self) -> &'static str {
        "duplicate_skill_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
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
                    json!({ "name": "example" }),
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
                    json!({ "name": "example", "root": root }),
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
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
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
