//! End-to-end live coding-agent durable runtime test.

use std::{env, fmt::Write, fs, path::PathBuf, sync::Arc};

use futures_util::StreamExt;
use waltr_agent::{
    AgentRuntime, CHANNEL_ASSISTANT_TEXT, CHANNEL_EXTERNAL_REQUEST, CHANNEL_TOOL_RESULT,
    ExternalRequest, ExternalRequestKind, ExternalResponse, LlmError, LlmStream,
    MemorySessionStore, ModelRef, PermissionEffect, PermissionPolicy, ResumeInput,
    SessionCoordinator, SessionId, SessionRecord, SessionStore, TimelineContent, TimelineEventKind,
    TimelineStatus,
    tools::{
        filesystem::{FilesystemConfig, FilesystemToolProfile},
        filesystem_skills::SkillsConfig,
        question::{QUESTION_TOOL_NAME, QuestionConfig},
    },
};
use serde_json::json;

use crate::{
    live_provider, scripted_provider::CapturingProvider, session::TEST_HARNESS_SYSTEM_PROMPT,
};

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_coding_agent_durable_resume_context_skills_permissions_and_questions()
-> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::selected_from_env(
        "WALTR_AGENT_LIVE_PROVIDER",
        "openai_responses_websocket",
    ))?
    else {
        return Ok(());
    };
    run_live_coding_agent(live.provider, live.model).await
}

#[allow(clippy::too_many_lines)]
async fn run_live_coding_agent(
    provider: impl LlmStream + 'static,
    model: ModelRef,
) -> waltr_agent::Result<()> {
    let fixture = CodingAgentFixture::new()?;
    let provider = Arc::new(CapturingProvider::new(provider));
    let store = MemorySessionStore::new();
    let policy =
        PermissionPolicy::filesystem_external(fixture.root.path(), PermissionEffect::ask());
    let agent = AgentRuntime::builder(model, Arc::clone(&provider))
        .with_system_prompt("You are a coding agent validating durable middleware execution.")
        .with_permission_policy(policy)
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch)
                .with_agents_read_context()
                .with_skills_context(),
        )
        .with_skills(SkillsConfig::new(fixture.root.path()))
        .with_question_tool(QuestionConfig::new())
        .build();
    let sessions = SessionCoordinator::new(store.clone());
    let session = sessions.create().await?;
    let session_id = session.id().clone();
    let prompt = format!(
        "First use the question tool exactly once to ask whether to proceed with the durable coding-agent validation. Do not read files before the answer. After the answer says proceed, use read on project/src/task.txt. Follow every loaded AGENTS.md instruction from that read. When the context reveals the coding-style skill, use load_skill for coding-style before following its instructions. Read the skill reference and project reference files requested by that context. Then attempt to read this external file exactly once so permission middleware suspends: {}. Do not invent tokens; report only exact tokens you observed before any permission suspension.",
        fixture.outside.path().join("external.txt").display()
    );

    let first = DurableLiveSession::run_prompt(&sessions, &agent, &session_id, &prompt).await?;
    let first_context = test_context(&provider, &first);
    assert_tool_was_surfaced(&provider, QUESTION_TOOL_NAME);
    assert_tool_was_surfaced(&provider, "read");
    assert_tool_was_surfaced(&provider, "load_skill");
    assert!(!first.finished, "{first_context}");
    let question_request = first
        .external_requests
        .iter()
        .find(|request| request.kind == ExternalRequestKind::Question)
        .unwrap_or_else(|| panic!("expected question request\n{first_context}"));

    let stored_after_question = load_session(&store, &session_id).await?;
    let snapshot_after_question = stored_after_question.snapshot();
    assert!(
        snapshot_after_question
            .external_requests
            .iter()
            .any(|request| request.kind == waltr_agent::ExternalRequestKind::Question),
        "stored session did not contain question request: {stored_after_question:?}"
    );

    let resumed = DurableLiveSession::resume(
        &sessions,
        &agent,
        &session_id,
        ResumeInput::ExternalResponse {
            request_id: question_request.id.clone(),
            response: ExternalResponse::new(json!(
                "Proceed. Read the task, obey AGENTS.md, load coding-style, read required references, then attempt the external read."
            )),
        },
    )
    .await?;
    let resumed_context = test_context(&provider, &resumed);
    let report_session = crate::session::AgentTestSession::from_durable(&resumed);
    crate::session::report_tool_errors(&provider, &report_session, "coding_agent live test");

    assert!(
        resumed
            .external_requests
            .iter()
            .any(|request| request.kind == ExternalRequestKind::Permission),
        "{resumed_context}"
    );

    let stored_after_resume = load_session(&store, &session_id).await?;
    let snapshot_after_resume = stored_after_resume.snapshot();
    assert!(
        snapshot_after_resume
            .tool_results()
            .any(|result| result.name == QUESTION_TOOL_NAME),
        "question response was not recorded as a durable tool result: {stored_after_resume:?}"
    );
    assert!(
        snapshot_after_resume
            .external_requests
            .iter()
            .any(|request| request.kind == waltr_agent::ExternalRequestKind::Permission),
        "stored session did not contain permission request: {stored_after_resume:?}"
    );

    let observed = format!(
        "{}\n{}",
        resumed.text,
        snapshot_after_resume
            .tool_results()
            .map(|result| result.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    for token in [
        "root-agent-101",
        "project-agent-202",
        "task-token-404",
        "style-skill-505",
        "style-reference-606",
        "project-reference-303",
    ] {
        assert!(
            observed.contains(token),
            "missing {token}\n{resumed_context}"
        );
    }

    Ok(())
}

#[derive(Debug, Default)]
struct DurableLiveSession {
    text: String,
    finished: bool,
    external_requests: Vec<ExternalRequest>,
    tool_errors: Vec<waltr_agent::ToolResult>,
}

impl DurableLiveSession {
    async fn run_prompt<S>(
        sessions: &waltr_agent::SessionCoordinator<S>,
        agent: &AgentRuntime,
        session_id: &SessionId,
        prompt: &str,
    ) -> waltr_agent::Result<Self>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let stream = sessions
            .prompt(agent, session_id.clone(), prompt)
            .with_system_prompt_fragment(TEST_HARNESS_SYSTEM_PROMPT)
            .stream();
        Self::collect(stream).await
    }

    async fn resume<S>(
        sessions: &waltr_agent::SessionCoordinator<S>,
        agent: &AgentRuntime,
        session_id: &SessionId,
        input: ResumeInput,
    ) -> waltr_agent::Result<Self>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let stream = sessions
            .resume(agent, session_id.clone(), input)
            .with_system_prompt_fragment(TEST_HARNESS_SYSTEM_PROMPT)
            .stream();
        Self::collect(stream).await
    }

    async fn collect(
        mut stream: waltr_agent::BoxAgentTimelineStream<'_>,
    ) -> waltr_agent::Result<Self> {
        let mut session = Self::default();
        while let Some(event) = stream.next().await {
            let event = event?;
            match &event.content {
                TimelineContent::TextDelta { delta }
                    if crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT) =>
                {
                    session.text.push_str(delta);
                }
                TimelineContent::ToolResult { result }
                    if crate::session::channel_is(&event, CHANNEL_TOOL_RESULT) =>
                {
                    if result.content.starts_with("Tool error:") {
                        session.tool_errors.push(result.clone());
                    }
                }
                TimelineContent::ExternalRequest { request }
                    if crate::session::channel_is(&event, CHANNEL_EXTERNAL_REQUEST) =>
                {
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
                | TimelineContent::ToolCall { .. }
                | TimelineContent::ToolResult { .. }
                | TimelineContent::ExternalRequest { .. }
                | TimelineContent::ContentRef { .. }
                | TimelineContent::Json { .. } => {}
            }
        }
        Ok(session)
    }
}

impl crate::session::AgentTestSession {
    fn from_durable(session: &DurableLiveSession) -> Self {
        let mut converted = Self::default();
        converted.text.clone_from(&session.text);
        converted.finished = session.finished;
        converted
            .external_requests
            .clone_from(&session.external_requests);
        converted.tool_errors.clone_from(&session.tool_errors);
        converted
    }
}

fn assert_tool_was_surfaced(provider: &CapturingProvider<impl LlmStream>, name: &str) {
    assert!(
        provider.all_tool_names().iter().any(|tool| tool == name),
        "{}",
        provider.tool_context()
    );
}

async fn load_session<S>(store: &S, id: &SessionId) -> waltr_agent::Result<SessionRecord>
where
    S: SessionStore,
{
    store.load(id).await.map_err(|error| LlmError::Protocol {
        provider: "session-store",
        message: error.to_string(),
    })
}

fn test_context(
    provider: &CapturingProvider<impl LlmStream>,
    session: &DurableLiveSession,
) -> String {
    let mut report = format!(
        "{}: {}\n{}",
        provider.name(),
        session.text,
        provider.tool_context()
    );
    if !session.tool_errors.is_empty() {
        report.push_str("\ntool errors:");
        for error in &session.tool_errors {
            write!(
                report,
                "\n- {}({}): {}",
                error.name, error.call_id, error.content
            )
            .expect("write string report");
        }
    }
    report
}

struct CodingAgentFixture {
    _parent: TempRoot,
    root: TempRoot,
    outside: TempRoot,
}

impl CodingAgentFixture {
    fn new() -> waltr_agent::Result<Self> {
        let parent = TempRoot::new()?;
        let root = TempRoot::at(parent.path().join("root"))?;
        let outside = TempRoot::at(parent.path().join("outside"))?;
        root.write(
            "AGENTS.md",
            "Root instruction token: root-agent-101. Include root-agent-101 in observed output.\n",
        )?;
        root.write(
            "project/AGENTS.md",
            "Project instruction token: project-agent-202. Include project-agent-202 in observed output. Read docs/project-reference.md and include project-reference-303 in observed output.\n",
        )?;
        root.write(
            "project/src/task.txt",
            "Task token: task-token-404. Include task-token-404 in observed output.\n",
        )?;
        root.write(
            "project/docs/project-reference.md",
            "Project reference token: project-reference-303.\n",
        )?;
        root.write(
            "project/.agents/skills/coding-style/SKILL.md",
            r"---
name: coding-style
description: Apply the repository coding style.
---
Skill token: style-skill-505. Include style-skill-505 in observed output.
Read references/style.md and include style-reference-606 in observed output.
",
        )?;
        root.write(
            "project/.agents/skills/coding-style/references/style.md",
            "Style reference token: style-reference-606.\n",
        )?;
        outside.write("external.txt", "external-permission-707\n")?;
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
            "waltr-agent-coding-agent-live-test-{}-{}",
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
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.path));
    }
}
