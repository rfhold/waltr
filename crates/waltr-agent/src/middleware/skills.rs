//! The single provider-facing skill loader and adapter router.

use serde::Deserialize;
use serde_json::json;

use super::{
    AgentMiddleware, HookOutcome,
    context::{RequestContext, ToolCallContext, ToolDraft},
    mcp_skills::McpSkillsMiddleware,
    sandbox_skills::SandboxSkillsMiddleware,
};
use crate::{LlmError, Result, ToolDefinition};

#[derive(Debug, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum SkillSelector {
    Local { name: String, root: Option<String> },
    Mcp { server: String, uri: String },
}

pub(crate) fn selector(ctx: &ToolCallContext<'_>) -> Result<SkillSelector> {
    parse_selector(&ctx.call.arguments)
}

pub(crate) fn parse_selector(arguments: &serde_json::Value) -> Result<SkillSelector> {
    let selector: SkillSelector =
        serde_json::from_value(arguments.clone()).map_err(|error| LlmError::Protocol {
            provider: "skills",
            message: format!("invalid load_skill selector: {error}"),
        })?;
    let valid = match &selector {
        SkillSelector::Local { name, root } => {
            !name.is_empty()
                && root.as_ref().is_none_or(|root| !root.is_empty())
                && arguments
                    .get("root")
                    .is_none_or(serde_json::Value::is_string)
        }
        SkillSelector::Mcp { server, uri } => {
            !server.is_empty() && reqwest::Url::parse(uri).is_ok() && uri.ends_with("/SKILL.md")
        }
    };
    if !valid {
        return Err(LlmError::Protocol {
            provider: "skills",
            message: "invalid load_skill selector values".into(),
        });
    }
    Ok(selector)
}

#[derive(Debug, Clone)]
pub(crate) struct SkillsMiddleware {
    local: Option<SandboxSkillsMiddleware>,
    remote: McpSkillsMiddleware,
    remote_enabled: bool,
}

impl SkillsMiddleware {
    pub(crate) fn new(
        local: Option<SandboxSkillsMiddleware>,
        remote: McpSkillsMiddleware,
        remote_enabled: bool,
    ) -> Self {
        Self {
            local,
            remote,
            remote_enabled,
        }
    }
}

impl AgentMiddleware for SkillsMiddleware {
    async fn list_tools(&self, _ctx: &mut RequestContext<'_>, tools: &mut ToolDraft) -> Result<()> {
        if self.local.is_some() || self.remote_enabled {
            tools.push(ToolDefinition::new(
                "load_skill",
                "Load a visible local skill or an origin-qualified MCP skill. MCP activation requires explicit manifest-bound consent. Always specify source.",
                json!({
                    "type": "object",
                    "properties": {
                        "source": {"type": "string", "enum": ["local", "mcp"]},
                        "name": {"type": "string", "minLength": 1},
                        "root": {"type": "string", "minLength": 1},
                        "server": {"type": "string", "minLength": 1},
                        "uri": {"type": "string", "minLength": 1}
                    },
                    "required": ["source"],
                    "oneOf": [
                        {
                            "properties": {"source": {"const": "local"}},
                            "required": ["name"],
                            "not": {"anyOf": [{"required": ["server"]}, {"required": ["uri"]}]}
                        },
                        {
                            "properties": {"source": {"const": "mcp"}},
                            "required": ["server", "uri"],
                            "not": {"anyOf": [{"required": ["name"]}, {"required": ["root"]}]}
                        }
                    ],
                    "additionalProperties": false
                }),
            ));
        }
        Ok(())
    }

    async fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<HookOutcome> {
        if ctx.call.name != "load_skill" {
            return Ok(HookOutcome::Continue);
        }
        match selector(ctx)? {
            SkillSelector::Local { name, root } => {
                let local = self.local.as_ref().ok_or_else(|| LlmError::Protocol {
                    provider: "skills",
                    message: "local skills are disabled".into(),
                })?;
                local.load(ctx, &name, root.as_deref()).await
            }
            SkillSelector::Mcp { server, uri } => {
                if !self.remote_enabled {
                    return Err(LlmError::Protocol {
                        provider: "skills",
                        message: "MCP skills are disabled".into(),
                    });
                }
                self.remote.load(ctx, &server, &uri).await
            }
        }
    }
}
