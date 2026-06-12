//! Deterministic middleware chain execution.

use std::{future::Future, pin::Pin, sync::Arc};

use crate::Result;

use super::{
    AgentMiddleware, HookOutcome, ModelEvent, ResumeInput, Suspension,
    context::{
        ModelRequestDraft, RequestContext, RunContext, StreamContext, SystemPromptParts,
        ToolCallContext, ToolDraft, ToolResultContext,
    },
};

type BoxHookFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

trait ChainMiddleware: Send + Sync {
    fn build_system_prompt<'a>(
        &'a self,
        ctx: &'a mut RequestContext<'_>,
        prompt: &'a mut SystemPromptParts,
    ) -> BoxHookFuture<'a, ()>;

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

    fn on_provider_event<'a>(
        &'a self,
        ctx: &'a mut StreamContext<'_>,
        event: &'a mut ModelEvent,
    ) -> BoxHookFuture<'a, HookOutcome>;

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

    fn on_resume<'a>(
        &'a self,
        ctx: &'a mut RunContext<'_>,
        input: &'a mut ResumeInput,
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
    ) -> BoxHookFuture<'a, ()> {
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

    fn on_provider_event<'a>(
        &'a self,
        ctx: &'a mut StreamContext<'_>,
        event: &'a mut ModelEvent,
    ) -> BoxHookFuture<'a, HookOutcome> {
        Box::pin(AgentMiddleware::on_provider_event(self, ctx, event))
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

    fn on_resume<'a>(
        &'a self,
        ctx: &'a mut RunContext<'_>,
        input: &'a mut ResumeInput,
    ) -> BoxHookFuture<'a, ()> {
        Box::pin(AgentMiddleware::on_resume(self, ctx, input))
    }
}

/// Ordered middleware collection for deterministic hook execution.
#[derive(Clone, Default)]
pub(crate) struct MiddlewareChain {
    middleware: Vec<Arc<dyn ChainMiddleware>>,
}

impl MiddlewareChain {
    /// Create an empty middleware chain.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Return true when the chain contains no middleware.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.middleware.is_empty()
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

    /// Add shared middleware to the end of the chain.
    pub(crate) fn push_arc<M>(&mut self, middleware: Arc<M>) -> &mut Self
    where
        M: AgentMiddleware + 'static,
    {
        self.middleware.push(middleware);
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
    ) -> Result<()> {
        for middleware in &self.middleware {
            middleware.build_system_prompt(ctx, prompt).await?;
        }

        Ok(())
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

    /// Run provider event middleware until the first non-continue outcome.
    pub(crate) async fn on_provider_event(
        &self,
        ctx: &mut StreamContext<'_>,
        event: &mut ModelEvent,
    ) -> Result<HookOutcome> {
        for middleware in &self.middleware {
            let outcome = middleware.on_provider_event(ctx, event).await?;
            if !matches!(outcome, HookOutcome::Continue) {
                return Ok(outcome);
            }
        }

        Ok(HookOutcome::Continue)
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

    /// Run resume middleware in insertion order.
    pub(crate) async fn on_resume(
        &self,
        ctx: &mut RunContext<'_>,
        input: &mut ResumeInput,
    ) -> Result<()> {
        for middleware in &self.middleware {
            middleware.on_resume(ctx, input).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::{AgentState, ModelId, Result};

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
        ) -> Result<()> {
            self.calls
                .lock()
                .expect("call log poisoned")
                .push(self.name);
            prompt.push(self.name);
            Ok(())
        }
    }

    struct ProviderRequestMiddleware {
        name: &'static str,
        calls: Arc<Mutex<Vec<&'static str>>>,
        outcome: ProviderRequestOutcome,
    }

    enum ProviderRequestOutcome {
        Continue,
        Suppress,
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
                ProviderRequestOutcome::Suppress => HookOutcome::Suppress,
            })
        }
    }

    fn request_context<'a>(
        model_id: &'a ModelId,
        session: &'a AgentState,
        middleware_state: &'a mut crate::middleware::context::MiddlewareStateLog,
        system_prompt_override: &'a mut Option<crate::middleware::prompt::SystemPromptOverride>,
    ) -> RequestContext<'a> {
        RequestContext {
            session_id: "session-1",
            revision: 0,
            model_id,
            provider_id: None,
            turn: 0,
            session,
            middleware_state,
            system_prompt_override,
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

        let model_id = ModelId::new("test-model");
        let session = AgentState::default();
        let mut middleware_state = crate::middleware::context::MiddlewareStateLog::default();
        let mut system_prompt_override = None;
        let mut ctx = request_context(
            &model_id,
            &session,
            &mut middleware_state,
            &mut system_prompt_override,
        );
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
                outcome: ProviderRequestOutcome::Suppress,
            })
            .push(ProviderRequestMiddleware {
                name: "third",
                calls: Arc::clone(&calls),
                outcome: ProviderRequestOutcome::Continue,
            });

        let model_id = ModelId::new("test-model");
        let session = AgentState::default();
        let mut middleware_state = crate::middleware::context::MiddlewareStateLog::default();
        let mut system_prompt_override = None;
        let mut ctx = request_context(
            &model_id,
            &session,
            &mut middleware_state,
            &mut system_prompt_override,
        );
        let mut request = ModelRequestDraft::default();

        let outcome = chain
            .before_provider_request(&mut ctx, &mut request)
            .await
            .expect("provider request middleware should run");

        assert!(matches!(outcome, HookOutcome::Suppress));
        assert_eq!(
            *calls.lock().expect("call log poisoned"),
            vec!["first", "second"]
        );
    }
}
