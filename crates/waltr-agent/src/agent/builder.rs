//! Provider-neutral runtime construction.
use super::{AgentRuntime, AgentTool, ProviderRetryConfig};
use crate::middleware::{chain::MiddlewareChain, prompt::SystemPromptMiddleware};
use crate::{LlmStream, ModelCost, ModelRef, SystemPromptBuilder};
use std::{fmt, sync::Arc};

#[derive(Clone)]
pub struct AgentRuntimeBuilder {
    provider: Arc<dyn LlmStream>,
    model: ModelRef,
    model_cost: Option<ModelCost>,
    tools: Vec<AgentTool>,
    middleware_chain: MiddlewareChain,
    system_prompt: SystemPromptBuilder,
    system_prompt_identity: Option<String>,
    retry_config: ProviderRetryConfig,
    max_provider_turns: Option<u64>,
    host_context: Option<Arc<dyn super::AgentHostContext>>,
}
impl AgentRuntimeBuilder {
    pub fn new<P: LlmStream + 'static>(model: ModelRef, provider: Arc<P>) -> Self {
        Self::new_dyn(model, provider)
    }
    pub fn new_dyn(model: ModelRef, provider: Arc<dyn LlmStream>) -> Self {
        Self {
            provider,
            model,
            model_cost: None,
            tools: Vec::new(),
            middleware_chain: MiddlewareChain::new(),
            system_prompt: SystemPromptBuilder::new(),
            system_prompt_identity: None,
            retry_config: ProviderRetryConfig::default(),
            max_provider_turns: None,
            host_context: None,
        }
    }
    pub fn model(&self) -> &ModelRef {
        &self.model
    }
    pub fn with_model_cost(mut self, cost: ModelCost) -> Self {
        self.model_cost = Some(cost);
        self
    }
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = self.system_prompt.with_fragment(prompt);
        self
    }
    pub fn with_system_prompt_identity(mut self, identity: impl Into<String>) -> Self {
        self.system_prompt_identity = Some(identity.into());
        self
    }
    pub fn with_system_date(mut self, date: impl Into<String>) -> Self {
        self.system_prompt = self.system_prompt.with_date(date);
        self
    }
    pub fn with_tool(mut self, tool: AgentTool) -> Self {
        self.tools.push(tool);
        self
    }
    pub fn with_host_context(mut self, context: Arc<dyn super::AgentHostContext>) -> Self {
        self.host_context = Some(context);
        self
    }
    #[cfg(test)]
    pub(crate) fn with_middleware<M: crate::middleware::AgentMiddleware + 'static>(
        mut self,
        middleware: M,
    ) -> Self {
        self.middleware_chain.push(middleware);
        self
    }
    #[cfg(test)]
    pub(crate) fn with_middleware_chain(mut self, chain: MiddlewareChain) -> Self {
        self.middleware_chain = chain;
        self
    }
    pub const fn with_provider_retry_config(mut self, config: ProviderRetryConfig) -> Self {
        self.retry_config = config;
        self
    }
    pub const fn with_max_provider_turns(mut self, limit: u64) -> Self {
        self.max_provider_turns = Some(limit);
        self
    }
    pub fn build(self) -> AgentRuntime {
        let mut chain = MiddlewareChain::new();
        let mut prompt = SystemPromptMiddleware::new(self.system_prompt.build());
        if let Some(identity) = self.system_prompt_identity {
            prompt = prompt.with_identity(identity);
        }
        chain.push(prompt);
        chain.push(super::host::HostToolRegistry(self.tools.clone()));
        chain.extend(self.middleware_chain);
        if let Some(context) = &self.host_context {
            chain.push(super::host::HostContextMiddleware(context.clone()));
        }
        AgentRuntime {
            completion_validation: None,
            provider: self.provider,
            model: self.model,
            model_cost: self.model_cost,
            tools: self.tools,
            middleware_chain: chain,
            retry_config: self.retry_config,
            max_provider_turns: self.max_provider_turns,
            host_context: self.host_context,
        }
    }
}
impl fmt::Debug for AgentRuntimeBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentRuntimeBuilder")
            .field("model", &self.model)
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}
