//! Agent tool abstractions.

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use crate::{Result, ToolCall, ToolDefinition};

/// Host tool available to an agent loop.
#[derive(Clone)]
pub struct AgentTool {
    definition: ToolDefinition,
    handler: Arc<ToolHandler>,
}

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
type ToolHandler = dyn for<'a> Fn(&'a ToolCall) -> ToolFuture<'a> + Send + Sync;

impl AgentTool {
    /// Create a host tool from a definition and synchronous handler.
    pub fn new(
        definition: ToolDefinition,
        handler: impl Fn(&ToolCall) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Arc::new(move |call| Box::pin(std::future::ready(handler(call)))),
        }
    }

    /// Create a host tool from a definition and asynchronous handler.
    pub fn new_async(
        definition: ToolDefinition,
        handler: impl for<'a> Fn(&'a ToolCall) -> ToolFuture<'a> + Send + Sync + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Arc::new(handler),
        }
    }

    /// Return the provider-facing tool definition.
    #[must_use]
    pub const fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    pub(super) async fn call(&self, call: &ToolCall) -> Result<String> {
        (self.handler)(call).await
    }
}

impl fmt::Debug for AgentTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentTool")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}
