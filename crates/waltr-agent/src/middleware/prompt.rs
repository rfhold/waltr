//! Prompt-building middleware helpers.

use crate::{
    Result,
    prompt::{AgentsPromptConfig, BASE_SYSTEM_PROMPT, FilesystemPromptConfig},
};

#[cfg(feature = "shell")]
use crate::prompt::ShellPromptConfig;

use super::{AgentMiddleware, context::RequestContext, context::SystemPromptParts};

/// Middleware that appends the reusable base system prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BaseSystemPromptMiddleware;

impl AgentMiddleware for BaseSystemPromptMiddleware {
    async fn build_system_prompt(
        &self,
        _ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<()> {
        prompt.push(BASE_SYSTEM_PROMPT);
        Ok(())
    }
}

/// Middleware that appends a captured system prompt fragment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StaticSystemPromptMiddleware {
    system_prompt: Option<String>,
}

impl StaticSystemPromptMiddleware {
    /// Create middleware from an optional system prompt fragment.
    pub(crate) fn new(system_prompt: Option<String>) -> Self {
        Self { system_prompt }
    }
}

impl AgentMiddleware for StaticSystemPromptMiddleware {
    async fn build_system_prompt(
        &self,
        _ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<()> {
        if let Some(system_prompt) = &self.system_prompt {
            prompt.push(system_prompt.clone());
        }

        Ok(())
    }
}

/// Middleware that renders configured `AGENTS.md` instruction context per request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentsSystemPromptMiddleware {
    config: AgentsPromptConfig,
}

impl AgentsSystemPromptMiddleware {
    /// Create middleware from an `AGENTS.md` prompt configuration.
    pub(crate) fn new(config: AgentsPromptConfig) -> Self {
        Self { config }
    }
}

impl AgentMiddleware for AgentsSystemPromptMiddleware {
    async fn build_system_prompt(
        &self,
        _ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<()> {
        for instruction in self.config.instructions() {
            prompt.push(instruction.content);
        }

        Ok(())
    }
}

/// Middleware that renders filesystem prompt context per request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilesystemSystemPromptMiddleware {
    config: FilesystemPromptConfig,
}

impl FilesystemSystemPromptMiddleware {
    /// Create middleware from filesystem prompt configuration.
    pub(crate) fn new(config: FilesystemPromptConfig) -> Self {
        Self { config }
    }
}

impl AgentMiddleware for FilesystemSystemPromptMiddleware {
    async fn build_system_prompt(
        &self,
        _ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<()> {
        prompt.push(self.config.system_prompt());
        Ok(())
    }
}

/// Middleware that renders shell prompt context per request.
#[cfg(feature = "shell")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellSystemPromptMiddleware {
    config: ShellPromptConfig,
}

#[cfg(feature = "shell")]
impl ShellSystemPromptMiddleware {
    /// Create middleware from shell prompt configuration.
    pub(crate) fn new(config: ShellPromptConfig) -> Self {
        Self { config }
    }
}

#[cfg(feature = "shell")]
impl AgentMiddleware for ShellSystemPromptMiddleware {
    async fn build_system_prompt(
        &self,
        _ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<()> {
        prompt.push(self.config.system_prompt());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{AgentState, ModelId, middleware::context::RequestContext};

    use super::*;

    fn request_context<'a>(model_id: &'a ModelId, session: &'a AgentState) -> RequestContext<'a> {
        RequestContext {
            session_id: "session-1",
            revision: 0,
            model_id,
            provider_id: None,
            turn: 0,
            session,
        }
    }

    #[tokio::test]
    async fn appends_static_system_prompt_fragment() {
        let middleware = StaticSystemPromptMiddleware::new(Some("static prompt".to_string()));
        let model_id = ModelId::new("test-model");
        let session = AgentState::default();
        let mut ctx = request_context(&model_id, &session);
        let mut prompt = SystemPromptParts::default();
        prompt.push("existing prompt");

        middleware
            .build_system_prompt(&mut ctx, &mut prompt)
            .await
            .expect("static prompt middleware should run");

        assert_eq!(prompt.parts, vec!["existing prompt", "static prompt"]);
    }

    #[tokio::test]
    async fn appends_base_system_prompt_fragment() {
        let middleware = BaseSystemPromptMiddleware;
        let model_id = ModelId::new("test-model");
        let session = AgentState::default();
        let mut ctx = request_context(&model_id, &session);
        let mut prompt = SystemPromptParts::default();

        middleware
            .build_system_prompt(&mut ctx, &mut prompt)
            .await
            .expect("base prompt middleware should run");

        assert_eq!(prompt.parts, vec![BASE_SYSTEM_PROMPT]);
    }

    #[tokio::test]
    async fn skips_missing_static_system_prompt_fragment() {
        let middleware = StaticSystemPromptMiddleware::new(None);
        let model_id = ModelId::new("test-model");
        let session = AgentState::default();
        let mut ctx = request_context(&model_id, &session);
        let mut prompt = SystemPromptParts::default();
        prompt.push("existing prompt");

        middleware
            .build_system_prompt(&mut ctx, &mut prompt)
            .await
            .expect("static prompt middleware should run");

        assert_eq!(prompt.parts, vec!["existing prompt"]);
    }
}
