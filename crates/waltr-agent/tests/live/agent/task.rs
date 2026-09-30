//! Live provider `task` tool integration tests.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use waltr_agent::{
    AgentRuntimeProvider, AgentSessionRuntimeFactory, BoxStreamFuture, CompletionRequest,
    FileCredentialStore, InferenceProviderSettings, InferenceSettings, LlmError, LlmStream,
    MemorySessionStore, ModelRef, SessionCoordinator, SessionId,
    tools::{question::QUESTION_TOOL_NAME, task::TASK_TOOL_NAME},
};
use serde_json::Value;
use tokio::time::timeout;

use crate::{live_provider, scripted_provider::CapturingProvider};

const LIVE_TASK_CASE_TIMEOUT: Duration = Duration::from_secs(30);
const LIVE_TASK_CASE_REQUEST_LIMIT: usize = 5;
const TASK_PROMPT: &str = r#"use the task tool and give it the prompt "return a comma separated list of your tool names" then once its complete, repeat that bare list back with no other content"#;

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_task_tool_visibility_follows_configured_depth() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::selected_from_env(
        "WALTR_AGENT_LIVE_PROVIDER",
        "openai_responses_websocket",
    ))?
    else {
        return Ok(());
    };
    run_live_task_tool_visibility(live.provider, live.model).await
}

async fn run_live_task_tool_visibility(
    provider: impl LlmStream + 'static,
    model: ModelRef,
) -> waltr_agent::Result<()> {
    let live_provider = Arc::new(provider);
    let cases = [
        ToolVisibilityCase {
            name: "depth 1 child hides task",
            max_depth: 1,
            expect_task: false,
        },
        ToolVisibilityCase {
            name: "depth 2 child exposes task",
            max_depth: 2,
            expect_task: true,
        },
    ];

    for case in cases {
        let provider = Arc::new(CapturingProvider::new(RequestLimitProvider::new(
            Arc::clone(&live_provider),
            LIVE_TASK_CASE_REQUEST_LIMIT,
        )));
        let sessions = task_sessions(model.clone(), Arc::clone(&provider), case.max_depth);
        let session_id = SessionId::new(format!("live-task-depth-{}", case.max_depth));
        let mut request = waltr_agent::SessionCreateRequest::new(session_id.clone());
        request.inference_settings = Some(inference_settings(&model));
        sessions.create(request).await?;
        let submission = sessions
            .submit_prompt(
                session_id,
                TASK_PROMPT,
                waltr_agent::RunConflictPolicy::ReplaceActive,
            )
            .await?;
        let outcome = timeout(LIVE_TASK_CASE_TIMEOUT, submission.handle.wait())
            .await
            .map_err(|_| case.timeout_error(&provider))?
            .map_err(|error| case.run_error(&provider, error))?;
        let output = outcome.assistant_text.unwrap_or_default();
        let tools = parse_tool_csv(&output);
        let context = case.context(&provider, &output, &tools);

        assert!(
            provider
                .all_tool_names()
                .iter()
                .any(|tool| tool == TASK_TOOL_NAME),
            "{context}"
        );
        assert!(
            tools.iter().any(|tool| tool == QUESTION_TOOL_NAME),
            "{context}"
        );
        assert_eq!(
            tools.iter().any(|tool| tool == TASK_TOOL_NAME),
            case.expect_task,
            "{context}"
        );
    }

    Ok(())
}

struct RequestLimitProvider<P> {
    inner: P,
    max_requests: usize,
    requests: AtomicUsize,
}

impl<P> RequestLimitProvider<P> {
    const fn new(inner: P, max_requests: usize) -> Self {
        Self {
            inner,
            max_requests,
            requests: AtomicUsize::new(0),
        }
    }
}

impl<P> LlmStream for RequestLimitProvider<P>
where
    P: LlmStream,
{
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<Value> {
        self.inner.project_request(request)
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let request_count = self.requests.fetch_add(1, Ordering::SeqCst) + 1;
        if request_count > self.max_requests {
            let provider = self.name();
            let max_requests = self.max_requests;
            drop(request);
            return Box::pin(async move {
                Err(LlmError::Protocol {
                    provider,
                    message: format!(
                        "live task visibility exceeded {max_requests} provider requests"
                    ),
                })
            });
        }

        self.inner.stream_request(request)
    }
}

#[derive(Clone, Copy)]
struct ToolVisibilityCase {
    name: &'static str,
    max_depth: usize,
    expect_task: bool,
}

impl ToolVisibilityCase {
    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn run_error<P: LlmStream, E: std::fmt::Display>(
        &self,
        provider: &CapturingProvider<P>,
        error: E,
    ) -> LlmError {
        LlmError::Protocol {
            provider: provider.name(),
            message: format!(
                "live task visibility case failed: {}\nerror: {error}\n{}",
                self.name,
                provider.tool_context()
            ),
        }
    }

    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn timeout_error<P: LlmStream>(&self, provider: &CapturingProvider<P>) -> LlmError {
        LlmError::Protocol {
            provider: provider.name(),
            message: format!(
                "live task visibility case timed out after {:?}: {}\n{}",
                LIVE_TASK_CASE_TIMEOUT,
                self.name,
                provider.tool_context()
            ),
        }
    }

    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn context<P: LlmStream>(
        &self,
        provider: &CapturingProvider<P>,
        output: &str,
        tools: &[String],
    ) -> String {
        format!(
            "case: {}\nmax depth: {}\nexpected task: {}\noutput: {:?}\nparsed tools: {:?}\n{}",
            self.name,
            self.max_depth,
            self.expect_task,
            output,
            tools,
            provider.tool_context()
        )
    }
}

fn task_sessions<P>(
    model: ModelRef,
    provider: Arc<P>,
    max_depth: usize,
) -> SessionCoordinator<MemorySessionStore>
where
    P: LlmStream + 'static,
{
    let environment = AgentSessionRuntimeFactory::new(
        waltr_agent::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
        std::env::temp_dir(),
        FileCredentialStore::new(std::env::temp_dir().join("waltr-agent-live-task-auth.json")),
        crate::agent_definitions::live_task_visibility_agent(max_depth, tool_names_system_prompt()),
    )
    .with_runtime_provider(move |_| {
        Ok(AgentRuntimeProvider::new(
            model.clone(),
            Arc::clone(&provider),
        ))
    });

    SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(environment)
}

fn inference_settings(model: &ModelRef) -> InferenceSettings {
    InferenceSettings {
        provider: InferenceProviderSettings {
            key: "live".to_owned(),
            api: model.api,
            url: String::new(),
            model_id: model.model.as_str().to_owned(),
            model_cost: None,
            headers: std::collections::BTreeMap::default(),
            oauth: None,
            reasoning_effort: None,
        },
    }
}

const fn tool_names_system_prompt() -> &'static str {
    r#"This is a live test for tool visibility. Follow these rules exactly:
- If the user asks you to use the task tool, call the task tool exactly once with the prompt exactly "return a comma separated list of your tool names".
- After a task result arrives, never call the task tool again in that turn. Extract the JSON result field and answer with only that bare value.
- If the user asks for a comma separated list of your tool names, do not call any tool. Answer with only the exact tool names available in the current request, separated by commas.
- Include question when it is available. Include task only when a tool named task is available in the current request. Never invent task when it is hidden.
- Never call question."#
}

fn parse_tool_csv(output: &str) -> Vec<String> {
    output
        .split(',')
        .map(str::trim)
        .filter(|tool| !tool.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}
