//! File prompt-template search and rendering.

use serde_json::json;
use std::path::Path;

use crate::agent_host::{
    Result,
    resource::{
        SessionTemplateKind, SessionTemplateOption, SessionTemplateSearchTarget, file_score,
        file_template,
    },
    template::{TemplateReference, TemplateReferenceRender, template_error},
};

use super::{
    AgentMiddleware,
    context::{TemplateRenderContext, TemplateSearchContext},
};

/// Middleware that owns file template autocomplete and file reference rendering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileTemplateMiddleware;

impl AgentMiddleware for FileTemplateMiddleware {
    async fn search_templates(
        &self,
        ctx: &TemplateSearchContext<'_>,
        results: &mut crate::agent_host::resource::TemplateSearchResults,
    ) -> Result<()> {
        if ctx.request.target != SessionTemplateSearchTarget::Resources
            || !ctx.request.includes(SessionTemplateKind::File)
            || ctx.sandbox.is_none()
        {
            return Ok(());
        }

        let query_lower = ctx.request.query.trim().to_lowercase();
        let limit = ctx.request.effective_limit();
        let workspace = ctx
            .sandbox
            .expect("checked above")
            .search_files(&query_lower, limit)
            .await?;
        for entry in workspace.entries {
            let path = Path::new(&entry);
            let relative = path.strip_prefix(ctx.root).unwrap_or(path);
            let detail = relative.to_string_lossy().replace('\\', "/");
            let label = relative
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(detail.as_str())
                .to_owned();
            let Some(score) = file_score(&label, &detail, &query_lower) else {
                continue;
            };
            results.push_limited(
                score,
                SessionTemplateOption {
                    kind: SessionTemplateKind::File,
                    label,
                    detail: detail.clone(),
                    template: file_template(&detail),
                    metadata: json!({ "path": detail }),
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
        let TemplateReference::File { path, .. } = reference else {
            return Ok(TemplateReferenceRender::Continue);
        };

        let dispatcher = ctx.middleware_chain.dispatcher()?.clone();
        let Some(tool) = dispatcher
            .agent_tools()
            .into_iter()
            .find(|tool| tool.definition().name == "read")
        else {
            return Ok(TemplateReferenceRender::Rendered(template_error(
                "file",
                "sandbox unavailable",
            )));
        };
        let target = ctx.base_dir.join(path);
        let call = crate::agent_host::ToolCall::new(
            ctx.tool_call_id
                .ok_or_else(|| crate::agent_host::LlmError::Protocol {
                    provider: "file-template",
                    message: "missing durable occurrence identity".into(),
                })?,
            "read",
            serde_json::json!({"filePath":target.to_string_lossy()}),
        );
        let mut result = ctx
            .middleware_state
            .execute_effect(tool, call.clone(), ctx.turn)
            .await?;
        let chain = ctx.middleware_chain;
        match chain
            .after_tool_call(&mut super::context::ToolResultContext {
                call: Some(&call),
                result: &mut result,
                middleware_state: ctx.middleware_state,
                middleware_chain: chain.clone(),
                permission_policy: ctx.permission_policy.clone(),
                cancellation_token: ctx.cancellation_token.clone(),
                session_id: ctx.session_id,
                turn: ctx.turn,
            })
            .await?
        {
            super::HookOutcome::Continue => {}
            super::HookOutcome::ReplaceToolResult(replacement) => result = replacement,
        }
        if !result.is_error {
            Ok(TemplateReferenceRender::TemplateResult {
                kind: "file".to_owned(),
                tool: Some("read".to_owned()),
                source: Some(path.clone()),
                content: result.content.to_string(),
                render_nested: true,
            })
        } else {
            Ok(TemplateReferenceRender::Rendered(template_error(
                &format!("file {path}"),
                &result.content,
            )))
        }
    }
}
