//! Tool-list middleware helpers.

use crate::{PermissionLevel, PermissionPolicy, Result, ToolDefinition};

use super::{AgentMiddleware, context::RequestContext, context::ToolDraft};

/// Middleware that exposes registered tools according to the active permission policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionFilteredToolRegistryMiddleware {
    tools: Vec<ToolDefinition>,
    policy: PermissionPolicy,
    ask_available: bool,
}

impl PermissionFilteredToolRegistryMiddleware {
    /// Create middleware from provider-neutral tool definitions and permission visibility inputs.
    pub const fn new(
        tools: Vec<ToolDefinition>,
        policy: PermissionPolicy,
        ask_available: bool,
    ) -> Self {
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
                .filter(|tool| tool_is_visible(&self.policy, tool, self.ask_available))
                .cloned(),
        );
        Ok(())
    }
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

    use crate::middleware::context::{MiddlewareStateLog, RequestContext};

    use super::*;

    fn request_context<'a>(
        middleware_state: &'a mut MiddlewareStateLog,
        system_prompt_override: &'a mut Option<crate::middleware::prompt::SystemPromptOverride>,
    ) -> RequestContext<'a> {
        RequestContext {
            middleware_state,
            session_metadata_values: std::collections::BTreeMap::new(),
            system_prompt_override,
            tools: &[],
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

    #[tokio::test]
    async fn filters_tool_definitions_by_permission_visibility() {
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
        let mut draft = ToolDraft::default();

        PermissionFilteredToolRegistryMiddleware::new(
            vec![allowed.clone(), ask.clone(), denied],
            policy,
            true,
        )
        .list_tools(&mut ctx, &mut draft)
        .await
        .expect("filtered tool middleware should run");

        assert_eq!(draft.tools, vec![allowed, ask]);
    }

    #[tokio::test]
    async fn hides_ask_tool_definitions_without_ask_path() {
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
        let mut draft = ToolDraft::default();

        PermissionFilteredToolRegistryMiddleware::new(vec![allowed.clone(), ask], policy, false)
            .list_tools(&mut ctx, &mut draft)
            .await
            .expect("filtered tool middleware should run");

        assert_eq!(draft.tools, vec![allowed]);
    }
}
