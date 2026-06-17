//! Permission claim middleware.
//!
//! Claims producers append `PermissionClaim`s to `ToolCallContext.permission_claims`
//! during `before_tool_call`. They must run before permission evaluation; the agent
//! loop evaluates accumulated claims after the full `before_tool_call` chain.

use crate::{
    ModelRef, Result,
    tools::filesystem::{FilesystemConfig, FilesystemToolKind},
};

#[cfg(feature = "shell")]
use crate::tools::shell::{SHELL_TOOL_NAME, ShellConfig, ShellInput};

use super::{AgentMiddleware, HookOutcome, context::ToolCallContext};

/// Middleware that derives filesystem path claims from tool call arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemClaimsMiddleware {
    config: FilesystemConfig,
    kinds: Vec<FilesystemToolKind>,
}

impl FilesystemClaimsMiddleware {
    /// Create middleware covering the filesystem tools exposed for a model.
    pub fn new(config: FilesystemConfig, model: &ModelRef) -> Self {
        let kinds = config.exposed_tools(model).all();
        Self { config, kinds }
    }
}

impl AgentMiddleware for FilesystemClaimsMiddleware {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<HookOutcome> {
        let Some(kind) = self
            .kinds
            .iter()
            .copied()
            .find(|kind| kind.name() == ctx.call.name)
        else {
            return Ok(HookOutcome::Continue);
        };
        let claims = self.config.permission_claims(kind, ctx.call)?;
        ctx.permission_claims.extend(claims);
        Ok(HookOutcome::Continue)
    }
}

/// Middleware that derives shell command and path claims from tool call arguments.
#[cfg(feature = "shell")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellClaimsMiddleware {
    config: ShellConfig,
}

#[cfg(feature = "shell")]
impl ShellClaimsMiddleware {
    /// Create middleware for the `shell` tool.
    pub const fn new(config: ShellConfig) -> Self {
        Self { config }
    }
}

#[cfg(feature = "shell")]
impl AgentMiddleware for ShellClaimsMiddleware {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<HookOutcome> {
        if ctx.call.name != SHELL_TOOL_NAME {
            return Ok(HookOutcome::Continue);
        }
        let input: ShellInput = serde_json::from_value(ctx.call.arguments.clone())?;
        ctx.permission_claims
            .extend(self.config.scan(&input)?.claims);
        Ok(HookOutcome::Continue)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use serde_json::json;

    use crate::{
        ModelApi, PermissionClaim, PermissionResource, ProviderId, ToolCall,
        middleware::{chain::MiddlewareChain, context::MiddlewareStateLog},
    };

    use super::*;

    struct ClaimObserverMiddleware {
        observed: Arc<Mutex<Vec<PermissionClaim>>>,
    }

    impl AgentMiddleware for ClaimObserverMiddleware {
        async fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<HookOutcome> {
            self.observed
                .lock()
                .expect("observed claims lock")
                .clone_from(ctx.permission_claims);
            Ok(HookOutcome::Continue)
        }
    }

    #[tokio::test]
    async fn later_middleware_observes_claims_from_earlier_claims_producer() {
        let root = temp_root("claims-order");
        fs::write(root.join("notes.txt"), "hello\n").expect("write fixture file");
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain
            .push(FilesystemClaimsMiddleware::new(
                FilesystemConfig::new(&root),
                &model,
            ))
            .push(ClaimObserverMiddleware {
                observed: Arc::clone(&observed),
            });

        let mut call = ToolCall::new("call-1", "read", json!({ "filePath": "notes.txt" }));
        let mut claims = Vec::new();
        let mut middleware_state = MiddlewareStateLog::default();
        let mut ctx = ToolCallContext {
            session_id: "session-1",
            turn: 0,
            call: &mut call,
            permission_claims: &mut claims,
            middleware_state: &mut middleware_state,
        };

        let outcome = chain
            .before_tool_call(&mut ctx)
            .await
            .expect("claims chain should run");

        assert!(matches!(outcome, HookOutcome::Continue));
        let observed = observed.lock().expect("observed claims lock").clone();
        let expected = root
            .canonicalize()
            .expect("canonical root")
            .join("notes.txt");
        assert!(
            observed.iter().any(|claim| matches!(
                &claim.resource,
                PermissionResource::FilesystemPath(path) if path == &expected
            )),
            "{observed:#?}"
        );
        assert_eq!(observed, claims);
        drop(fs::remove_dir_all(root));
    }

    fn temp_root(label: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "waltr-agent-claims-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("create temp root");
        path
    }
}
