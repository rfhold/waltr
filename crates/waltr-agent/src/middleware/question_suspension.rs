//! Question tool suspension middleware helpers.

use crate::{
    Result, ToolCall,
    middleware::{
        AgentMiddleware, ExternalRequestDraft, ExternalRequestKind, ExternalRequestStatus,
        HookOutcome, Suspension, context::ToolCallContext,
    },
    tools::question::{QUESTION_TOOL_NAME, QuestionInput},
};

/// Middleware that converts provider-facing `question` tool calls into suspensions.
#[derive(Debug, Clone, Default)]
pub struct QuestionSuspensionMiddleware;

impl QuestionSuspensionMiddleware {
    /// Build a suspension outcome for a valid question tool call.
    pub fn suspend_question_call(ctx: &ToolCallContext<'_>) -> Result<Option<HookOutcome>> {
        if ctx.call.name != QUESTION_TOOL_NAME {
            return Ok(None);
        }

        let input = parse_question_input(ctx.call)?;
        input.validate()?;

        let prompt = input
            .questions
            .iter()
            .map(|question| question.question.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let request = ExternalRequestDraft {
            id: format!("question:{}:{}:{}", ctx.session_id, ctx.turn, ctx.call.id),
            kind: ExternalRequestKind::Question,
            tool_call_id: Some(ctx.call.id.clone()),
            prompt,
            payload: ctx.call.arguments.clone(),
            status: ExternalRequestStatus::Pending,
        };

        Ok(Some(HookOutcome::Suspend(Suspension::external(
            "question", request,
        ))))
    }
}

impl AgentMiddleware for QuestionSuspensionMiddleware {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<HookOutcome> {
        Ok(Self::suspend_question_call(ctx)?.unwrap_or(HookOutcome::Continue))
    }
}

fn parse_question_input(call: &ToolCall) -> Result<QuestionInput> {
    Ok(serde_json::from_value::<QuestionInput>(
        call.arguments.clone(),
    )?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{
        ToolCall,
        middleware::context::{MiddlewareStateLog, ToolCallContext},
    };

    use super::*;

    #[test]
    fn question_call_suspends_with_external_request() {
        let mut call = ToolCall::new(
            "call-1",
            QUESTION_TOOL_NAME,
            json!({
                "questions": [{
                    "question": "Pick one",
                    "header": "Choice",
                    "options": [{ "label": "A", "description": "Option A" }],
                    "multiple": false
                }]
            }),
        );
        let mut claims = Vec::new();
        let mut middleware_state = MiddlewareStateLog::default();
        let ctx = ToolCallContext {
            approved_tool_name: "question",
            permission_policy: &crate::PermissionPolicy::default(),
            contextual_input_is_approved: crate::template::contextual_input_is_approved,
            contextual_suspension: crate::template::contextual_suspension,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            session_id: "session-1",
            turn: 2,
            call: &mut call,
            permission_claims: &mut claims,
            middleware_state: &mut middleware_state,
        };

        let outcome = QuestionSuspensionMiddleware::suspend_question_call(&ctx)
            .expect("question call should parse")
            .expect("question call should suspend");

        let HookOutcome::Suspend(suspension) = outcome else {
            panic!("expected suspension");
        };
        let request = suspension
            .external_request
            .expect("suspension should include external request");
        assert_eq!(request.id, "question:session-1:2:call-1");
        assert_eq!(request.kind, ExternalRequestKind::Question);
        assert_eq!(request.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(request.prompt, "Pick one");
        assert_eq!(request.status, ExternalRequestStatus::Pending);
    }

    #[test]
    fn non_question_call_continues() {
        let mut call = ToolCall::new("call-1", "lookup", json!({}));
        let mut claims = Vec::new();
        let mut middleware_state = MiddlewareStateLog::default();
        let ctx = ToolCallContext {
            approved_tool_name: "lookup",
            permission_policy: &crate::PermissionPolicy::default(),
            contextual_input_is_approved: crate::template::contextual_input_is_approved,
            contextual_suspension: crate::template::contextual_suspension,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            session_id: "session-1",
            turn: 0,
            call: &mut call,
            permission_claims: &mut claims,
            middleware_state: &mut middleware_state,
        };

        assert!(
            QuestionSuspensionMiddleware::suspend_question_call(&ctx)
                .expect("non-question should not fail")
                .is_none()
        );
    }

    #[test]
    fn invalid_question_call_errors() {
        let mut call = ToolCall::new("call-1", QUESTION_TOOL_NAME, json!({ "questions": [] }));
        let mut claims = Vec::new();
        let mut middleware_state = MiddlewareStateLog::default();
        let ctx = ToolCallContext {
            approved_tool_name: "question",
            permission_policy: &crate::PermissionPolicy::default(),
            contextual_input_is_approved: crate::template::contextual_input_is_approved,
            contextual_suspension: crate::template::contextual_suspension,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            session_id: "session-1",
            turn: 0,
            call: &mut call,
            permission_claims: &mut claims,
            middleware_state: &mut middleware_state,
        };

        let error = QuestionSuspensionMiddleware::suspend_question_call(&ctx)
            .expect_err("empty questions should fail validation");
        assert!(error.to_string().contains("at least one question"));
    }
}
