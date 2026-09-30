//! MCP resource data reader and user-inserted resource templates.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    LlmError, Result, ToolDefinition, ToolResult,
    mcp::McpClientRuntime,
    resource::{
        SessionTemplateKind, SessionTemplateOption, SessionTemplateSearchTarget,
        TemplateSearchResults, mcp_resource_template, tool_score,
    },
    template::{TemplateReference, TemplateReferenceRender, template_error},
};

use super::{
    AgentMiddleware, HookOutcome,
    context::{
        RequestContext, TemplateRenderContext, TemplateSearchContext, ToolCallContext, ToolDraft,
    },
    mcp_skills::McpSkillsMiddleware,
};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResourceSelector {
    pub server: String,
    pub uri: String,
}

pub(crate) fn resource_selector(ctx: &ToolCallContext<'_>) -> Result<ResourceSelector> {
    let selector: ResourceSelector =
        serde_json::from_value(ctx.call.arguments.clone()).map_err(|error| LlmError::Protocol {
            provider: "mcp-resources",
            message: format!("invalid read_mcp_resource selector: {error}"),
        })?;
    if selector.server.is_empty() || reqwest::Url::parse(&selector.uri).is_err() {
        return Err(LlmError::Protocol {
            provider: "mcp-resources",
            message: "invalid read_mcp_resource selector values".into(),
        });
    }
    Ok(selector)
}

#[derive(Debug, Clone)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct McpResourcesMiddleware {
    runtime: Arc<McpClientRuntime>,
    ordinary: bool,
}

impl McpResourcesMiddleware {
    pub(crate) const fn new(runtime: Arc<McpClientRuntime>) -> Self {
        Self {
            runtime,
            ordinary: true,
        }
    }

    pub(crate) const fn skill_references(runtime: Arc<McpClientRuntime>) -> Self {
        Self {
            runtime,
            ordinary: false,
        }
    }
}

impl AgentMiddleware for McpResourcesMiddleware {
    async fn list_tools(&self, _ctx: &mut RequestContext<'_>, tools: &mut ToolDraft) -> Result<()> {
        let description = if self.ordinary {
            "Read raw MCP resource DATA from the named server and direct URI, whether listed, template-derived, or unlisted. Does not activate skills or evaluate templates. Loaded skill files are automatically verified against every held manifest; unlisted paths beneath held roots are rejected."
        } else {
            "Read raw DATA for a file in a loaded, approved MCP skill's held manifest, using server and the exact file URI. Ordinary resource reads are disabled. Automatically verifies all matching held manifests. Does not activate skills or evaluate templates."
        };
        tools.push(ToolDefinition::new(
            "read_mcp_resource", description,
            json!({
                "type": "object",
                "properties": {"server": {"type": "string", "minLength": 1}, "uri": {"type": "string", "minLength": 1}},
                "required": ["server", "uri"],
                "additionalProperties": false
            }),
        ));
        Ok(())
    }

    async fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<HookOutcome> {
        if ctx.call.name != "read_mcp_resource" {
            return Ok(HookOutcome::Continue);
        }
        let ResourceSelector { server, uri } = resource_selector(ctx)?;
        let manifests = McpSkillsMiddleware::resource_manifests(ctx, &server, &uri)?;
        if !self.ordinary && manifests.is_empty() {
            return Err(LlmError::Protocol { provider: "mcp-resources", message: "ordinary MCP resource reads are disabled; only loaded held skill files may be read".into() });
        }
        let response = match self
            .runtime
            .read_resource_data(&server, &uri, ctx.cancellation_token.clone())
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if ctx.cancellation_token.is_cancelled() {
                    return Err(error);
                }
                McpSkillsMiddleware::revoke_resource(ctx, &server, &manifests)?;
                return Err(error);
            }
        };
        for manifest in &manifests {
            if let Err(error) =
                crate::mcp::skills::verified_resource_bytes(manifest, &uri, response.clone())
            {
                McpSkillsMiddleware::revoke_resource(ctx, &server, &manifests)?;
                return Err(error);
            }
        }
        let origins: Vec<_> = manifests
            .iter()
            .map(|manifest| json!({"server":server,"uri":manifest.uri}))
            .collect();
        Ok(HookOutcome::ReplaceToolResult(ToolResult::new(
            ctx.call.id.clone(),
            ctx.call.name.clone(),
            json!({"server":server,"uri":uri,"data":response.raw,"verified_skill_origins":origins})
                .to_string(),
        )))
    }

    #[allow(clippy::too_many_lines)]
    async fn search_templates(
        &self,
        ctx: &TemplateSearchContext<'_>,
        results: &mut TemplateSearchResults,
    ) -> Result<()> {
        if !self.ordinary
            || ctx.request.target != SessionTemplateSearchTarget::Resources
            || !ctx.request.includes(SessionTemplateKind::Resource)
        {
            return Ok(());
        }

        let query = ctx.request.query.trim().to_lowercase();
        if let Some(completion_context) =
            resource_completion_context(&ctx.request.query, ctx.request.cursor)
        {
            for server_resource_template in self.runtime.resource_templates().await {
                let server_name = server_resource_template.server_name;
                let resource_template = server_resource_template.resource_template;
                if server_name != completion_context.server {
                    continue;
                }
                let Some(argument_context) = resource_template_argument_context(
                    &resource_template.uri_template,
                    &completion_context.uri,
                    completion_context.uri_cursor,
                ) else {
                    continue;
                };
                let completion = self
                    .runtime
                    .complete_resource_template(
                        &server_name,
                        &resource_template.uri_template,
                        &argument_context.argument_name,
                        &argument_context.argument_value,
                        argument_context.context_arguments,
                        CancellationToken::new(),
                    )
                    .await;
                let Ok(Some(completion)) = completion else {
                    continue;
                };
                for value in completion.values {
                    let uri = format!(
                        "{}{}{}",
                        &completion_context.uri[..argument_context.value_start],
                        value,
                        &completion_context.uri[argument_context.value_end..]
                    );
                    let Some(template) = mcp_resource_template(&server_name, &uri) else {
                        continue;
                    };
                    results.push(
                        250,
                        SessionTemplateOption {
                            kind: SessionTemplateKind::Resource,
                            label: value.clone(),
                            detail: uri.clone(),
                            template,
                            metadata: json!({
                                "resourceType": "mcp_template_completion",
                                "server": server_name,
                                "uri": uri,
                                "uriTemplate": resource_template.uri_template,
                                "argument": argument_context.argument_name,
                                "value": value,
                                "total": completion.total,
                                "hasMore": completion.has_more,
                            }),
                        },
                    );
                }
            }
        }
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
        for server_resource_template in self.runtime.resource_templates().await {
            let server_name = server_resource_template.server_name;
            let resource_template = server_resource_template.resource_template;
            let label = format!("{server_name}:{}", resource_template.name);
            let detail = resource_template.description_text().to_owned();
            let searchable = format!("{label} {} {detail}", resource_template.uri_template);
            let Some(score) = tool_score(&label, &searchable, &query) else {
                continue;
            };
            let Some(template) =
                mcp_resource_template(&server_name, &resource_template.uri_template)
            else {
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
                        "resourceType": "mcp_template",
                        "server": server_name,
                        "uriTemplate": resource_template.uri_template,
                        "name": resource_template.name,
                        "title": resource_template.title,
                        "description": resource_template.description,
                        "mimeType": resource_template.mime_type,
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
        if !self.ordinary {
            return Ok(TemplateReferenceRender::Continue);
        }
        let TemplateReference::Resource { server, uri, .. } = reference else {
            return Ok(TemplateReferenceRender::Continue);
        };
        if !self.runtime.has_server(server) {
            return Ok(TemplateReferenceRender::Continue);
        }
        if !self.runtime.has_resource(server, uri).await
            && !self.runtime.has_resource_template_match(server, uri).await
        {
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceCompletionContext {
    server: String,
    uri: String,
    uri_cursor: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceTemplateArgumentContext {
    argument_name: String,
    argument_value: String,
    value_start: usize,
    value_end: usize,
    context_arguments: Map<String, Value>,
}

fn resource_completion_context(
    query: &str,
    cursor: Option<usize>,
) -> Option<ResourceCompletionContext> {
    let cursor = cursor?.min(query.len());
    if !query.is_char_boundary(cursor) {
        return None;
    }
    let prefix = "@resource(";
    if !query.starts_with(prefix) {
        return None;
    }
    let mut index = prefix.len();
    let server_start = index;
    while query[index..].chars().next().is_some_and(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
    }) {
        index += query[index..].chars().next()?.len_utf8();
    }
    if index == server_start {
        return None;
    }
    let server = query[server_start..index].to_owned();
    let rest = &query[index..];
    let comma = rest.find(',')?;
    index += comma + 1;
    while query[index..]
        .chars()
        .next()
        .is_some_and(char::is_whitespace)
    {
        index += query[index..].chars().next()?.len_utf8();
    }
    if query[index..].chars().next()? != '"' {
        return None;
    }
    let string_start = index + 1;
    let mut string_end = string_start;
    let mut escaped = false;
    for (offset, character) in query[string_start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if character == '"' {
            string_end = string_start + offset;
            break;
        }
    }
    if string_end < string_start || cursor < string_start || cursor > string_end {
        return None;
    }
    let raw_string = &query[index..=string_end];
    let uri: String = serde_json::from_str(raw_string).ok()?;
    let raw_before_cursor = &query[string_start..cursor];
    let uri_cursor = serde_json::from_str::<String>(&format!("\"{raw_before_cursor}\""))
        .ok()?
        .len();
    Some(ResourceCompletionContext {
        server,
        uri,
        uri_cursor,
    })
}

fn resource_template_argument_context(
    uri_template: &str,
    uri: &str,
    uri_cursor: usize,
) -> Option<ResourceTemplateArgumentContext> {
    let segments = template_segments(uri_template)?;
    let mut uri_index = 0;
    let mut context_arguments = Map::new();
    for (index, segment) in segments.iter().enumerate() {
        match segment {
            TemplateSegment::Literal(literal) => {
                if !uri[uri_index..].starts_with(literal) {
                    return None;
                }
                uri_index += literal.len();
            }
            TemplateSegment::Variable(name) => {
                let next_literal = segments[index + 1..]
                    .iter()
                    .find_map(|segment| match segment {
                        TemplateSegment::Literal(literal) if !literal.is_empty() => {
                            Some(literal.as_str())
                        }
                        _ => None,
                    });
                let value_end = if let Some(next_literal) = next_literal {
                    uri[uri_index..]
                        .find(next_literal)
                        .map(|offset| uri_index + offset)?
                } else {
                    uri.len()
                };
                if uri_cursor >= uri_index && uri_cursor <= value_end {
                    return Some(ResourceTemplateArgumentContext {
                        argument_name: name.clone(),
                        argument_value: uri[uri_index..uri_cursor].to_owned(),
                        value_start: uri_index,
                        value_end,
                        context_arguments,
                    });
                }
                context_arguments.insert(
                    name.clone(),
                    Value::String(uri[uri_index..value_end].to_owned()),
                );
                uri_index = value_end;
            }
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplateSegment {
    Literal(String),
    Variable(String),
}

fn template_segments(uri_template: &str) -> Option<Vec<TemplateSegment>> {
    let mut segments = Vec::new();
    let mut remaining = uri_template;
    while let Some(open) = remaining.find('{') {
        if open > 0 {
            segments.push(TemplateSegment::Literal(remaining[..open].to_owned()));
        }
        let close = remaining[open + 1..].find('}')?;
        let variable = &remaining[open + 1..open + 1 + close];
        if variable.is_empty()
            || variable
                .chars()
                .any(|character| matches!(character, ',' | '*' | ':' | '+'))
        {
            return None;
        }
        segments.push(TemplateSegment::Variable(variable.to_owned()));
        remaining = &remaining[open + close + 2..];
    }
    if !remaining.is_empty() {
        segments.push(TemplateSegment::Literal(remaining.to_owned()));
    }
    Some(segments)
}
