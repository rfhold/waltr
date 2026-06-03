//! Agent configuration and loop execution.

use std::{fmt, path::PathBuf};

use crate::{LlmStream, ModelRef, PermissionPolicy, SharedPermissionHandler};

mod builder;
mod context;
mod event;
mod r#loop;
mod run;
mod state;
mod tool;

pub use builder::AgentBuilder;
pub use context::AgentToolContext;
pub use event::{AgentEvent, BoxAgentEventStream};
pub use run::AgentRunBuilder;
pub use state::{AgentState, AgentTranscriptEvent};
pub use tool::AgentTool;

use crate::SkillDefinition;

/// Agent configuration and enabled capabilities.
#[derive(Clone)]
pub struct Agent {
    pub(super) model: ModelRef,
    pub(super) tools: Vec<AgentTool>,
    pub(super) system_prompt: Option<String>,
    pub(super) agents_system_paths: Vec<PathBuf>,
    pub(super) visible_skills: Vec<SkillDefinition>,
    pub(super) skills_enabled: bool,
    pub(super) permission_policy: PermissionPolicy,
    pub(super) permission_handler: Option<SharedPermissionHandler>,
}

impl Agent {
    /// Start building an agent for a model.
    #[must_use]
    pub fn builder(model: ModelRef) -> AgentBuilder {
        AgentBuilder::new(model)
    }

    /// Return the model identity configured for this agent.
    #[must_use]
    pub const fn model(&self) -> &ModelRef {
        &self.model
    }

    /// Start building an agent loop request.
    pub fn prompt<'a, P>(
        &'a self,
        provider: &'a P,
        prompt: impl Into<String>,
    ) -> AgentRunBuilder<'a, P>
    where
        P: LlmStream + ?Sized,
    {
        AgentRunBuilder::new(self, provider, prompt.into())
    }

    /// Return configured host tools.
    #[must_use]
    pub fn tools(&self) -> &[AgentTool] {
        &self.tools
    }
}

impl fmt::Debug for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Agent")
            .field("model", &self.model)
            .field("tools", &self.tools)
            .field("system_prompt", &self.system_prompt)
            .field("agents_system_paths", &self.agents_system_paths)
            .field("visible_skills", &self.visible_skills)
            .field("skills_enabled", &self.skills_enabled)
            .field("permission_policy", &self.permission_policy)
            .field("has_permission_handler", &self.permission_handler.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    use futures_util::{StreamExt, stream};
    use serde_json::json;

    use crate::{
        AgentsPromptConfig, BoxCompletionStream, BoxStreamFuture, CompletionEvent,
        CompletionRequest, FilesystemPromptConfig, LlmError, ModelApi, ModelRef, ProviderId,
        SkillsConfig, ThinkingBlock, ThinkingBlockKind, ThinkingDelta, ToolCall, ToolDefinition,
        tools::filesystem::FilesystemConfig,
    };

    use super::{Agent, AgentEvent, AgentTool};

    #[test]
    fn filesystem_tools_are_opt_in() {
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5-nano", ModelApi::OpenAiResponses);

        assert!(Agent::builder(model.clone()).build().tools().is_empty());
        assert!(
            Agent::builder(model)
                .with_filesystem_tools(FilesystemConfig::new("."))
                .build()
                .tools()
                .iter()
                .any(|tool| tool.definition().name == "apply_patch")
        );
    }

    #[tokio::test]
    async fn agents_system_prompt_is_sent_to_provider() {
        let temp = TempRoot::new("waltr-agent-static-agents-test");
        temp.write("AGENTS.md", "Prefer concise answers.\n");
        let provider = AgentsSystemPromptProvider {
            expected_path: temp.path().join("AGENTS.md"),
        };
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "hello").stream();
        let mut text = String::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::TextDelta { delta, .. } = event.expect("agent event") {
                text.push_str(&delta);
            }
        }

        assert_eq!(text, "ok");
    }

    struct AgentsSystemPromptProvider {
        expected_path: PathBuf,
    }

    impl crate::LlmStream for AgentsSystemPromptProvider {
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

    #[tokio::test]
    async fn filesystem_read_agents_context_is_opt_in_and_loaded_once() {
        let temp = TempRoot::new("waltr-agent-read-agents-test");
        temp.write("AGENTS.md", "Root instructions.\n");
        temp.write("src/AGENTS.md", "Nested instructions.\n");
        temp.write("src/main.rs", "fn main() {}\n");
        let provider = RepeatedReadProvider::default();
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_agents_read_context())
            .build();

        let mut events = agent.prompt(&provider, "read twice").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
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
        let provider = SingleReadProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_filesystem_tools(FilesystemConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "read once").stream();
        let mut tool_result = None;
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_result = Some(result.content);
            }
        }

        let tool_result = tool_result.expect("tool result");
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
        let provider = SingleReadProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_agents_read_context())
            .build();

        let mut events = agent.prompt(&provider, "read once").stream();
        let mut tool_result = None;
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_result = Some(result.content);
            }
        }

        let tool_result = tool_result.expect("tool result");
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
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "hello").stream();
        while events.next().await.is_some() {}
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
        let provider = LoadSkillProvider::new(json!({ "name": "example" }));
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "load skill").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

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
        let provider = DuplicateSkillProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
            .build();

        let mut events = agent.prompt(&provider, "read and load").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

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
        let provider = RootedSkillProvider { root: app_root };
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
            .build();

        let mut events = agent.prompt(&provider, "read and load").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

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
        let provider = RepeatedGlobProvider::default();
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
            .build();

        let mut events = agent.prompt(&provider, "glob twice").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

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
        let provider = SingleReadProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "read once").stream();
        let mut tool_result = None;
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_result = Some(result.content);
            }
        }

        let tool_result = tool_result.expect("tool result");
        assert!(!tool_result.contains("New skills are available"));
        assert!(!tool_result.contains("Use app example."));
    }

    struct SkillsSystemPromptProvider {
        expected_root: PathBuf,
    }

    impl crate::LlmStream for SkillsSystemPromptProvider {
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

                Ok(Box::pin(stream::iter(vec![Ok(CompletionEvent::Finished)]))
                    as BoxCompletionStream)
            })
        }
    }

    struct LoadSkillProvider {
        requests: Mutex<usize>,
        arguments: serde_json::Value,
    }

    impl LoadSkillProvider {
        fn new(arguments: serde_json::Value) -> Self {
            Self {
                requests: Mutex::new(0),
                arguments,
            }
        }
    }

    impl crate::LlmStream for LoadSkillProvider {
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

    impl crate::LlmStream for DuplicateSkillProvider {
        fn name(&self) -> &'static str {
            "duplicate_skill_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let events = match request.tool_results.len() {
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

    impl crate::LlmStream for RootedSkillProvider {
        fn name(&self) -> &'static str {
            "rooted_skill_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let root = self.root.display().to_string();
            Box::pin(async move {
                let events = match request.tool_results.len() {
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

    impl crate::LlmStream for RepeatedGlobProvider {
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

    impl crate::LlmStream for RepeatedReadProvider {
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

    impl crate::LlmStream for SingleReadProvider {
        fn name(&self) -> &'static str {
            "single_read_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let is_continuation = !request.tool_results.is_empty();
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

    #[tokio::test]
    async fn agent_system_prompt_fragments_are_sent_in_order() {
        let provider = SystemPromptProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_base_system_prompt()
            .with_system_date("2026-06-02")
            .with_filesystem_system_prompt(FilesystemPromptConfig::new("/tmp/waltr-root"))
            .with_system_prompt("custom fragment")
            .build();

        let mut events = agent
            .prompt(&provider, "hello")
            .with_system_prompt_fragment("run fragment")
            .stream();
        let mut text = String::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::TextDelta { delta, .. } = event.expect("agent event") {
                text.push_str(&delta);
            }
        }

        assert_eq!(text, "ok");
    }

    struct SystemPromptProvider;

    impl crate::LlmStream for SystemPromptProvider {
        fn name(&self) -> &'static str {
            "system_prompt_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let system_prompt = request.system_prompt.expect("system prompt");
                assert!(system_prompt.contains(crate::BASE_SYSTEM_PROMPT));
                assert!(system_prompt.contains("Current date: 2026-06-02."));
                assert!(system_prompt.contains("Filesystem tools are available"));
                assert!(system_prompt.contains("custom fragment"));
                assert!(system_prompt.ends_with("run fragment"));
                assert!(
                    system_prompt.find(crate::BASE_SYSTEM_PROMPT)
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

    #[tokio::test]
    async fn tool_errors_are_reported_to_agent_for_recovery() {
        let provider = RecoveringProvider;
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
        let agent = Agent::builder(ModelRef::new(
            ProviderId::OpenAi,
            "test-model",
            ModelApi::OpenAiResponses,
        ))
        .with_tool(tool)
        .build();

        let mut events = agent.prompt(&provider, "recover").stream();
        let mut saw_error_result = false;
        let mut final_text = String::new();

        while let Some(event) = events.next().await {
            match event.expect("agent event") {
                AgentEvent::ToolResult { result, .. } => {
                    saw_error_result = result.content.contains("Tool error:")
                        && result.content.contains("validation failed");
                }
                AgentEvent::TextDelta { delta, .. } => final_text.push_str(&delta),
                AgentEvent::ThinkingDelta { .. }
                | AgentEvent::ToolCall { .. }
                | AgentEvent::TurnFinished { .. }
                | AgentEvent::Finished { .. } => {}
            }
        }

        assert!(saw_error_result);
        assert_eq!(final_text, "recovered");
    }

    #[tokio::test]
    async fn agent_surfaces_and_stores_thinking_without_final_text_pollution() {
        let provider = ThinkingProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model).build();

        let mut events = agent.prompt(&provider, "think").stream();
        let mut saw_thinking_delta = false;
        let mut final_state = None;

        while let Some(event) = events.next().await {
            match event.expect("agent event") {
                AgentEvent::ThinkingDelta { delta, .. } => {
                    saw_thinking_delta = delta == "hidden thought";
                }
                AgentEvent::Finished { state } => final_state = Some(state),
                AgentEvent::TextDelta { .. }
                | AgentEvent::ToolCall { .. }
                | AgentEvent::ToolResult { .. }
                | AgentEvent::TurnFinished { .. } => {}
            }
        }

        let state = final_state.expect("final state");
        assert!(saw_thinking_delta);
        assert_eq!(state.last_assistant_text(), "final answer");
        assert!(matches!(
            state.events[0],
            super::AgentTranscriptEvent::Thinking { ref text, .. } if text == "hidden thought"
        ));
        assert_eq!(state.thinking_blocks.len(), 1);
    }

    struct ThinkingProvider;

    impl crate::LlmStream for ThinkingProvider {
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

    #[tokio::test]
    async fn agent_replays_thinking_blocks_on_tool_continuation() {
        let provider = ReplayThinkingProvider::default();
        let tool = AgentTool::new(
            ToolDefinition::new(
                "lookup",
                "Look up test data.",
                json!({ "type": "object", "additionalProperties": false }),
            ),
            |_call| Ok("tool output".to_owned()),
        );
        let agent = Agent::builder(ModelRef::new(
            ProviderId::OpenAi,
            "test-model",
            ModelApi::OpenAiResponses,
        ))
        .with_tool(tool)
        .build();

        let mut events = agent.prompt(&provider, "use tool").stream();
        while events.next().await.is_some() {}

        assert!(provider.saw_replayed_thinking());
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

    impl crate::LlmStream for ReplayThinkingProvider {
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
                        request.thinking_blocks.len() == 1;
                    Ok(Box::pin(stream::iter(vec![
                        Ok(CompletionEvent::Delta("done".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ])) as BoxCompletionStream)
                }
            })
        }
    }

    struct RecoveringProvider;

    impl crate::LlmStream for RecoveringProvider {
        fn name(&self) -> &'static str {
            "recovering_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let events = if request.tool_results.is_empty() {
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
                        request.tool_results[0]
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

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(prefix: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time after epoch")
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
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
}
