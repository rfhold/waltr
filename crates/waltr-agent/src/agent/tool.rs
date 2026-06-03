//! Agent tool abstractions.

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use crate::{PermissionClaim, Result, ToolCall, ToolDefinition};

use super::AgentToolContext;

/// Host tool available to an agent loop.
#[derive(Clone)]
pub struct AgentTool {
    definition: ToolDefinition,
    handler: Arc<ToolHandler>,
    claim_extractor: Arc<ClaimExtractor>,
}

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
type ToolHandler =
    dyn for<'a> Fn(&'a ToolCall, &'a mut AgentToolContext) -> ToolFuture<'a> + Send + Sync;
type ClaimExtractor = dyn Fn(&ToolCall) -> Result<Vec<PermissionClaim>> + Send + Sync;

impl AgentTool {
    /// Create a host tool from a definition and synchronous handler.
    pub fn new(
        definition: ToolDefinition,
        handler: impl Fn(&ToolCall) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Arc::new(move |call, _context| Box::pin(std::future::ready(handler(call)))),
            claim_extractor: Arc::new(|_| Ok(Vec::new())),
        }
    }

    /// Create a host tool from a definition and asynchronous handler.
    pub fn new_async(
        definition: ToolDefinition,
        handler: impl for<'a> Fn(&'a ToolCall) -> ToolFuture<'a> + Send + Sync + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Arc::new(move |call, _context| handler(call)),
            claim_extractor: Arc::new(|_| Ok(Vec::new())),
        }
    }

    /// Create a host tool with access to mutable per-run tool context.
    pub fn with_context(
        definition: ToolDefinition,
        handler: impl Fn(&ToolCall, &mut AgentToolContext) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Arc::new(move |call, context| {
                Box::pin(std::future::ready(handler(call, context)))
            }),
            claim_extractor: Arc::new(|_| Ok(Vec::new())),
        }
    }

    /// Create a host tool with asynchronous access to mutable per-run tool context.
    pub fn with_context_async(
        definition: ToolDefinition,
        handler: impl for<'a> Fn(&'a ToolCall, &'a mut AgentToolContext) -> ToolFuture<'a>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Arc::new(handler),
            claim_extractor: Arc::new(|_| Ok(Vec::new())),
        }
    }

    /// Add a contextual permission claim extractor.
    #[must_use]
    pub fn with_permission_claims(
        mut self,
        extractor: impl Fn(&ToolCall) -> Result<Vec<PermissionClaim>> + Send + Sync + 'static,
    ) -> Self {
        self.claim_extractor = Arc::new(extractor);
        self
    }

    /// Return the provider-facing tool definition.
    #[must_use]
    pub const fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    pub(super) async fn call(
        &self,
        call: &ToolCall,
        context: &mut AgentToolContext,
    ) -> Result<String> {
        (self.handler)(call, context).await
    }

    pub(super) fn claims(&self, call: &ToolCall) -> Result<Vec<PermissionClaim>> {
        (self.claim_extractor)(call)
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
