//! Live, operation-bound question handling.

use crate::agent_host::{
    Result, ToolResult,
    middleware::{AgentMiddleware, HookOutcome, context::ToolCallContext},
    tools::question::{QUESTION_TOOL_NAME, QuestionAnswer, QuestionInput, QuestionOutput},
};
use crate::interaction::{Interaction, InteractionResponse};

#[derive(Debug, Clone, Default)]
pub struct QuestionMiddleware;

impl AgentMiddleware for QuestionMiddleware {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<HookOutcome> {
        if ctx.call.name != QUESTION_TOOL_NAME {
            return Ok(HookOutcome::Continue);
        }
        let input: QuestionInput = serde_json::from_value(ctx.call.arguments.clone())?;
        input.validate()?;
        let result = match ctx
            .interact(Interaction::Question {
                input: input.clone(),
            })
            .await
        {
            Ok(InteractionResponse::Question(answers)) => {
                let answers = input
                    .questions
                    .into_iter()
                    .zip(answers)
                    .map(|(question, values)| {
                        let (selections, custom): (Vec<_>, Vec<_>) =
                            values.into_iter().partition(|value| {
                                question.options.iter().any(|option| option.label == *value)
                            });
                        QuestionAnswer {
                            header: question.header,
                            selections,
                            custom: (!custom.is_empty()).then(|| custom.join("\n")),
                        }
                    })
                    .collect();
                ToolResult::new(
                    ctx.call.id.clone(),
                    ctx.call.name.clone(),
                    serde_json::to_string(&QuestionOutput { answers })?,
                )
            }
            _ => ToolResult::new(
                ctx.call.id.clone(),
                ctx.call.name.clone(),
                "question interaction unavailable or cancelled",
            )
            .with_error(),
        };
        Ok(HookOutcome::ReplaceToolResult(result))
    }
}
