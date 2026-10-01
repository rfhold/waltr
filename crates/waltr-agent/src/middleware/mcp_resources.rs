//! MCP resource data reader and user-inserted resource templates.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    LlmError, PermissionLevel, Result, ToolDefinition, ToolResult,
    mcp::McpClientRuntime,
    permission::{PERMISSION_STATE_NAMESPACE, PermissionRuntimeState},
    resource::{
        SessionTemplateKind, SessionTemplateOption, SessionTemplateSearchTarget,
        TemplateSearchResults, mcp_resource_template, tool_score,
    },
    template::{
        TemplateReference, TemplateReferenceRender, template_error, template_read_permission_error,
    },
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
        if let Some(outcome) = resource_input_permission(ctx)? {
            return Ok(outcome);
        }
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
        if reqwest::Url::parse(uri).is_err() {
            return Ok(TemplateReferenceRender::Rendered(template_error(
                &format!("mcp resource {server}:{uri}"),
                "invalid read_mcp_resource selector values",
            )));
        }
        if !self.runtime.has_resource(server, uri).await
            && !self.runtime.has_resource_template_match(server, uri).await
        {
            return Ok(TemplateReferenceRender::Continue);
        }

        if let Some(error) = template_read_permission_error(
            ctx,
            "read_mcp_resource",
            &json!({"server": server, "uri": uri}),
        )? {
            return Ok(TemplateReferenceRender::Rendered(template_error(
                &format!("mcp resource {server}:{uri}"),
                error,
            )));
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

fn resource_input_permission(ctx: &ToolCallContext<'_>) -> Result<Option<HookOutcome>> {
    if ctx.call.name != ctx.approved_tool_name {
        return Err(LlmError::Protocol {
            provider: "permission",
            message: "middleware changed the approved tool name".into(),
        });
    }
    if ctx.permission_policy.evaluate_tool(&ctx.call.name) == PermissionLevel::Deny {
        return Err(LlmError::Protocol {
            provider: "permission",
            message: format!("tool is not permitted: {}", ctx.call.name),
        });
    }
    let Some(input) = ctx
        .permission_policy
        .evaluate_input(&ctx.call.name, &ctx.call.arguments)
    else {
        return Ok(None);
    };
    match input.effect.level {
        PermissionLevel::Allow => return Ok(None),
        PermissionLevel::Deny => {
            return Err(LlmError::Protocol {
                provider: "permission",
                message: "permission denied: tool input does not satisfy policy".into(),
            });
        }
        PermissionLevel::Ask => {}
    }
    let state: PermissionRuntimeState = ctx
        .middleware_state
        .get(PERMISSION_STATE_NAMESPACE)
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .map_err(|_| LlmError::Protocol {
            provider: "permission",
            message: "invalid permission runtime state".into(),
        })?
        .unwrap_or_default();
    let unmatched = input
        .claims
        .into_iter()
        .filter(|claim| !state.patterns.iter().any(|pattern| pattern.matches(claim)))
        .collect::<Vec<_>>();
    if unmatched.is_empty() || (ctx.contextual_input_is_approved)(ctx, &unmatched, &state) {
        Ok(None)
    } else {
        Ok(Some(HookOutcome::Suspend((ctx.contextual_suspension)(
            ctx, &unmatched,
        ))))
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

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Mutex};

    use mcp::{
        McpResourceList, McpResourceResult, McpResourceTemplateList,
        server::{
            BoxFuture, ServerCapabilities, ServerContext, ServerHandler, ServerInfo, ServerResult,
        },
    };

    use super::*;
    use crate::{
        PermissionEffect, PermissionInputCondition, PermissionInputRule, PermissionInputScalar,
        PermissionInputValueMatcher, PermissionJsonPointer, PermissionOperation, PermissionPattern,
        PermissionPolicy, PermissionResourcePattern, PermissionToolSelector,
        mcp::{McpConfig, McpServerConfig},
        middleware::{chain::MiddlewareChain, context::MiddlewareStateLog},
        permission::{
            PERMISSION_STATE_NAMESPACE, PermissionApprovalStage, PermissionRuntimeState,
            permission_claim_identities,
        },
        template::{TemplateOrigin, TemplateRender, TemplateRunContext, TemplateRuntime},
    };

    #[derive(Default)]
    struct ResourceServer {
        reads: Mutex<Vec<String>>,
    }

    impl ServerHandler for ResourceServer {
        fn server_info(&self) -> ServerInfo {
            ServerInfo::new("template-permissions", "1")
        }

        fn capabilities(&self) -> ServerCapabilities {
            ServerCapabilities::new().resources()
        }

        fn list_resources(
            &self,
            _cursor: Option<String>,
            _context: ServerContext,
        ) -> BoxFuture<ServerResult<McpResourceList>> {
            Box::pin(async {
                Ok(McpResourceList {
                    resources: ["waltr://resources", "waltr://sessions", "invalid-uri"]
                        .into_iter()
                        .map(|uri| mcp::McpResourceDefinition {
                            uri: uri.into(),
                            name: uri.into(),
                            title: None,
                            description: None,
                            mime_type: None,
                            icons: Vec::new(),
                            size: None,
                            annotations: None,
                            meta: None,
                        })
                        .collect(),
                    next_cursor: None,
                })
            })
        }

        fn list_resource_templates(
            &self,
            _cursor: Option<String>,
            _context: ServerContext,
        ) -> BoxFuture<ServerResult<McpResourceTemplateList>> {
            Box::pin(async {
                Ok(McpResourceTemplateList {
                    resource_templates: [
                        "waltr://{kind}",
                        "waltr://resources/{path}/source",
                        "waltr://sessions/{id}",
                    ]
                    .into_iter()
                    .map(|uri| mcp::McpResourceTemplateDefinition {
                        uri_template: uri.into(),
                        name: uri.into(),
                        title: None,
                        description: None,
                        mime_type: None,
                        icons: Vec::new(),
                        annotations: None,
                        meta: None,
                    })
                    .collect(),
                    next_cursor: None,
                })
            })
        }

        fn read_resource(
            &self,
            uri: String,
            _context: ServerContext,
        ) -> BoxFuture<ServerResult<McpResourceResult>> {
            self.reads.lock().unwrap().push(uri.clone());
            let text = if uri.ends_with("nested/source") {
                "@resource(waltr, \"waltr://sessions\")".to_owned()
            } else {
                format!("resource data for {uri}")
            };
            Box::pin(async move { Ok(McpResourceResult::text(&uri, "text/plain", text)) })
        }
    }

    struct Fixture {
        handler: Arc<ResourceServer>,
        runtime: Arc<McpClientRuntime>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        async fn new() -> Self {
            let handler = Arc::new(ResourceServer::default());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
            let router = mcp::server::streamable_http_router(handler.clone());
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let config = McpConfig::new()
                .with_server(McpServerConfig::streamable_http("waltr", &endpoint))
                .with_server(McpServerConfig::streamable_http("other", &endpoint));
            let runtime =
                McpClientRuntime::start(config, "file:///test".into(), CancellationToken::new())
                    .await
                    .unwrap();
            Self {
                handler,
                runtime,
                server,
            }
        }

        async fn render(
            &self,
            policy: &PermissionPolicy,
            state: &mut MiddlewareStateLog,
            ordinary: bool,
            prompt: &str,
        ) -> String {
            let mut chain = MiddlewareChain::new();
            chain.push(if ordinary {
                McpResourcesMiddleware::new(self.runtime.clone())
            } else {
                McpResourcesMiddleware::skill_references(self.runtime.clone())
            });
            let mut renderer = TemplateRuntime::new(
                Path::new("/test"),
                state,
                TemplateRunContext {
                    middleware_chain: &chain,
                    permission_policy: policy,
                    cancellation_token: CancellationToken::new(),
                    session_id: "session",
                    turn: 0,
                    sandbox: None,
                },
                TemplateOrigin::UserPrompt {
                    submission: 0,
                    prompt_index: 0,
                },
            );
            let TemplateRender::Rendered(content) = renderer.render_template(prompt).await.unwrap()
            else {
                panic!("resource references do not suspend")
            };
            assert!(renderer.rendered_tool_ids().is_empty());
            content
        }

        async fn render_reader_occurrence(
            &self,
            policy: &PermissionPolicy,
            state: &mut MiddlewareStateLog,
            arguments: Value,
        ) -> TemplateReferenceRender {
            let mut chain = MiddlewareChain::new();
            chain.push(McpResourcesMiddleware::new(self.runtime.clone()));
            let tools = [crate::AgentTool::new(
                ToolDefinition::new("read_mcp_resource", "reader", json!({})),
                |_| panic!("resource middleware must intercept the reader"),
            )];
            let mut ctx = TemplateRenderContext {
                base_dir: Path::new("/test"),
                middleware_state: state,
                middleware_chain: &chain,
                permission_policy: policy,
                cancellation_token: CancellationToken::new(),
                session_id: "session",
                turn: 0,
                sandbox: None,
                tool_call_id: Some("same-reader-occurrence"),
            };
            crate::template::render_tool_template_reference(
                &mut ctx,
                &tools,
                "read_mcp_resource",
                arguments,
            )
            .await
            .unwrap()
        }

        async fn render_tool(
            &self,
            policy: &PermissionPolicy,
            state: &mut MiddlewareStateLog,
            uri: &str,
        ) -> TemplateRender {
            let mut chain = MiddlewareChain::new();
            chain.push(McpResourcesMiddleware::new(self.runtime.clone()));
            chain.push(
                crate::middleware::tools::PermissionFilteredToolRegistryMiddleware::new(
                    vec![crate::AgentTool::new(
                        ToolDefinition::new("read_mcp_resource", "reader", json!({})),
                        |_| panic!("resource middleware must intercept the reader"),
                    )],
                    policy.clone(),
                    true,
                ),
            );
            let mut renderer = TemplateRuntime::new(
                Path::new("/test"),
                state,
                TemplateRunContext {
                    middleware_chain: &chain,
                    permission_policy: policy,
                    cancellation_token: CancellationToken::new(),
                    session_id: "session",
                    turn: 0,
                    sandbox: None,
                },
                TemplateOrigin::UserPrompt {
                    submission: 0,
                    prompt_index: 0,
                },
            );
            renderer
                .render_template(&format!(
                    "@tool(read_mcp_resource, {})",
                    json!({"server":"waltr","uri":uri})
                ))
                .await
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    fn condition(pointer: &str, value: PermissionInputValueMatcher) -> PermissionInputCondition {
        PermissionInputCondition {
            pointer: PermissionJsonPointer::parse(pointer).unwrap(),
            value,
        }
    }

    fn exact(value: &str) -> PermissionInputValueMatcher {
        PermissionInputValueMatcher::Exact(PermissionInputScalar::String(value.into()))
    }

    fn resource_only_policy() -> PermissionPolicy {
        let selector = PermissionToolSelector::exact("read_mcp_resource");
        let mut policy =
            PermissionPolicy::default().with_tool_rule(selector.clone(), PermissionEffect::allow());
        for uri in [
            exact("waltr://resources"),
            PermissionInputValueMatcher::WildcardPrefix("waltr://resources/".into()),
        ] {
            policy = policy.with_input_rule(
                selector.clone(),
                PermissionInputRule::new(
                    vec![condition("/server", exact("waltr")), condition("/uri", uri)],
                    PermissionEffect::allow(),
                )
                .unwrap(),
            );
        }
        policy.with_input_rule(
            selector,
            PermissionInputRule::new(
                vec![condition(
                    "/uri",
                    PermissionInputValueMatcher::WildcardPrefix(String::new()),
                )],
                PermissionEffect::deny(),
            )
            .unwrap(),
        )
    }

    fn coarse_reader_ask_policy() -> PermissionPolicy {
        PermissionPolicy::default().with_input_rule(
            PermissionToolSelector::exact("read_mcp_resource"),
            PermissionInputRule::new(
                vec![condition(
                    "/server",
                    PermissionInputValueMatcher::WildcardPrefix(String::new()),
                )],
                PermissionEffect::ask(),
            )
            .unwrap(),
        )
    }

    fn approve_reader_occurrence(
        state: &mut MiddlewareStateLog,
        suspension: crate::middleware::Suspension,
    ) -> String {
        let request = suspension.external_request.unwrap();
        let mut approvals = PermissionRuntimeState::default();
        approvals.allow_once(
            &crate::ExternalRequestId::new(request.id.clone()),
            request.tool_call_id.unwrap(),
            PermissionApprovalStage::Contextual,
            serde_json::from_value(request.payload["claim_identities"].clone()).unwrap(),
        );
        state.set(
            PERMISSION_STATE_NAMESPACE,
            serde_json::to_value(approvals).unwrap(),
        );
        request.id
    }

    #[tokio::test]
    async fn coarse_template_approval_binds_full_selector_at_the_same_occurrence() {
        let fixture = Fixture::new().await;
        let policy = coarse_reader_ask_policy();
        let mut state = MiddlewareStateLog::default();
        let original = json!({"server":"waltr","uri":"waltr://resources"});
        let TemplateReferenceRender::Suspend(suspension) = fixture
            .render_reader_occurrence(&policy, &mut state, original.clone())
            .await
        else {
            panic!("coarse input rule asks")
        };
        assert!(fixture.handler.reads.lock().unwrap().is_empty());
        let approved_id = approve_reader_occurrence(&mut state, suspension);
        for changed in [
            json!({"server":"waltr","uri":"waltr://sessions"}),
            json!({"server":"other","uri":"waltr://resources"}),
        ] {
            if changed["server"] == original["server"] {
                assert_eq!(
                    permission_claim_identities(
                        &policy
                            .evaluate_input("read_mcp_resource", &original)
                            .unwrap()
                            .claims
                    ),
                    permission_claim_identities(
                        &policy
                            .evaluate_input("read_mcp_resource", &changed)
                            .unwrap()
                            .claims
                    ),
                    "URI is deliberately absent from the Ask claim"
                );
            }
            let TemplateReferenceRender::Suspend(suspension) = fixture
                .render_reader_occurrence(&policy, &mut state, changed)
                .await
            else {
                panic!("changed selector requires a new approval")
            };
            assert_ne!(suspension.external_request.unwrap().id, approved_id);
            assert!(fixture.handler.reads.lock().unwrap().is_empty());
        }
        let TemplateReferenceRender::Rendered(rendered) = fixture
            .render_reader_occurrence(&policy, &mut state, original)
            .await
        else {
            panic!("original approval resumes")
        };
        assert!(rendered.contains("resource data for waltr://resources"));
        assert_eq!(*fixture.handler.reads.lock().unwrap(), ["waltr://resources"]);
        fixture.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reader_completion_cache_requires_exact_arguments_and_policy_authority() {
        let fixture = Fixture::new().await;
        let policy = coarse_reader_ask_policy();
        let mut state = MiddlewareStateLog::default();
        state.set(
            "template_tool_results",
            json!({"same-reader-occurrence":"private legacy reader data"}),
        );
        let original = json!({"server":"waltr","uri":"waltr://resources"});
        let TemplateReferenceRender::Suspend(suspension) = fixture
            .render_reader_occurrence(&policy, &mut state, original.clone())
            .await
        else {
            panic!("unbound legacy cache cannot authorize a read")
        };
        assert!(fixture.handler.reads.lock().unwrap().is_empty());
        approve_reader_occurrence(&mut state, suspension);
        let TemplateReferenceRender::Rendered(first) = fixture
            .render_reader_occurrence(&policy, &mut state, original.clone())
            .await
        else {
            panic!("approved read completes")
        };
        assert_eq!(fixture.handler.reads.lock().unwrap().len(), 1);
        assert!(!first.contains("private legacy reader data"));
        let TemplateReferenceRender::Rendered(retry) = fixture
            .render_reader_occurrence(&policy, &mut state, original.clone())
            .await
        else {
            panic!("exact authorized completion retries from cache")
        };
        assert_eq!(retry, first);
        assert_eq!(fixture.handler.reads.lock().unwrap().len(), 1);
        let changed = json!({"server":"waltr","uri":"waltr://resources/agents%2Fcoding.yaml/source"});
        assert!(matches!(
            fixture
                .render_reader_occurrence(&policy, &mut state, changed)
                .await,
            TemplateReferenceRender::Suspend(_)
        ));
        assert_eq!(fixture.handler.reads.lock().unwrap().len(), 1);
        let denied = policy.clone().with_input_rule(
            PermissionToolSelector::exact("read_mcp_resource"),
            PermissionInputRule::new(
                vec![
                    condition("/server", exact("waltr")),
                    condition("/uri", exact("waltr://resources")),
                ],
                PermissionEffect::deny(),
            )
            .unwrap(),
        );
        let TemplateReferenceRender::Rendered(error) = fixture
            .render_reader_occurrence(&denied, &mut state, original.clone())
            .await
        else {
            panic!("narrower denial returns an error")
        };
        assert!(error.contains("permission denied"), "{error}");
        assert!(!error.contains("resource data for waltr://resources"));
        assert_eq!(fixture.handler.reads.lock().unwrap().len(), 1);
        assert!(matches!(
            fixture
                .render_reader_occurrence(&ask_policy(), &mut state, original.clone())
                .await,
            TemplateReferenceRender::Suspend(_)
        ));
        assert_eq!(fixture.handler.reads.lock().unwrap().len(), 1);
        let TemplateReferenceRender::Rendered(restored) = fixture
            .render_reader_occurrence(&policy, &mut state, original)
            .await
        else {
            panic!("exact original completion remains valid")
        };
        assert_eq!(restored, first);
        assert_eq!(fixture.handler.reads.lock().unwrap().len(), 1);
        fixture.runtime.shutdown().await.unwrap();
    }

    fn ask_policy() -> PermissionPolicy {
        PermissionPolicy::default().with_input_rule(
            PermissionToolSelector::exact("read_mcp_resource"),
            PermissionInputRule::new(
                ["/server", "/uri"]
                    .into_iter()
                    .map(|pointer| {
                        condition(
                            pointer,
                            PermissionInputValueMatcher::WildcardPrefix(String::new()),
                        )
                    })
                    .collect(),
                PermissionEffect::ask(),
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn template_tool_reader_denies_before_io_and_resumes_exact_one_shot() {
        let fixture = Fixture::new().await;
        for uri in ["waltr://sessions", "waltr://sessions/123"] {
            let TemplateRender::Rendered(rendered) = fixture
                .render_tool(
                    &resource_only_policy(),
                    &mut MiddlewareStateLog::default(),
                    uri,
                )
                .await
            else {
                panic!("denial does not suspend")
            };
            assert!(rendered.contains("permission denied"), "{rendered}");
            assert!(fixture.handler.reads.lock().unwrap().is_empty());
        }
        let policy = ask_policy();
        let mut state = MiddlewareStateLog::default();
        let TemplateRender::Suspend(suspension) = fixture
            .render_tool(&policy, &mut state, "waltr://resources")
            .await
        else {
            panic!("input Ask suspends")
        };
        assert!(fixture.handler.reads.lock().unwrap().is_empty());
        let request = suspension.external_request.unwrap();
        assert_eq!(request.payload["template"], true);
        let mut approvals = PermissionRuntimeState::default();
        approvals.allow_once(
            &crate::ExternalRequestId::new(request.id),
            request.tool_call_id.unwrap(),
            PermissionApprovalStage::Contextual,
            serde_json::from_value(request.payload["claim_identities"].clone()).unwrap(),
        );
        state.set(
            PERMISSION_STATE_NAMESPACE,
            serde_json::to_value(approvals).unwrap(),
        );
        assert!(matches!(
            fixture
                .render_tool(
                    &policy,
                    &mut state,
                    "waltr://resources/agents%2Fcoding.yaml/source"
                )
                .await,
            TemplateRender::Suspend(_)
        ));
        assert!(fixture.handler.reads.lock().unwrap().is_empty());
        let TemplateRender::Rendered(rendered) = fixture
            .render_tool(&policy, &mut state, "waltr://resources")
            .await
        else {
            panic!("matching approval resumes")
        };
        assert!(rendered.contains("resource data"), "{rendered}");
        assert_eq!(fixture.handler.reads.lock().unwrap().len(), 1);
        let approvals: PermissionRuntimeState =
            serde_json::from_value(state.get(PERMISSION_STATE_NAMESPACE).unwrap().clone()).unwrap();
        assert!(approvals.one_shot.is_empty());
        assert!(matches!(
            fixture
                .render_tool(&policy, &mut state, "waltr://resources")
                .await,
            TemplateRender::Rendered(_)
        ));
        assert_eq!(
            fixture.handler.reads.lock().unwrap().len(),
            1,
            "completed reference is cached on retry"
        );
        fixture.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn direct_reader_input_ask_suspends_before_io_and_consumes_one_shot_on_resume() {
        use crate::SessionStore as _;
        use futures_util::StreamExt as _;
        let fixture = Fixture::new().await;
        let agent = crate::AgentRuntime::builder(
            crate::ModelRef::new(
                crate::ProviderId::OpenAi,
                "test",
                crate::ModelApi::OpenAiResponses,
            ),
            Arc::new(ReaderProvider(
                json!({"server":"waltr","uri":"waltr://resources"}),
            )),
        )
        .with_mcp_resources(fixture.runtime.clone(), true)
        .with_permission_policy(ask_policy())
        .build();
        let store = crate::MemorySessionStore::new();
        let sessions = crate::SessionCoordinator::new(store.clone());
        let id = sessions
            .create(crate::SessionCreateRequest::new(
                crate::SessionId::generate(),
            ))
            .await
            .unwrap()
            .id()
            .clone();
        let initial = sessions
            .prompt(&agent, id.clone(), "read catalog")
            .stream()
            .collect::<Vec<_>>()
            .await;
        let request = initial
            .iter()
            .find_map(|event| match &event.as_ref().unwrap().content {
                crate::TimelineContent::ExternalRequest { request } => Some(request),
                _ => None,
            })
            .expect("reader asks for contextual approval");
        assert!(!request.payload["claims"].as_array().unwrap().is_empty());
        assert!(fixture.handler.reads.lock().unwrap().is_empty());
        let resumed = sessions
            .resume(
                &agent,
                id.clone(),
                crate::ResumeInput::ExternalResponse {
                    request_id: request.id.clone(),
                    response: crate::ExternalResponse::new(json!(
                        crate::PermissionResponse::AllowOnce
                    )),
                },
            )
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(resumed.iter().any(|event| matches!(&event.as_ref().unwrap().content, crate::TimelineContent::ToolResult { result } if !result.is_error)));
        assert_eq!(*fixture.handler.reads.lock().unwrap(), ["waltr://resources"]);
        let snapshot = store.load(&id).await.unwrap().snapshot();
        let approvals: PermissionRuntimeState =
            serde_json::from_value(snapshot.middleware_state[PERMISSION_STATE_NAMESPACE].clone())
                .unwrap();
        assert!(approvals.one_shot.is_empty());
        fixture.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reader_gate_binds_one_shot_to_call_and_final_selector_and_preserves_reusable_patterns()
    {
        let fixture = Fixture::new().await;
        let reader = McpResourcesMiddleware::new(fixture.runtime.clone());
        let policy = ask_policy();
        let mut state = MiddlewareStateLog::default();
        let args = json!({"server":"waltr","uri":"waltr://resources"});
        let input = policy.evaluate_input("read_mcp_resource", &args).unwrap();
        let mut approved_call =
            crate::ToolCall::new("approved-reader", "read_mcp_resource", args.clone());
        let mut claims = Vec::new();
        let mut ctx = ToolCallContext {
            approved_tool_name: "read_mcp_resource",
            permission_policy: &policy,
            contextual_input_is_approved: crate::template::contextual_input_is_approved,
            contextual_suspension: crate::template::contextual_suspension,
            cancellation_token: CancellationToken::new(),
            session_id: "session",
            turn: 0,
            call: &mut approved_call,
            permission_claims: &mut claims,
            middleware_state: &mut state,
        };
        let HookOutcome::Suspend(suspension) = reader.before_tool_call(&mut ctx).await.unwrap()
        else {
            panic!("input asks")
        };
        let request = suspension.external_request.unwrap();
        let mut approvals = PermissionRuntimeState::default();
        approvals.allow_once(
            &crate::ExternalRequestId::new(request.id),
            "approved-reader",
            PermissionApprovalStage::Contextual,
            permission_claim_identities(&input.claims),
        );
        ctx.middleware_state.set(
            PERMISSION_STATE_NAMESPACE,
            serde_json::to_value(&approvals).unwrap(),
        );
        ctx.call.arguments["uri"] = json!("waltr://sessions");
        assert!(matches!(
            reader.before_tool_call(&mut ctx).await.unwrap(),
            HookOutcome::Suspend(_)
        ));
        ctx.call.arguments = args;
        ctx.call.id = "another-reader".into();
        assert!(matches!(
            reader.before_tool_call(&mut ctx).await.unwrap(),
            HookOutcome::Suspend(_)
        ));
        assert!(fixture.handler.reads.lock().unwrap().is_empty());
        approvals.patterns = serde_json::from_value(
            (ctx.contextual_suspension)(&ctx, &input.claims)
                .external_request
                .unwrap()
                .payload["patterns"]
                .clone(),
        )
        .unwrap();
        approvals.one_shot.clear();
        ctx.middleware_state.set(
            PERMISSION_STATE_NAMESPACE,
            serde_json::to_value(approvals).unwrap(),
        );
        ctx.approved_tool_name = "other-reader";
        assert!(reader.before_tool_call(&mut ctx).await.is_err());
        assert!(fixture.handler.reads.lock().unwrap().is_empty());
        ctx.approved_tool_name = "read_mcp_resource";
        assert!(matches!(
            reader.before_tool_call(&mut ctx).await.unwrap(),
            HookOutcome::ReplaceToolResult(_)
        ));
        ctx.call.arguments["uri"] = json!("waltr://sessions");
        assert!(matches!(
            reader.before_tool_call(&mut ctx).await.unwrap(),
            HookOutcome::Suspend(_)
        ));
        assert_eq!(*fixture.handler.reads.lock().unwrap(), ["waltr://resources"]);
        fixture.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resource_templates_allow_catalog_and_source_without_tool_call_ids() {
        let fixture = Fixture::new().await;
        let prompt = "@resource(waltr, \"waltr://resources\") @resource(waltr, \"waltr://resources/agents%2Fcoding.yaml/source\")";
        let rendered = fixture
            .render(
                &resource_only_policy(),
                &mut MiddlewareStateLog::default(),
                true,
                prompt,
            )
            .await;
        assert!(
            rendered.contains("kind=\"mcp_resource\" source=\"waltr:waltr://resources\""),
            "{rendered}"
        );
        assert!(
            rendered.contains("resource data for waltr://resources/agents%2Fcoding.yaml/source"),
            "{rendered}"
        );
        assert_eq!(
            *fixture.handler.reads.lock().unwrap(),
            [
                "waltr://resources",
                "waltr://resources/agents%2Fcoding.yaml/source"
            ]
        );
        fixture.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resource_templates_deny_listed_and_template_session_reads_and_other_servers() {
        let fixture = Fixture::new().await;
        for prompt in [
            "@resource(waltr, \"waltr://sessions\")",
            "@resource(waltr, \"waltr://sessions/123\")",
            "@resource(other, \"waltr://resources\")",
        ] {
            let rendered = fixture
                .render(
                    &resource_only_policy(),
                    &mut MiddlewareStateLog::default(),
                    true,
                    prompt,
                )
                .await;
            assert!(
                rendered.contains("permission denied: tool input"),
                "{rendered}"
            );
            assert!(fixture.handler.reads.lock().unwrap().is_empty());
        }
        fixture.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resource_templates_fail_closed_on_whole_tool_deny_and_ask() {
        let fixture = Fixture::new().await;
        for (effect, expected) in [
            (PermissionEffect::deny(), "tool is not permitted"),
            (PermissionEffect::ask(), "permission required"),
        ] {
            let policy = PermissionPolicy::default()
                .with_tool_rule(PermissionToolSelector::exact("read_mcp_resource"), effect);
            let rendered = fixture
                .render(
                    &policy,
                    &mut MiddlewareStateLog::default(),
                    true,
                    "@resource(waltr, \"waltr://resources\")",
                )
                .await;
            assert!(rendered.contains(expected), "{rendered}");
            assert!(fixture.handler.reads.lock().unwrap().is_empty());
        }
        fixture.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resource_templates_input_ask_reads_only_after_reusable_approval() {
        let fixture = Fixture::new().await;
        let policy = PermissionPolicy::default().with_input_rule(
            PermissionToolSelector::exact("read_mcp_resource"),
            PermissionInputRule::new(
                vec![
                    condition("/server", exact("waltr")),
                    condition("/uri", exact("waltr://resources")),
                ],
                PermissionEffect::ask(),
            )
            .unwrap(),
        );
        let mut state = MiddlewareStateLog::default();
        let prompt = "@resource(waltr, \"waltr://resources\")";
        let rendered = fixture.render(&policy, &mut state, true, prompt).await;
        assert!(rendered.contains("permission required"), "{rendered}");
        assert!(fixture.handler.reads.lock().unwrap().is_empty());
        let mut approvals = PermissionRuntimeState::default();
        let input = policy
            .evaluate_input(
                "read_mcp_resource",
                &json!({"server":"waltr","uri":"waltr://resources"}),
            )
            .unwrap();
        approvals.allow_once(
            &crate::ExternalRequestId::new("direct-reader-approval"),
            "direct-reader",
            PermissionApprovalStage::Contextual,
            permission_claim_identities(&input.claims),
        );
        state.set(
            PERMISSION_STATE_NAMESPACE,
            serde_json::to_value(&approvals).unwrap(),
        );
        let rendered = fixture.render(&policy, &mut state, true, prompt).await;
        assert!(rendered.contains("permission required"), "{rendered}");
        assert!(fixture.handler.reads.lock().unwrap().is_empty());
        for (pointer, value) in [("/server", "waltr"), ("/uri", "waltr://resources")] {
            approvals.patterns.push(PermissionPattern {
                operation: Some(PermissionOperation::Execute),
                resource: PermissionResourcePattern::ToolInput {
                    tool: "read_mcp_resource".into(),
                    pointer: PermissionJsonPointer::parse(pointer).unwrap(),
                    value: PermissionInputScalar::String(value.into()),
                },
            });
        }
        state.set(
            PERMISSION_STATE_NAMESPACE,
            serde_json::to_value(approvals).unwrap(),
        );
        let rendered = fixture.render(&policy, &mut state, true, prompt).await;
        assert!(rendered.contains("kind=\"mcp_resource\""), "{rendered}");
        assert_eq!(fixture.handler.reads.lock().unwrap().len(), 1);
        let denied = policy.with_tool_rule(
            PermissionToolSelector::exact("read_mcp_resource"),
            PermissionEffect::deny(),
        );
        let rendered = fixture.render(&denied, &mut state, true, prompt).await;
        assert!(rendered.contains("tool is not permitted"), "{rendered}");
        assert_eq!(fixture.handler.reads.lock().unwrap().len(), 1);
        fixture.runtime.shutdown().await.unwrap();
    }

    struct ReaderProvider(Value);

    struct RewriteReader;

    impl AgentMiddleware for RewriteReader {
        async fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<HookOutcome> {
            ctx.call.arguments["uri"] = json!("waltr://sessions");
            Ok(HookOutcome::Continue)
        }
    }

    #[tokio::test]
    async fn middleware_selector_mutation_cannot_fetch_denied_resource_data() {
        use futures_util::StreamExt as _;
        let fixture = Fixture::new().await;
        let agent = crate::AgentRuntime::builder(
            crate::ModelRef::new(
                crate::ProviderId::OpenAi,
                "test",
                crate::ModelApi::OpenAiResponses,
            ),
            Arc::new(ReaderProvider(
                json!({"server":"waltr","uri":"waltr://resources"}),
            )),
        )
        .with_middleware(RewriteReader)
        .with_middleware(McpResourcesMiddleware::new(fixture.runtime.clone()))
        .with_permission_policy(resource_only_policy())
        .build();
        let sessions = crate::SessionCoordinator::new(crate::MemorySessionStore::new());
        let id = sessions
            .create(crate::SessionCreateRequest::new(
                crate::SessionId::generate(),
            ))
            .await
            .unwrap()
            .id()
            .clone();
        let events = sessions
            .prompt(&agent, id, "read catalog")
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(&event.as_ref().unwrap().content,
            crate::TimelineContent::ToolResult { result } if result.is_error && result.content.contains("permission denied"))));
        assert!(fixture.handler.reads.lock().unwrap().is_empty());
        fixture.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn whole_tool_gate_and_approval_do_not_override_reader_input_denial() {
        use futures_util::StreamExt as _;
        let fixture = Fixture::new().await;
        for effect in [PermissionEffect::deny(), PermissionEffect::ask()] {
            let level = effect.level;
            let agent = crate::AgentRuntime::builder(
                crate::ModelRef::new(
                    crate::ProviderId::OpenAi,
                    "test",
                    crate::ModelApi::OpenAiResponses,
                ),
                Arc::new(ReaderProvider(
                    json!({"server":"waltr","uri":"waltr://sessions"}),
                )),
            )
            .with_mcp_resources(fixture.runtime.clone(), true)
            .with_permission_policy(
                resource_only_policy()
                    .with_tool_rule(PermissionToolSelector::exact("read_mcp_resource"), effect),
            )
            .build();
            let sessions = crate::SessionCoordinator::new(crate::MemorySessionStore::new());
            let id = sessions
                .create(crate::SessionCreateRequest::new(
                    crate::SessionId::generate(),
                ))
                .await
                .unwrap()
                .id()
                .clone();
            let initial = sessions
                .prompt(&agent, id.clone(), "read sessions")
                .stream()
                .collect::<Vec<_>>()
                .await;
            assert!(fixture.handler.reads.lock().unwrap().is_empty());
            if level == PermissionLevel::Ask {
                let request = initial
                    .iter()
                    .find_map(|event| match &event.as_ref().unwrap().content {
                        crate::TimelineContent::ExternalRequest { request } => Some(request),
                        _ => None,
                    })
                    .expect("whole-tool Ask suspends");
                assert_eq!(request.payload["claims"], json!([]));
                let resumed = sessions
                    .resume(
                        &agent,
                        id,
                        crate::ResumeInput::ExternalResponse {
                            request_id: request.id.clone(),
                            response: crate::ExternalResponse::new(json!(
                                crate::PermissionResponse::AllowOnce
                            )),
                        },
                    )
                    .stream()
                    .collect::<Vec<_>>()
                    .await;
                assert!(resumed.iter().any(|event| matches!(&event.as_ref().unwrap().content,
                    crate::TimelineContent::ToolResult { result } if result.is_error && result.content.contains("permission denied"))));
            } else {
                assert!(initial.iter().any(|event| matches!(&event.as_ref().unwrap().content,
                    crate::TimelineContent::ToolResult { result } if result.is_error && result.content.contains("not permitted"))));
            }
            assert!(fixture.handler.reads.lock().unwrap().is_empty());
        }
        fixture.runtime.shutdown().await.unwrap();
    }

    impl crate::LlmStream for ReaderProvider {
        fn name(&self) -> &'static str {
            "template-permission-reader"
        }

        fn stream_request<'a>(
            &'a self,
            request: crate::CompletionRequest<'a>,
        ) -> crate::BoxStreamFuture<'a> {
            let event = if request.tool_results().next().is_some() {
                crate::CompletionEvent::Finished
            } else {
                crate::CompletionEvent::ToolCall(crate::ToolCall::new(
                    "direct-reader",
                    "read_mcp_resource",
                    self.0.clone(),
                ))
            };
            Box::pin(async move {
                Ok(Box::pin(futures_util::stream::iter([Ok(event)])) as crate::BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn resource_reader_and_templates_share_input_policy_results() {
        use futures_util::StreamExt as _;

        let fixture = Fixture::new().await;
        for (server, uri, allowed) in [
            ("waltr", "waltr://resources", true),
            ("waltr", "waltr://resources/agents%2Fcoding.yaml/source", true),
            ("waltr", "waltr://sessions", false),
            ("waltr", "waltr://sessions/123", false),
            ("other", "waltr://resources", false),
        ] {
            let before = fixture.handler.reads.lock().unwrap().len();
            let policy = resource_only_policy();
            let agent = crate::AgentRuntime::builder(
                crate::ModelRef::new(
                    crate::ProviderId::OpenAi,
                    "test",
                    crate::ModelApi::OpenAiResponses,
                ),
                Arc::new(ReaderProvider(json!({"server": server, "uri": uri}))),
            )
            .with_mcp_resources(fixture.runtime.clone(), true)
            .with_permission_policy(policy.clone())
            .build();
            let sessions = crate::SessionCoordinator::new(crate::MemorySessionStore::new());
            let id = sessions
                .create(crate::SessionCreateRequest::new(
                    crate::SessionId::generate(),
                ))
                .await
                .unwrap()
                .id()
                .clone();
            let events = sessions
                .prompt(&agent, id, "read resource data")
                .stream()
                .collect::<Vec<_>>()
                .await;
            let result = events
                .iter()
                .find_map(|event| match &event.as_ref().unwrap().content {
                    crate::TimelineContent::ToolResult { result } => Some(result),
                    _ => None,
                })
                .expect("the direct reader attempt produces a result");
            assert_eq!(result.is_error, !allowed, "{result:?}");
            let after_direct = fixture.handler.reads.lock().unwrap().len();
            assert_eq!(after_direct - before, usize::from(allowed));
            let prompt = format!("@resource({server}, {uri:?})");
            let rendered = fixture
                .render(&policy, &mut MiddlewareStateLog::default(), true, &prompt)
                .await;
            assert_eq!(
                rendered.contains("<template-error>"),
                !allowed,
                "{rendered}"
            );
            assert_eq!(
                fixture.handler.reads.lock().unwrap().len() - after_direct,
                usize::from(allowed)
            );
        }
        fixture.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resource_templates_preserve_discovery_and_resources_disabled_gates() {
        let fixture = Fixture::new().await;
        for (ordinary, prompt) in [
            (false, "@resource(waltr, \"waltr://resources\")"),
            (true, "@resource(unknown, \"waltr://resources\")"),
            (true, "@resource(waltr, \"unlisted://item/path\")"),
            (true, "@resource(waltr, \"invalid-uri\")"),
            (true, "@resource(waltr, 123)"),
        ] {
            let rendered = fixture
                .render(
                    &PermissionPolicy::default(),
                    &mut MiddlewareStateLog::default(),
                    ordinary,
                    prompt,
                )
                .await;
            assert!(rendered.contains("<template-error>"), "{rendered}");
            assert!(fixture.handler.reads.lock().unwrap().is_empty());
        }
        fixture.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn nested_resource_templates_cannot_bypass_input_denial() {
        let fixture = Fixture::new().await;
        let rendered = fixture
            .render(
                &resource_only_policy(),
                &mut MiddlewareStateLog::default(),
                true,
                "@resource(waltr, \"waltr://resources/nested/source\")",
            )
            .await;
        assert!(rendered.contains("kind=\"mcp_resource\""), "{rendered}");
        assert!(
            rendered.contains("permission denied: tool input"),
            "{rendered}"
        );
        assert_eq!(
            *fixture.handler.reads.lock().unwrap(),
            ["waltr://resources/nested/source"]
        );
        fixture.runtime.shutdown().await.unwrap();
    }
}
