//! Tool-list middleware helpers.

use serde_json::json;

use crate::{
    AgentTool, PermissionLevel, PermissionPolicy, Result, ToolDefinition,
    resource::{
        SessionTemplateKind, SessionTemplateOption, SessionTemplateSearchTarget, tool_score,
        tool_template,
    },
    template::{TemplateReference, TemplateReferenceRender, render_tool_template_reference},
};

use super::{
    AgentMiddleware,
    context::{RequestContext, TemplateRenderContext, TemplateSearchContext, ToolDraft},
};

/// Middleware that exposes registered tools according to the active permission policy.
#[derive(Debug, Clone)]
pub struct PermissionFilteredToolRegistryMiddleware {
    tools: Vec<AgentTool>,
    policy: PermissionPolicy,
    ask_available: bool,
}

impl PermissionFilteredToolRegistryMiddleware {
    /// Create middleware from provider-neutral tools and permission visibility inputs.
    pub const fn new(tools: Vec<AgentTool>, policy: PermissionPolicy, ask_available: bool) -> Self {
        Self {
            tools,
            policy,
            ask_available,
        }
    }
}

impl AgentMiddleware for PermissionFilteredToolRegistryMiddleware {
    async fn list_tools(&self, _ctx: &mut RequestContext<'_>, draft: &mut ToolDraft) -> Result<()> {
        draft.tools.extend(
            self.tools
                .iter()
                .filter(|tool| model_visible(tool))
                .map(AgentTool::definition)
                .cloned(),
        );
        Ok(())
    }

    async fn search_templates(
        &self,
        ctx: &TemplateSearchContext<'_>,
        results: &mut crate::resource::TemplateSearchResults,
    ) -> Result<()> {
        if ctx.request.target != SessionTemplateSearchTarget::Resources
            || !ctx.request.includes(SessionTemplateKind::Tool)
        {
            return Ok(());
        }

        let query_lower = ctx.request.query.trim().to_lowercase();
        let limit = ctx.request.effective_limit();
        for tool in &self.tools {
            if !model_visible(tool) {
                continue;
            }
            let definition = tool.definition();
            if !tool_is_visible(&self.policy, definition, self.ask_available) {
                continue;
            }
            let Some(score) = tool_score(&definition.name, &definition.description, &query_lower)
            else {
                continue;
            };
            results.push_limited(
                score,
                SessionTemplateOption {
                    kind: SessionTemplateKind::Tool,
                    label: definition.name.clone(),
                    detail: definition.description.clone(),
                    template: tool_template(&definition.name, &definition.input_schema),
                    metadata: json!({
                        "name": definition.name.clone(),
                        "description": definition.description.clone(),
                        "inputSchema": definition.input_schema.clone(),
                    }),
                },
                limit,
            );
        }

        Ok(())
    }

    async fn render_template_reference(
        &self,
        ctx: &mut TemplateRenderContext<'_>,
        reference: &TemplateReference,
    ) -> Result<TemplateReferenceRender> {
        let TemplateReference::Tool {
            name, arguments, ..
        } = reference
        else {
            return Ok(TemplateReferenceRender::Continue);
        };
        if !self
            .tools
            .iter()
            .any(|tool| model_visible(tool) && tool.definition().name == *name)
        {
            return Ok(TemplateReferenceRender::Continue);
        }

        render_tool_template_reference(ctx, &self.tools, name, arguments.clone()).await
    }
}

pub(crate) fn model_visible(tool: &AgentTool) -> bool {
    tool.mcp_app_source
        .as_ref()
        .is_none_or(|source| source.ui.visible_to(mcp::apps::UiVisibility::Model))
}

/// Return true when a tool should be visible to the model for the given permission setup.
pub fn tool_is_visible(
    policy: &PermissionPolicy,
    definition: &ToolDefinition,
    ask_available: bool,
) -> bool {
    match policy.evaluate_tool(&definition.name) {
        PermissionLevel::Allow => true,
        PermissionLevel::Ask => ask_available,
        PermissionLevel::Deny => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{
        AgentTool,
        middleware::context::{MiddlewareStateLog, RequestContext},
    };

    use super::*;

    fn request_context<'a>(
        middleware_state: &'a mut MiddlewareStateLog,
        system_prompt_override: &'a mut Option<crate::middleware::prompt::SystemPromptOverride>,
    ) -> RequestContext<'a> {
        RequestContext {
            middleware_state,
            session_metadata_values: std::collections::BTreeMap::new(),
            system_prompt_override,
            middleware_chain: crate::middleware::chain::MiddlewareChain::new(),
            permission_policy: crate::PermissionPolicy::default(),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            session_id: "test-session",
            turn: 0,
        }
    }

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition::new(
            name,
            format!("{name} description"),
            json!({
                "type": "object",
                "properties": {}
            }),
        )
    }

    fn agent_tool(definition: ToolDefinition) -> AgentTool {
        AgentTool::new(definition, |call| Ok(call.result("unused")))
    }

    #[tokio::test]
    async fn leaves_registered_tool_filtering_to_the_middleware_chain() {
        use crate::{PermissionEffect, PermissionToolSelector};

        let allowed = tool("allowed");
        let ask = tool("ask");
        let denied = tool("denied");
        let policy = PermissionPolicy::default()
            .with_tool_rule(
                PermissionToolSelector::exact("ask"),
                PermissionEffect::ask(),
            )
            .with_tool_rule(
                PermissionToolSelector::exact("denied"),
                PermissionEffect::deny(),
            );
        let mut middleware_state = MiddlewareStateLog::default();
        let mut system_prompt_override = None;
        let mut ctx = request_context(&mut middleware_state, &mut system_prompt_override);
        ctx.permission_policy = policy.clone();
        let mut draft = ToolDraft::default();

        PermissionFilteredToolRegistryMiddleware::new(
            vec![
                agent_tool(allowed.clone()),
                agent_tool(ask.clone()),
                agent_tool(denied),
            ],
            policy,
            true,
        )
        .list_tools(&mut ctx, &mut draft)
        .await
        .expect("filtered tool middleware should run");

        assert_eq!(draft.tools, vec![allowed, ask, tool("denied")]);
    }

    #[tokio::test]
    async fn does_not_filter_registered_ask_tools() {
        use crate::{PermissionEffect, PermissionToolSelector};

        let allowed = tool("allowed");
        let ask = tool("ask");
        let policy = PermissionPolicy::default().with_tool_rule(
            PermissionToolSelector::exact("ask"),
            PermissionEffect::ask(),
        );
        let mut middleware_state = MiddlewareStateLog::default();
        let mut system_prompt_override = None;
        let mut ctx = request_context(&mut middleware_state, &mut system_prompt_override);
        ctx.permission_policy = policy.clone();
        let mut draft = ToolDraft::default();

        PermissionFilteredToolRegistryMiddleware::new(
            vec![agent_tool(allowed.clone()), agent_tool(ask)],
            policy,
            false,
        )
        .list_tools(&mut ctx, &mut draft)
        .await
        .expect("filtered tool middleware should run");

        assert_eq!(draft.tools, vec![allowed, tool("ask")]);
    }
}
