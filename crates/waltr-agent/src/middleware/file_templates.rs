//! File prompt-template search and rendering.

use serde_json::json;
use std::path::Path;

use crate::{
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
        results: &mut crate::resource::TemplateSearchResults,
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

        let Some(sandbox) = ctx.sandbox else {
            return Ok(TemplateReferenceRender::Rendered(template_error(
                "file",
                "sandbox unavailable",
            )));
        };
        let target = ctx.base_dir.join(path);
        match sandbox.formatted_read(&target.to_string_lossy()).await {
            Ok(data) => Ok(TemplateReferenceRender::TemplateResult {
                kind: "file".to_owned(),
                tool: Some("read".to_owned()),
                source: Some(path.clone()),
                content: data.content,
                render_nested: true,
            }),
            Err(error) => Ok(TemplateReferenceRender::Rendered(template_error(
                &format!("file {path}"),
                error,
            ))),
        }
    }
}
