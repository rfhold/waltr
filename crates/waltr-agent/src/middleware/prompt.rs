//! Prompt-building middleware helpers.

use serde::{Deserialize, Serialize};

use crate::Result;

use super::{
    AgentMiddleware, HookOutcome,
    context::{MiddlewareStateAccess, RequestContext, SystemPromptParts},
};

/// Durable state namespace owned by [`SystemPromptMiddleware`].
const SYSTEM_PROMPT_STATE_NAMESPACE: &str = "system_prompt";

/// Captured system prompt persisted in the `"system_prompt"` namespace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SystemPromptState {
    prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<String>,
}

/// Per-run system prompt override consumed at the next provider request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemPromptOverride {
    replacement: Option<String>,
    fragments: Vec<String>,
}

impl SystemPromptOverride {
    /// Replace the effective system prompt; an empty prompt clears it.
    pub fn replace(&mut self, prompt: impl Into<String>) {
        let prompt = prompt.into();
        self.replacement = Some(prompt.trim().to_owned());
        self.fragments.clear();
    }

    /// Append one prompt fragment. Empty fragments are ignored.
    pub fn push_fragment(&mut self, prompt: impl Into<String>) {
        let prompt = prompt.into();
        let prompt = prompt.trim();
        if !prompt.is_empty() {
            self.fragments.push(prompt.to_owned());
        }
    }

    /// Return true when no replacement or fragments were set.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.replacement.is_none() && self.fragments.is_empty()
    }

    /// Compute the effective prompt from a base prompt.
    fn apply(&self, base: Option<String>) -> Option<String> {
        let mut prompt = match &self.replacement {
            Some(replacement) if replacement.is_empty() => None,
            Some(replacement) => Some(replacement.clone()),
            None => base,
        };
        for fragment in &self.fragments {
            prompt = Some(prompt.map_or_else(
                || fragment.clone(),
                |existing| format!("{existing}\n\n{fragment}"),
            ));
        }
        prompt
    }
}

/// Middleware that captures the configured system prompt into durable state and
/// renders from state on every later turn and resume, so stored state wins over
/// the configured value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemPromptMiddleware {
    configured: Option<String>,
    identity: Option<String>,
}

impl SystemPromptMiddleware {
    /// Create middleware from an optional configured system prompt.
    pub const fn new(configured: Option<String>) -> Self {
        Self {
            configured,
            identity: None,
        }
    }

    /// Attach the durable identity of the configured base prompt.
    #[must_use]
    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(identity.into());
        self
    }
}

impl AgentMiddleware for SystemPromptMiddleware {
    async fn build_system_prompt(
        &self,
        ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<HookOutcome> {
        let run_override = ctx.take_system_prompt_override();
        let state = if let Some(mut stored) =
            ctx.state_as::<SystemPromptState>(SYSTEM_PROMPT_STATE_NAMESPACE)?
        {
            let identity_changed = stored.identity.is_some() && stored.identity != self.identity;
            let identity_initialized = stored.identity.is_none() && self.identity.is_some();
            if identity_changed {
                stored = SystemPromptState {
                    prompt: self.configured.clone(),
                    identity: self.identity.clone(),
                };
            } else if identity_initialized {
                stored.identity.clone_from(&self.identity);
            }
            let effective =
                run_override.map(|run_override| run_override.apply(stored.prompt.clone()));
            match effective {
                Some(effective) if effective != stored.prompt => {
                    let state = SystemPromptState {
                        prompt: effective,
                        identity: stored.identity,
                    };
                    ctx.set_state_as(SYSTEM_PROMPT_STATE_NAMESPACE, &state)?;
                    state
                }
                _ if identity_changed || identity_initialized => {
                    ctx.set_state_as(SYSTEM_PROMPT_STATE_NAMESPACE, &stored)?;
                    stored
                }
                _ => stored,
            }
        } else {
            let state = SystemPromptState {
                prompt: run_override
                    .unwrap_or_default()
                    .apply(self.configured.clone()),
                identity: self.identity.clone(),
            };
            ctx.set_state_as(SYSTEM_PROMPT_STATE_NAMESPACE, &state)?;
            state
        };

        if let Some(stored) = state.prompt {
            prompt.push(stored);
        }
        Ok(HookOutcome::Continue)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::middleware::context::{MiddlewareStateLog, RequestContext};

    use super::*;

    fn request_context<'a>(
        middleware_state: &'a mut MiddlewareStateLog,
        system_prompt_override: &'a mut Option<SystemPromptOverride>,
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

    #[tokio::test]
    async fn captures_configured_prompt_into_state_on_first_turn() {
        let middleware = SystemPromptMiddleware::new(Some("configured prompt".to_owned()));
        let mut middleware_state = MiddlewareStateLog::default();
        let mut run_override = None;
        let mut ctx = request_context(&mut middleware_state, &mut run_override);
        let mut prompt = SystemPromptParts::default();

        middleware
            .build_system_prompt(&mut ctx, &mut prompt)
            .await
            .expect("system prompt middleware should run");

        assert_eq!(prompt.parts, vec!["configured prompt"]);
        assert_eq!(
            middleware_state.get("system_prompt"),
            Some(&json!({ "prompt": "configured prompt" }))
        );
    }

    #[tokio::test]
    async fn stored_state_wins_over_configured_prompt() {
        let middleware = SystemPromptMiddleware::new(Some("configured prompt".to_owned()));
        let mut middleware_state = MiddlewareStateLog::default();
        middleware_state.set("system_prompt", json!({ "prompt": "stored prompt" }));
        drop(middleware_state.drain_pending());
        let mut run_override = None;
        let mut ctx = request_context(&mut middleware_state, &mut run_override);
        let mut prompt = SystemPromptParts::default();

        middleware
            .build_system_prompt(&mut ctx, &mut prompt)
            .await
            .expect("system prompt middleware should run");

        assert_eq!(prompt.parts, vec!["stored prompt"]);
        assert!(
            middleware_state.drain_pending().is_empty(),
            "rendering from state must not journal a write"
        );
    }

    #[tokio::test]
    async fn run_override_folds_into_first_turn_capture() {
        let middleware = SystemPromptMiddleware::new(Some("configured prompt".to_owned()));
        let mut middleware_state = MiddlewareStateLog::default();
        let mut run_override = Some({
            let mut value = SystemPromptOverride::default();
            value.replace("override prompt");
            value.push_fragment("extra fragment");
            value
        });
        let mut ctx = request_context(&mut middleware_state, &mut run_override);
        let mut prompt = SystemPromptParts::default();

        middleware
            .build_system_prompt(&mut ctx, &mut prompt)
            .await
            .expect("system prompt middleware should run");

        assert_eq!(prompt.parts, vec!["override prompt\n\nextra fragment"]);
        assert_eq!(
            middleware_state.get("system_prompt"),
            Some(&json!({ "prompt": "override prompt\n\nextra fragment" }))
        );
        assert!(run_override.is_none(), "override must be consumed");
    }

    #[tokio::test]
    async fn run_fragment_appends_to_configured_prompt() {
        let middleware = SystemPromptMiddleware::new(Some("configured prompt".to_owned()));
        let mut middleware_state = MiddlewareStateLog::default();
        let mut run_override = Some({
            let mut value = SystemPromptOverride::default();
            value.push_fragment("run fragment");
            value
        });
        let mut ctx = request_context(&mut middleware_state, &mut run_override);
        let mut prompt = SystemPromptParts::default();

        middleware
            .build_system_prompt(&mut ctx, &mut prompt)
            .await
            .expect("system prompt middleware should run");

        assert_eq!(prompt.parts, vec!["configured prompt\n\nrun fragment"]);
    }

    #[tokio::test]
    async fn run_override_on_stored_state_journals_replacement() {
        let middleware = SystemPromptMiddleware::new(Some("configured prompt".to_owned()));
        let mut middleware_state = MiddlewareStateLog::default();
        middleware_state.set("system_prompt", json!({ "prompt": "stored prompt" }));
        drop(middleware_state.drain_pending());
        let mut run_override = Some({
            let mut value = SystemPromptOverride::default();
            value.replace("override prompt");
            value
        });
        let mut ctx = request_context(&mut middleware_state, &mut run_override);
        let mut prompt = SystemPromptParts::default();

        middleware
            .build_system_prompt(&mut ctx, &mut prompt)
            .await
            .expect("system prompt middleware should run");

        assert_eq!(prompt.parts, vec!["override prompt"]);
        assert_eq!(
            middleware_state.get("system_prompt"),
            Some(&json!({ "prompt": "override prompt" }))
        );
        assert_eq!(middleware_state.drain_pending().len(), 1);
    }

    #[tokio::test]
    async fn empty_replacement_clears_captured_prompt() {
        let middleware = SystemPromptMiddleware::new(Some("configured prompt".to_owned()));
        let mut middleware_state = MiddlewareStateLog::default();
        let mut run_override = Some({
            let mut value = SystemPromptOverride::default();
            value.replace("");
            value
        });
        let mut ctx = request_context(&mut middleware_state, &mut run_override);
        let mut prompt = SystemPromptParts::default();

        middleware
            .build_system_prompt(&mut ctx, &mut prompt)
            .await
            .expect("system prompt middleware should run");

        assert!(prompt.parts.is_empty());
        assert_eq!(
            middleware_state.get("system_prompt"),
            Some(&json!({ "prompt": null }))
        );
    }

    #[tokio::test]
    async fn changed_identity_replaces_only_the_materialized_base_prompt() {
        let middleware = SystemPromptMiddleware::new(Some("new prompt".to_owned()))
            .with_identity("definition-b");
        let mut middleware_state = MiddlewareStateLog::default();
        middleware_state.set(
            "system_prompt",
            json!({ "prompt": "old prompt", "identity": "definition-a" }),
        );
        middleware_state.set("other", json!({ "history": true }));
        drop(middleware_state.drain_pending());
        let mut run_override = None;
        let mut ctx = request_context(&mut middleware_state, &mut run_override);
        let mut prompt = SystemPromptParts::default();

        middleware
            .build_system_prompt(&mut ctx, &mut prompt)
            .await
            .unwrap();

        assert_eq!(prompt.parts, vec!["new prompt"]);
        assert_eq!(
            middleware_state.get("system_prompt"),
            Some(&json!({ "prompt": "new prompt", "identity": "definition-b" }))
        );
        assert_eq!(
            middleware_state.get("other"),
            Some(&json!({ "history": true }))
        );
        assert_eq!(middleware_state.drain_pending().len(), 1);
    }

    #[tokio::test]
    async fn unchanged_identity_retains_materialized_prompt_after_restart() {
        let middleware = SystemPromptMiddleware::new(Some("startup prompt changed".to_owned()))
            .with_identity("definition-a");
        let mut middleware_state = MiddlewareStateLog::default();
        middleware_state.set(
            "system_prompt",
            json!({ "prompt": "materialized prompt", "identity": "definition-a" }),
        );
        drop(middleware_state.drain_pending());
        let mut run_override = None;
        let mut ctx = request_context(&mut middleware_state, &mut run_override);
        let mut prompt = SystemPromptParts::default();

        middleware
            .build_system_prompt(&mut ctx, &mut prompt)
            .await
            .unwrap();

        assert_eq!(prompt.parts, vec!["materialized prompt"]);
        assert!(middleware_state.drain_pending().is_empty());
    }
}
