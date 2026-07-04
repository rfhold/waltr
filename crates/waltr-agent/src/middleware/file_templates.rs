//! File prompt-template search and rendering.

use ignore::WalkBuilder;
use serde_json::json;

use crate::{
    Result,
    resource::{
        SessionTemplateKind, SessionTemplateOption, SessionTemplateSearchTarget, file_score,
        file_template, normalize_path,
    },
    template::{TemplateReference, TemplateReferenceRender, template_error},
    tools::filesystem::{FilesystemConfig, ReadInput},
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
            || !ctx.root.is_dir()
        {
            return Ok(());
        }

        let query_lower = ctx.request.query.trim().to_lowercase();
        let limit = ctx.request.effective_limit();
        for entry in WalkBuilder::new(ctx.root)
            .standard_filters(true)
            .build()
            .filter_map(std::result::Result::ok)
        {
            let file_type = entry.file_type();
            if !file_type.is_some_and(|file_type| file_type.is_file()) {
                continue;
            }
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(ctx.root) else {
                continue;
            };
            let detail = normalize_path(relative);
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

        match FilesystemConfig::new(ctx.base_dir).read(&ReadInput::new(path.clone())) {
            Ok(content) => Ok(TemplateReferenceRender::TemplateResult {
                kind: "file".to_owned(),
                tool: Some("read".to_owned()),
                source: Some(path.clone()),
                content,
                render_nested: true,
            }),
            Err(error) => Ok(TemplateReferenceRender::Rendered(template_error(
                &format!("file {path}"),
                error,
            ))),
        }
    }
}
