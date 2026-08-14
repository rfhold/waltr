//! Deterministic middleware chain execution.

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use crate::{
    Result,
    resource::TemplateSearchResults,
    template::{TemplateReference, TemplateReferenceRender},
};

use super::{
    AgentMiddleware, HookOutcome, Suspension,
    context::{
        ModelRequestDraft, RequestContext, RunContext, SystemPromptParts, TemplateRenderContext,
        TemplateSearchContext, ToolCallContext, ToolDraft, ToolResultContext,
    },
};

type BoxHookFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

trait ChainMiddleware: Send + Sync {
    fn build_system_prompt<'a>(
        &'a self,
        ctx: &'a mut RequestContext<'_>,
        prompt: &'a mut SystemPromptParts,
    ) -> BoxHookFuture<'a, HookOutcome>;

    fn list_tools<'a>(
        &'a self,
        ctx: &'a mut RequestContext<'_>,
        tools: &'a mut ToolDraft,
    ) -> BoxHookFuture<'a, ()>;

    fn edit_tool_metadata<'a>(
        &'a self,
        ctx: &'a mut RequestContext<'_>,
        tools: &'a mut ToolDraft,
    ) -> BoxHookFuture<'a, ()>;

    fn before_provider_request<'a>(
        &'a self,
        ctx: &'a mut RequestContext<'_>,
        request: &'a mut ModelRequestDraft,
    ) -> BoxHookFuture<'a, HookOutcome>;

    fn search_templates<'a>(
        &'a self,
        ctx: &'a TemplateSearchContext<'_>,
        results: &'a mut TemplateSearchResults,
    ) -> BoxHookFuture<'a, ()>;

    fn render_template_reference<'a>(
        &'a self,
        ctx: &'a mut TemplateRenderContext<'_>,
        reference: &'a TemplateReference,
    ) -> BoxHookFuture<'a, TemplateReferenceRender>;

    fn before_tool_call<'a>(
        &'a self,
        ctx: &'a mut ToolCallContext<'_>,
    ) -> BoxHookFuture<'a, HookOutcome>;

    fn after_tool_call<'a>(
        &'a self,
        ctx: &'a mut ToolResultContext<'_>,
    ) -> BoxHookFuture<'a, HookOutcome>;

    fn on_suspend<'a>(
        &'a self,
        ctx: &'a mut RunContext<'_>,
        suspension: &'a mut Suspension,
    ) -> BoxHookFuture<'a, ()>;
}

impl<T> ChainMiddleware for T
where
    T: AgentMiddleware + 'static,
{
    fn build_system_prompt<'a>(
        &'a self,
        ctx: &'a mut RequestContext<'_>,
        prompt: &'a mut SystemPromptParts,
    ) -> BoxHookFuture<'a, HookOutcome> {
        Box::pin(AgentMiddleware::build_system_prompt(self, ctx, prompt))
    }

    fn list_tools<'a>(
        &'a self,
        ctx: &'a mut RequestContext<'_>,
        tools: &'a mut ToolDraft,
    ) -> BoxHookFuture<'a, ()> {
        Box::pin(AgentMiddleware::list_tools(self, ctx, tools))
    }

    fn edit_tool_metadata<'a>(
        &'a self,
        ctx: &'a mut RequestContext<'_>,
        tools: &'a mut ToolDraft,
    ) -> BoxHookFuture<'a, ()> {
        Box::pin(AgentMiddleware::edit_tool_metadata(self, ctx, tools))
    }

    fn before_provider_request<'a>(
        &'a self,
        ctx: &'a mut RequestContext<'_>,
        request: &'a mut ModelRequestDraft,
    ) -> BoxHookFuture<'a, HookOutcome> {
        Box::pin(AgentMiddleware::before_provider_request(self, ctx, request))
    }

    fn search_templates<'a>(
        &'a self,
        ctx: &'a TemplateSearchContext<'_>,
        results: &'a mut TemplateSearchResults,
    ) -> BoxHookFuture<'a, ()> {
        Box::pin(AgentMiddleware::search_templates(self, ctx, results))
    }

    fn render_template_reference<'a>(
        &'a self,
        ctx: &'a mut TemplateRenderContext<'_>,
        reference: &'a TemplateReference,
    ) -> BoxHookFuture<'a, TemplateReferenceRender> {
        Box::pin(AgentMiddleware::render_template_reference(
            self, ctx, reference,
        ))
    }

    fn before_tool_call<'a>(
        &'a self,
        ctx: &'a mut ToolCallContext<'_>,
    ) -> BoxHookFuture<'a, HookOutcome> {
        Box::pin(AgentMiddleware::before_tool_call(self, ctx))
    }

    fn after_tool_call<'a>(
        &'a self,
        ctx: &'a mut ToolResultContext<'_>,
    ) -> BoxHookFuture<'a, HookOutcome> {
        Box::pin(AgentMiddleware::after_tool_call(self, ctx))
    }

    fn on_suspend<'a>(
        &'a self,
        ctx: &'a mut RunContext<'_>,
        suspension: &'a mut Suspension,
    ) -> BoxHookFuture<'a, ()> {
        Box::pin(AgentMiddleware::on_suspend(self, ctx, suspension))
    }
}

/// Ordered middleware collection for deterministic hook execution.
#[derive(Clone, Default)]
pub(crate) struct MiddlewareChain {
    middleware: Vec<Arc<dyn ChainMiddleware>>,
}

impl fmt::Debug for MiddlewareChain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MiddlewareChain")
            .field("len", &self.middleware.len())
            .finish()
    }
}

impl MiddlewareChain {
    /// Create an empty middleware chain.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Return the number of middleware entries in insertion order.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.middleware.len()
    }

    /// Add middleware to the end of the chain.
    pub(crate) fn push<M>(&mut self, middleware: M) -> &mut Self
    where
        M: AgentMiddleware + 'static,
    {
        self.middleware.push(Arc::new(middleware));
        self
    }

    /// Append another middleware chain, preserving its insertion order.
    pub(crate) fn extend(&mut self, middleware_chain: Self) -> &mut Self {
        self.middleware.extend(middleware_chain.middleware);
        self
    }

    /// Run system prompt middleware in insertion order.
    pub(crate) async fn build_system_prompt(
        &self,
        ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<HookOutcome> {
        for middleware in &self.middleware {
            let outcome = middleware.build_system_prompt(ctx, prompt).await?;
            if !matches!(outcome, HookOutcome::Continue) {
                return Ok(outcome);
            }
        }

        Ok(HookOutcome::Continue)
    }

    /// Run tool-list middleware in insertion order.
    pub(crate) async fn list_tools(
        &self,
        ctx: &mut RequestContext<'_>,
        tools: &mut ToolDraft,
    ) -> Result<()> {
        for middleware in &self.middleware {
            middleware.list_tools(ctx, tools).await?;
        }
        tools
            .tools
            .retain(|tool| super::tools::tool_is_visible(&ctx.permission_policy, tool, true));

        Ok(())
    }

    /// Run tool metadata middleware in insertion order.
    pub(crate) async fn edit_tool_metadata(
        &self,
        ctx: &mut RequestContext<'_>,
        tools: &mut ToolDraft,
    ) -> Result<()> {
        for middleware in &self.middleware {
            middleware.edit_tool_metadata(ctx, tools).await?;
        }

        Ok(())
    }

    /// Run provider request middleware until the first non-continue outcome.
    pub(crate) async fn before_provider_request(
        &self,
        ctx: &mut RequestContext<'_>,
        request: &mut ModelRequestDraft,
    ) -> Result<HookOutcome> {
        for middleware in &self.middleware {
            let outcome = middleware.before_provider_request(ctx, request).await?;
            if !matches!(outcome, HookOutcome::Continue) {
                return Ok(outcome);
            }
        }

        Ok(HookOutcome::Continue)
    }

    /// Run template-search middleware in insertion order.
    pub(crate) async fn search_templates(
        &self,
        ctx: &TemplateSearchContext<'_>,
        results: &mut TemplateSearchResults,
    ) -> Result<()> {
        for middleware in &self.middleware {
            middleware.search_templates(ctx, results).await?;
        }

        Ok(())
    }

    /// Run template-render middleware until the first owner renders the reference.
    pub(crate) async fn render_template_reference(
        &self,
        ctx: &mut TemplateRenderContext<'_>,
        reference: &TemplateReference,
    ) -> Result<TemplateReferenceRender> {
        for middleware in &self.middleware {
            let rendered = middleware.render_template_reference(ctx, reference).await?;
            if !matches!(rendered, TemplateReferenceRender::Continue) {
                return Ok(rendered);
            }
        }

        Ok(TemplateReferenceRender::Continue)
    }

    /// Run pre-tool-call middleware until the first non-continue outcome.
    pub(crate) async fn before_tool_call(
        &self,
        ctx: &mut ToolCallContext<'_>,
    ) -> Result<HookOutcome> {
        for middleware in &self.middleware {
            let outcome = middleware.before_tool_call(ctx).await?;
            if !matches!(outcome, HookOutcome::Continue) {
                return Ok(outcome);
            }
        }

        Ok(HookOutcome::Continue)
    }

    /// Run post-tool-call middleware until the first non-continue outcome.
    pub(crate) async fn after_tool_call(
        &self,
        ctx: &mut ToolResultContext<'_>,
    ) -> Result<HookOutcome> {
        for middleware in &self.middleware {
            let outcome = middleware.after_tool_call(ctx).await?;
            if !matches!(outcome, HookOutcome::Continue) {
                return Ok(outcome);
            }
        }

        Ok(HookOutcome::Continue)
    }

    /// Run suspension middleware in insertion order.
    pub(crate) async fn on_suspend(
        &self,
        ctx: &mut RunContext<'_>,
        suspension: &mut Suspension,
    ) -> Result<()> {
        for middleware in &self.middleware {
            middleware.on_suspend(ctx, suspension).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::{Result, ToolResult};

    use super::*;

    struct OrderingMiddleware {
        name: &'static str,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl AgentMiddleware for OrderingMiddleware {
        async fn build_system_prompt(
            &self,
            _ctx: &mut RequestContext<'_>,
            prompt: &mut SystemPromptParts,
        ) -> Result<HookOutcome> {
            self.calls
                .lock()
                .expect("call log poisoned")
                .push(self.name);
            prompt.push(self.name);
            Ok(HookOutcome::Continue)
        }
    }

    struct ProviderRequestMiddleware {
        name: &'static str,
        calls: Arc<Mutex<Vec<&'static str>>>,
        outcome: ProviderRequestOutcome,
    }

    struct ToolProducerMiddleware;

    impl AgentMiddleware for ToolProducerMiddleware {
        async fn list_tools(
            &self,
            _ctx: &mut RequestContext<'_>,
            tools: &mut ToolDraft,
        ) -> Result<()> {
            tools.push(crate::ToolDefinition::new(
                "load_skill",
                "middleware-owned tool",
                serde_json::json!({ "type": "object" }),
            ));
            Ok(())
        }
    }

    enum ProviderRequestOutcome {
        Continue,
        Replace,
    }

    impl AgentMiddleware for ProviderRequestMiddleware {
        async fn before_provider_request(
            &self,
            _ctx: &mut RequestContext<'_>,
            _request: &mut ModelRequestDraft,
        ) -> Result<HookOutcome> {
            self.calls
                .lock()
                .expect("call log poisoned")
                .push(self.name);
            Ok(match self.outcome {
                ProviderRequestOutcome::Continue => HookOutcome::Continue,
                ProviderRequestOutcome::Replace => {
                    HookOutcome::ReplaceToolResult(ToolResult::new("call-1", "test", "replace"))
                }
            })
        }
    }

    fn request_context<'a>(
        middleware_state: &'a mut crate::middleware::context::MiddlewareStateLog,
        system_prompt_override: &'a mut Option<crate::middleware::prompt::SystemPromptOverride>,
    ) -> RequestContext<'a> {
        RequestContext {
            middleware_state,
            session_metadata_values: std::collections::BTreeMap::new(),
            system_prompt_override,
            middleware_chain: MiddlewareChain::new(),
            permission_policy: crate::PermissionPolicy::default(),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            session_id: "test-session",
            turn: 0,
        }
    }

    #[tokio::test]
    async fn void_hooks_run_in_insertion_order() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain
            .push(OrderingMiddleware {
                name: "first",
                calls: Arc::clone(&calls),
            })
            .push(OrderingMiddleware {
                name: "second",
                calls: Arc::clone(&calls),
            });

        let mut middleware_state = crate::middleware::context::MiddlewareStateLog::default();
        let mut system_prompt_override = None;
        let mut ctx = request_context(&mut middleware_state, &mut system_prompt_override);
        let mut prompt = SystemPromptParts::default();

        chain
            .build_system_prompt(&mut ctx, &mut prompt)
            .await
            .expect("prompt middleware should run");

        assert_eq!(prompt.parts, vec!["first", "second"]);
        assert_eq!(
            *calls.lock().expect("call log poisoned"),
            vec!["first", "second"]
        );
    }

    #[tokio::test]
    async fn outcome_hooks_short_circuit_on_first_non_continue() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain
            .push(ProviderRequestMiddleware {
                name: "first",
                calls: Arc::clone(&calls),
                outcome: ProviderRequestOutcome::Continue,
            })
            .push(ProviderRequestMiddleware {
                name: "second",
                calls: Arc::clone(&calls),
                outcome: ProviderRequestOutcome::Replace,
            })
            .push(ProviderRequestMiddleware {
                name: "third",
                calls: Arc::clone(&calls),
                outcome: ProviderRequestOutcome::Continue,
            });

        let mut middleware_state = crate::middleware::context::MiddlewareStateLog::default();
        let mut system_prompt_override = None;
        let mut ctx = request_context(&mut middleware_state, &mut system_prompt_override);
        let mut request = ModelRequestDraft::default();

        let outcome = chain
            .before_provider_request(&mut ctx, &mut request)
            .await
            .expect("provider request middleware should run");

        assert!(matches!(outcome, HookOutcome::ReplaceToolResult(_)));
        assert_eq!(
            *calls.lock().expect("call log poisoned"),
            vec!["first", "second"]
        );
    }

    #[tokio::test]
    async fn tool_listing_filters_middleware_owned_tools() {
        let mut chain = MiddlewareChain::new();
        chain.push(ToolProducerMiddleware);
        let mut middleware_state = crate::middleware::context::MiddlewareStateLog::default();
        let mut system_prompt_override = None;
        let mut ctx = request_context(&mut middleware_state, &mut system_prompt_override);
        ctx.permission_policy = crate::PermissionPolicy::new(crate::PermissionLevel::Deny);
        let mut tools = ToolDraft::default();

        chain
            .list_tools(&mut ctx, &mut tools)
            .await
            .expect("tool listing should run");

        assert!(tools.tools.is_empty());
    }
}
