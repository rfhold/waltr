//! Structured user question tool configuration.

use std::fmt;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;

use crate::{AgentTool, LlmError, Result, ToolCall, ToolDefinition};

/// Provider-facing question tool name.
pub const QUESTION_TOOL_NAME: &str = "question";

/// Configuration for the structured user question tool.
#[derive(Clone, Default)]
pub struct QuestionConfig;

impl QuestionConfig {
    /// Create question tool configuration.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Convert this configuration into the `question` agent tool.
    #[must_use]
    pub fn into_agent_tool(self) -> AgentTool {
        AgentTool::new(question_definition(), move |call| {
            let input = tool_input::<QuestionInput>(call)?;
            input.validate()?;
            Err(question_error(format!(
                "question tool call {} must suspend for external response",
                call.id
            )))
        })
    }
}

impl fmt::Debug for QuestionConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QuestionConfig")
            .finish_non_exhaustive()
    }
}

/// Input supplied by the model to ask one or more structured questions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionInput {
    /// Questions to ask the user in order.
    pub questions: Vec<QuestionPrompt>,
}

impl QuestionInput {
    /// Validate that every prompt is answerable.
    ///
    /// # Errors
    ///
    /// Returns an error if no questions are present or a prompt has no answer path.
    pub fn validate(&self) -> Result<()> {
        if self.questions.is_empty() {
            return Err(question_error(
                "question tool requires at least one question",
            ));
        }

        for question in &self.questions {
            if question.options.is_empty() && !question.custom {
                return Err(question_error(format!(
                    "question '{}' has no options and custom answers are disabled",
                    question.header
                )));
            }
        }

        Ok(())
    }
}

/// One structured question to present to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionPrompt {
    /// Complete question text.
    pub question: String,
    /// Short display label for the question.
    pub header: String,
    /// Selectable options for the question.
    pub options: Vec<QuestionOption>,
    /// Whether more than one option may be selected.
    pub multiple: bool,
    /// Whether a custom free-form answer is allowed.
    #[serde(default = "default_custom")]
    pub custom: bool,
}

/// One selectable answer option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    /// Short option label shown to the user.
    pub label: String,
    /// Concise explanation of the option.
    pub description: String,
}

/// Structured answers returned by the host callback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOutput {
    /// Answers in the same order as the requested questions.
    pub answers: Vec<QuestionAnswer>,
}

/// One structured user answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    /// Header of the answered question.
    pub header: String,
    /// Selected option labels.
    pub selections: Vec<String>,
    /// Optional free-form answer text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

fn question_definition() -> ToolDefinition {
    ToolDefinition::new(
        QUESTION_TOOL_NAME,
        "Ask the user one or more structured questions and return their answers. Use this when progress requires user preference, clarification, or a decision.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["questions"],
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["question", "header", "options", "multiple"],
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "The complete question to ask the user."
                            },
                            "header": {
                                "type": "string",
                                "description": "A very short label for the question, ideally 30 characters or fewer."
                            },
                            "options": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["label", "description"],
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "Short display text for the option."
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "A concise explanation of what this choice means."
                                        }
                                    }
                                }
                            },
                            "multiple": {
                                "type": "boolean",
                                "description": "Whether the user may select multiple options."
                            },
                            "custom": {
                                "type": "boolean",
                                "description": "Whether the host should allow a custom free-form answer. Defaults to true when omitted."
                            }
                        }
                    }
                }
            }
        }),
    )
}

fn tool_input<T>(call: &ToolCall) -> Result<T>
where
    T: DeserializeOwned,
{
    Ok(serde_json::from_value(call.arguments.clone())?)
}

const fn default_custom() -> bool {
    true
}

fn question_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: QUESTION_TOOL_NAME,
        message: message.into(),
    }
}
