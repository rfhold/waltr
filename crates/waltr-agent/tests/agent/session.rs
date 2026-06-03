//! Shared agent test session helpers.

use std::{env, fmt::Write};

use futures_util::StreamExt;
use waltr_agent::{Agent, AgentEvent, AgentState, LlmError, LlmStream, ToolResult};

#[allow(dead_code)]
pub const REPORT_TOOL_ERRORS_ENV: &str = "WALTR_AGENT_REPORT_TOOL_ERRORS";
pub const TEST_HARNESS_SYSTEM_PROMPT: &str = "You are running inside an automated test harness. Follow the user instructions exactly. Use tools when the user explicitly asks you to use them. When asked to report exact values, include those values verbatim. Keep final answers concise and do not add extra commentary unless requested.";

#[derive(Debug, Default)]
pub struct AgentTestSession {
    pub text: String,
    pub final_state: Option<AgentState>,
    pub saw_text_after_tool_result: bool,
    tool_calls: usize,
    tool_results: usize,
    tool_errors: Vec<ToolResult>,
}

#[allow(dead_code)]
pub fn report_tool_errors(provider: &impl LlmStream, session: &AgentTestSession, label: &str) {
    session.report_tool_errors_if_requested(provider.name(), label);
}

pub fn session_context(provider: &impl LlmStream, session: &AgentTestSession) -> String {
    format!(
        "{}: {}\n{}",
        provider.name(),
        session.text,
        session.tool_error_report()
    )
}

impl AgentTestSession {
    pub async fn run(
        agent: &Agent,
        provider: &impl LlmStream,
        prompt: &str,
    ) -> waltr_agent::Result<Self> {
        let mut stream = agent
            .prompt(provider, prompt)
            .with_system_prompt_fragment(TEST_HARNESS_SYSTEM_PROMPT)
            .stream();
        let mut session = Self::default();
        let mut saw_tool_result = false;

        while let Some(event) = stream.next().await {
            match event? {
                AgentEvent::TextDelta { delta, .. } => {
                    if saw_tool_result && !delta.is_empty() {
                        session.saw_text_after_tool_result = true;
                    }
                    session.text.push_str(&delta);
                }
                AgentEvent::ToolCall { .. } => session.tool_calls += 1,
                AgentEvent::ToolResult { result, .. } => {
                    session.tool_results += 1;
                    saw_tool_result = true;
                    if result.content.starts_with("Tool error:") {
                        session.tool_errors.push(result);
                    }
                }
                AgentEvent::ThinkingDelta { .. } | AgentEvent::TurnFinished { .. } => {}
                AgentEvent::Finished { state } => session.final_state = Some(state),
            }
        }

        Ok(session)
    }

    pub fn require_tool_execution(&self, provider: &'static str) -> waltr_agent::Result<()> {
        if self.tool_calls == 0 || self.tool_results == 0 {
            return Err(LlmError::Protocol {
                provider,
                message: "agent prompt did not execute a tool".to_owned(),
            });
        }

        Ok(())
    }

    pub fn tool_error_report(&self) -> String {
        if self.tool_errors.is_empty() {
            return "tool errors: none".to_owned();
        }

        let mut report = String::from("tool errors:");
        for error in &self.tool_errors {
            write!(
                report,
                "\n- {}({}): {}",
                error.name, error.call_id, error.content
            )
            .expect("write string report");
        }
        report
    }

    #[allow(dead_code)]
    pub fn report_tool_errors_if_requested(&self, provider: &str, label: &str) {
        if !report_tool_errors_enabled() || self.tool_errors.is_empty() {
            return;
        }

        eprintln!(
            "{provider} {label} {}\n{}",
            REPORT_TOOL_ERRORS_ENV,
            self.tool_error_report()
        );
    }
}

#[allow(dead_code)]
fn report_tool_errors_enabled() -> bool {
    env::var(REPORT_TOOL_ERRORS_ENV)
        .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
}
