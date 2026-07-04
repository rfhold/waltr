//! MCP resource template middleware.

use std::sync::Arc;

use serde_json::json;

use crate::{
    Result,
    mcp::McpClientRuntime,
    resource::{
        SessionTemplateKind, SessionTemplateOption, SessionTemplateSearchTarget,
        TemplateSearchResults, mcp_resource_template, tool_score,
    },
    template::{TemplateReference, TemplateReferenceRender, template_error},
};

use super::{AgentMiddleware, context::TemplateRenderContext, context::TemplateSearchContext};

#[derive(Debug, Clone)]
pub(crate) struct McpResourcesMiddleware {
    runtime: Arc<McpClientRuntime>,
}

impl McpResourcesMiddleware {
    pub(crate) fn new(runtime: Arc<McpClientRuntime>) -> Self {
        Self { runtime }
    }
}

impl AgentMiddleware for McpResourcesMiddleware {
    async fn search_templates(
        &self,
        ctx: &TemplateSearchContext<'_>,
        results: &mut TemplateSearchResults,
    ) -> Result<()> {
        if ctx.request.target != SessionTemplateSearchTarget::Resources
            || !ctx.request.includes(SessionTemplateKind::Resource)
        {
            return Ok(());
        }

        let query = ctx.request.query.trim().to_lowercase();
        for server_resource in self.runtime.resources().await {
            let server_name = server_resource.server_name;
            let resource = server_resource.resource;
            let label = format!("{server_name}:{}", resource.name);
            let detail = resource.description_text().to_owned();
            let searchable = format!("{label} {} {detail}", resource.uri);
            let Some(score) = tool_score(&label, &searchable, &query) else {
                continue;
            };
            let Some(template) = mcp_resource_template(&server_name, &resource.uri) else {
                continue;
            };
            results.push(
                score,
                SessionTemplateOption {
                    kind: SessionTemplateKind::Resource,
                    label,
                    detail,
                    template,
                    metadata: json!({
                        "resourceType": "mcp",
                        "server": server_name,
                        "uri": resource.uri,
                        "name": resource.name,
                        "title": resource.title,
                        "description": resource.description,
                        "mimeType": resource.mime_type,
                        "size": resource.size,
                    }),
                },
            );
        }
        Ok(())
    }

    async fn render_template_reference(
        &self,
        ctx: &mut TemplateRenderContext<'_>,
        reference: &TemplateReference,
    ) -> Result<TemplateReferenceRender> {
        let TemplateReference::Resource { server, uri, .. } = reference else {
            return Ok(TemplateReferenceRender::Continue);
        };
        if !self.runtime.has_server(server) {
            return Ok(TemplateReferenceRender::Continue);
        }
        if !self.runtime.has_resource(server, uri).await {
            return Ok(TemplateReferenceRender::Continue);
        }

        match self
            .runtime
            .read_resource(server, uri, ctx.cancellation_token.clone())
            .await
        {
            Ok(content) => Ok(TemplateReferenceRender::TemplateResult {
                kind: "mcp_resource".to_owned(),
                tool: None,
                source: Some(format!("{server}:{uri}")),
                content,
                render_nested: true,
            }),
            Err(error) => Ok(TemplateReferenceRender::Rendered(template_error(
                &format!("mcp resource {server}:{uri}"),
                error,
            ))),
        }
    }
}
