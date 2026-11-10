use crate::host_tests::harness::HostSession;
use futures_util::StreamExt;
use std::fmt::Write;
use waltr_agent::*;

pub const TEST_HARNESS_SYSTEM_PROMPT: &str = "You are running inside an automated test harness. Follow the user instructions exactly. Use tools when the user explicitly asks you to use them. When asked to report exact values, include those values verbatim. Keep final answers concise and do not add extra commentary unless requested.";

#[derive(Debug, Default)]
pub struct AgentTestSession {
    pub text: String,
    pub timeline: Vec<AgentTimelineEvent>,
    pub finished: bool,
    pub saw_text_after_tool_result: bool,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    pub tool_errors: Vec<ToolResult>,
}

pub fn channel_is(event: &AgentTimelineEvent, channel: &str) -> bool {
    event
        .channel
        .as_ref()
        .is_some_and(|value| value.as_str() == channel)
}

pub fn session_context(provider: &impl LlmStream, session: &AgentTestSession) -> String {
    format!(
        "{}: {}\n{}",
        provider.name(),
        session.text,
        session.tool_error_report()
    )
}

pub fn report_tool_errors(provider: &impl LlmStream, session: &AgentTestSession, label: &str) {
    session.report_tool_errors_if_requested(provider.name(), label);
}

impl AgentTestSession {
    pub async fn run(
        agent: &HostSession,
        _provider: &impl LlmStream,
        prompt: &str,
    ) -> waltr_agent::Result<Self> {
        let events = agent
            .prompt(prompt)
            .with_system_prompt_fragment(TEST_HARNESS_SYSTEM_PROMPT)
            .stream()
            .collect::<Vec<_>>()
            .await;
        Self::from_events(events)
    }

    pub fn from_events(
        events: Vec<waltr_agent::Result<AgentTimelineEvent>>,
    ) -> waltr_agent::Result<Self> {
        let mut session = Self::default();
        for event in events {
            let event = event?;
            match &event.content {
                TimelineContent::TextDelta { delta }
                    if channel_is(&event, CHANNEL_ASSISTANT_TEXT) =>
                {
                    session.saw_text_after_tool_result |=
                        !session.tool_results.is_empty() && !delta.is_empty();
                    session.text.push_str(delta);
                }
                TimelineContent::ToolCall { call } => session.tool_calls.push(call.clone()),
                TimelineContent::ToolResult { result } => {
                    if result.is_error {
                        session.tool_errors.push(result.clone());
                    }
                    session.tool_results.push(result.clone());
                }
                TimelineContent::Empty if event.status == Some(TimelineStatus::Finished) => {
                    session.finished = true
                }
                _ => {}
            }
            session.timeline.push(event);
        }
        Ok(session)
    }

    pub fn require_tool_execution(&self, provider: &'static str) -> waltr_agent::Result<()> {
        if self.tool_calls.is_empty() || self.tool_results.is_empty() {
            Err(LlmError::Protocol {
                provider,
                message: "agent prompt did not execute a tool".into(),
            })
        } else {
            Ok(())
        }
    }

    pub fn tool_error_report(&self) -> String {
        let mut report = String::from("tool errors:");
        if self.tool_errors.is_empty() {
            report.push_str(" none");
        }
        for error in &self.tool_errors {
            write!(
                report,
                "\n- {}({}): {}",
                error.name, error.call_id, error.content
            )
            .unwrap();
        }
        report
    }

    pub fn report_tool_errors_if_requested(&self, provider: &str, label: &str) {
        if std::env::var("WALTR_AGENT_REPORT_TOOL_ERRORS")
            .is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
            && !self.tool_errors.is_empty()
        {
            eprintln!("{provider} {label}\n{}", self.tool_error_report());
        }
    }
}

#[cfg(test)]
mod state {
    use super::*;
    use crate::host_tests::{
        sandbox::TestSandbox,
        scripted_provider::{CapturingProvider, ScriptedProvider},
    };
    use crate::{
        agent_host::tools::filesystem::FilesystemConfig,
        agent_host::{AgentsPromptConfig, HostRuntimeBuilder, SkillsConfig},
        permission::ServerPermissionPolicy,
    };
    use serde_json::json;
    use std::sync::Arc;

    fn model() -> ModelRef {
        ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses)
    }
    fn provider(turns: Vec<Vec<CompletionEvent>>) -> Arc<CapturingProvider<ScriptedProvider>> {
        Arc::new(CapturingProvider::new(ScriptedProvider::from_turns(
            "state", turns,
        )))
    }
    fn done() -> Vec<CompletionEvent> {
        vec![
            CompletionEvent::Delta("done".into()),
            CompletionEvent::Finished,
        ]
    }

    #[tokio::test]
    async fn root_hint_is_request_scoped_and_absent_from_stored_definitions() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let a = TestSandbox::new(first.path()).await;
        let b = TestSandbox::new(second.path()).await;
        let p = provider(vec![done(), done()]);
        for tool in FilesystemConfig::new(first.path()).into_agent_tools(&model(), a.handle()) {
            assert!(!tool.definition().description.contains("Filesystem root:"));
        }
        let host = HostSession::new(
            HostRuntimeBuilder::new(model(), p.clone())
                .with_sandbox(a.handle())
                .with_filesystem_tools(FilesystemConfig::new(first.path())),
        )
        .await;
        AgentTestSession::run(&host, p.as_ref(), "first")
            .await
            .unwrap();
        let rebuilt = HostRuntimeBuilder::new(model(), p.clone())
            .with_sandbox(b.handle())
            .with_filesystem_tools(FilesystemConfig::new(second.path()))
            .with_dispatch_context(
                Some(host.id.clone()),
                ServerPermissionPolicy::sessions(host.broker.clone()),
            )
            .build();
        host.sessions
            .prompt(&rebuilt, host.id.clone(), "second")
            .stream()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<waltr_agent::Result<Vec<_>>>()
            .unwrap();
        let first_hint = format!(
            "Filesystem root: {}.",
            first.path().canonicalize().unwrap().display()
        );
        let second_hint = format!(
            "Filesystem root: {}.",
            second.path().canonicalize().unwrap().display()
        );
        assert!(p.tool_description(0, "read").contains(&first_hint));
        assert!(p.tool_description(1, "read").contains(&second_hint));
        assert!(!p.tool_description(1, "read").contains(&first_hint));
        host.close().await;
    }

    #[tokio::test]
    async fn captured_prompt_and_explicit_run_override_are_journaled_before_requests() {
        let p = provider(vec![done(), done(), done()]);
        let host = HostSession::new(
            HostRuntimeBuilder::new(model(), p.clone()).with_system_prompt("marker-first"),
        )
        .await;
        AgentTestSession::run(&host, p.as_ref(), "first")
            .await
            .unwrap();
        let rebuilt = HostRuntimeBuilder::new(model(), p.clone())
            .with_system_prompt("marker-second")
            .with_dispatch_context(
                Some(host.id.clone()),
                ServerPermissionPolicy::sessions(host.broker.clone()),
            )
            .build();
        host.sessions
            .prompt(&rebuilt, host.id.clone(), "second")
            .stream()
            .collect::<Vec<_>>()
            .await;
        host.sessions
            .prompt(&rebuilt, host.id.clone(), "third")
            .with_system_prompt("marker-override")
            .stream()
            .collect::<Vec<_>>()
            .await;
        let prompts = p.system_prompts();
        assert!(prompts[0].as_ref().unwrap().contains("marker-first"));
        assert!(prompts[1].as_ref().unwrap().contains("marker-first"));
        assert!(!prompts[1].as_ref().unwrap().contains("marker-second"));
        assert!(prompts[2].as_ref().unwrap().contains("marker-override"));
        let record = host.sessions.load(&host.id).await.unwrap().record;
        let states = record.journal.iter().enumerate().filter_map(|(i,e)| matches!(e,SessionJournalEvent::MiddlewareStateUpdated{namespace,..} if namespace=="system_prompt").then_some(i)).collect::<Vec<_>>();
        let requests = record
            .journal
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                matches!(e, SessionJournalEvent::ProviderRequestRecorded { .. }).then_some(i)
            })
            .collect::<Vec<_>>();
        assert_eq!(states.len(), 2);
        assert!(states[0] < requests[0]);
        assert!(requests[1] < states[1] && states[1] < requests[2]);
        host.close().await;
    }

    #[tokio::test]
    async fn captured_agents_skills_and_read_injection_survive_rebuild_and_disk_changes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::create_dir_all(root.path().join(".agents/skills/example")).unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "marker-original").unwrap();
        std::fs::write(root.path().join("src/AGENTS.md"), "marker-nested").unwrap();
        std::fs::write(root.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(
            root.path().join(".agents/skills/example/SKILL.md"),
            "---\nname: example\ndescription: Use example.\n---\nmarker-skill-original\n",
        )
        .unwrap();
        let sandbox = TestSandbox::new(root.path()).await;
        let read = |id| {
            vec![CompletionEvent::ToolCall(ToolCall::new(
                id,
                "read",
                json!({"filePath":"src/main.rs"}),
            ))]
        };
        let skill = |id| {
            vec![CompletionEvent::ToolCall(ToolCall::new(
                id,
                "load_skill",
                json!({"source":"local","name":"example"}),
            ))]
        };
        let p = provider(vec![
            read("r1"),
            skill("s1"),
            done(),
            read("r2"),
            skill("s2"),
            done(),
        ]);
        let builder = || {
            HostRuntimeBuilder::new(model(), p.clone())
                .with_sandbox(sandbox.handle())
                .with_agents_system_prompt(AgentsPromptConfig::new(root.path()))
                .with_filesystem_tools(
                    FilesystemConfig::new(root.path()).with_agents_read_context(),
                )
                .with_skills(SkillsConfig::new(root.path()))
        };
        let host = HostSession::new(builder()).await;
        let first = AgentTestSession::run(&host, p.as_ref(), "first")
            .await
            .unwrap();
        assert!(first.tool_results[0].content.contains("marker-nested"));
        assert!(first.tool_results[0].content.contains("<system-reminder>"));
        assert!(
            first.tool_results[1]
                .content
                .contains("marker-skill-original")
        );
        std::fs::write(root.path().join("AGENTS.md"), "marker-modified").unwrap();
        std::fs::write(root.path().join("src/AGENTS.md"), "marker-changed").unwrap();
        std::fs::write(
            root.path().join(".agents/skills/example/SKILL.md"),
            "---\nname: example\ndescription: Use example.\n---\nmarker-skill-modified\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join(".agents/skills/extra")).unwrap();
        std::fs::write(
            root.path().join(".agents/skills/extra/SKILL.md"),
            "---\nname: extra\ndescription: Extra.\n---\nExtra\n",
        )
        .unwrap();
        let rebuilt = builder()
            .with_dispatch_context(
                Some(host.id.clone()),
                ServerPermissionPolicy::sessions(host.broker.clone()),
            )
            .build();
        let second = AgentTestSession::from_events(
            host.sessions
                .prompt(&rebuilt, host.id.clone(), "second")
                .stream()
                .collect()
                .await,
        )
        .unwrap();
        assert!(!second.tool_results[0].content.contains("<system-reminder>"));
        assert!(!second.tool_results[0].content.contains("marker-changed"));
        assert!(second.tool_results[1].content.contains("already loaded"));
        for prompt in p.system_prompts().into_iter().flatten() {
            assert!(prompt.contains("marker-original"));
            assert!(!prompt.contains("marker-modified"));
            assert!(!prompt.contains("extra: Extra."));
        }
        let record = host.sessions.load(&host.id).await.unwrap().record;
        let snapshot = record.snapshot();
        assert!(
            snapshot.middleware_state["agents"]
                .to_string()
                .contains("marker-original")
        );
        assert!(
            snapshot.middleware_state["skills"]["loaded"]
                .to_string()
                .contains("marker-skill-original")
        );
        assert!(!snapshot.middleware_state.contains_key("sandbox_skills"));
        host.close().await;
    }
}
