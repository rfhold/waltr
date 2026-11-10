//! MCP resource data reader and user-inserted resource templates.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent_host::{
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
    context::{TemplateRenderContext, TemplateSearchContext, ToolCallContext},
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

impl McpResourcesMiddleware {
    pub(crate) fn definition(ordinary: bool) -> ToolDefinition {
        let description = if ordinary {
            "Read raw MCP resource DATA from the named server and direct URI, whether listed, template-derived, or unlisted. Does not activate skills or evaluate templates. Loaded skill files are automatically verified against every held manifest; unlisted paths beneath held roots are rejected."
        } else {
            "Read raw DATA for a file in a loaded, approved MCP skill's held manifest, using server and the exact file URI. Ordinary resource reads are disabled. Automatically verifies all matching held manifests. Does not activate skills or evaluate templates."
        };
        ToolDefinition::new(
            "read_mcp_resource",
            description,
            json!({
                "type": "object",
                "properties": {"server": {"type": "string", "minLength": 1}, "uri": {"type": "string", "minLength": 1}},
                "required": ["server", "uri"],
                "additionalProperties": false
            }),
        )
    }
}

impl AgentMiddleware for McpResourcesMiddleware {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<HookOutcome> {
        if ctx.call.name != "read_mcp_resource" {
            return Ok(HookOutcome::Continue);
        }
        let ResourceSelector { server, uri } = resource_selector(ctx)?;
        if let Some(outcome) = resource_input_permission(ctx).await? {
            return Ok(outcome);
        }
        let manifests = McpSkillsMiddleware::resource_manifests(ctx, &server, &uri)?;
        if !self.ordinary && manifests.is_empty() {
            return Err(LlmError::Protocol { provider: "mcp-resources", message: "ordinary MCP resource reads are disabled; only loaded held skill files may be read".into() });
        }
        let response = match self
            .runtime
            .read_resource_data(
                &server,
                &uri,
                ctx.cancellation_token.clone(),
                ctx.authorization.as_ref().and_then(|authorization| {
                    crate::agent_host::mcp::elicitation::McpOperation::for_context(
                        authorization.policy.as_ref(),
                        &authorization.context,
                    )
                }),
            )
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
            if let Err(error) = crate::agent_host::mcp::skills::verified_resource_bytes(
                manifest,
                &uri,
                response.clone(),
            ) {
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

        let result = crate::agent_host::template::execute_template_capability(
            ctx,
            "read_mcp_resource",
            json!({"server": server, "uri": uri}),
        )
        .await?;
        if result.is_error {
            return Ok(TemplateReferenceRender::Rendered(template_error(
                &format!("mcp resource {server}:{uri}"),
                result.content,
            )));
        }
        Ok(TemplateReferenceRender::TemplateResult {
            kind: "mcp_resource".into(),
            tool: None,
            source: Some(format!("{server}:{uri}")),
            content: resource_template_text(&result)?,
            render_nested: false,
        })
    }
}

fn resource_template_text(result: &ToolResult) -> Result<String> {
    let envelope: Value = serde_json::from_str(&result.content.to_string())?;
    let data = envelope
        .get("data")
        .cloned()
        .ok_or_else(|| LlmError::Protocol {
            provider: "mcp-resources",
            message: "fenced resource result has no data".into(),
        })?;
    Ok(mcp::McpResourceResult::new(data).format_text())
}

async fn resource_input_permission(ctx: &ToolCallContext<'_>) -> Result<Option<HookOutcome>> {
    ctx.authorize(&[]).await?;
    Ok(None)
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
    #[test]
    fn fenced_resource_template_unwraps_only_the_authorized_data_not_the_tool_envelope() {
        let data =
            mcp::McpResourceResult::text("resource://held", "text/plain", "@tool(not-an-effect)");
        let result = super::ToolResult::new(
            "call",
            "read_mcp_resource",
            serde_json::json!({"server":"upstream","uri":"resource://held",
            "data":data.raw,"verified_skill_origins":[]})
            .to_string(),
        );
        assert_eq!(
            super::resource_template_text(&result).unwrap(),
            data.format_text()
        );
        let legacy = super::ToolResult::new("call", "read_mcp_resource", data.raw.to_string());
        assert!(super::resource_template_text(&legacy).is_err());
    }
    use std::sync::Mutex;

    use mcp::{
        McpResourceList, McpResourceResult, McpResourceTemplateList,
        server::{
            BoxFuture, ServerCapabilities, ServerContext, ServerHandler, ServerInfo, ServerResult,
        },
    };

    use super::*;
    use crate::agent_host::{
        PermissionEffect, PermissionInputCondition, PermissionInputRule, PermissionInputScalar,
        PermissionInputValueMatcher, PermissionJsonPointer, PermissionPolicy,
        PermissionToolSelector,
        mcp::{McpConfig, McpServerConfig},
        middleware::context::MiddlewareStateLog,
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
            _state: &mut MiddlewareStateLog,
            ordinary: bool,
            prompt: &str,
        ) -> String {
            let captured = Arc::new(std::sync::Mutex::new(String::new()));
            let output = captured.clone();
            let provider = Arc::new(crate::host_tests::scripted_provider::ScriptedProvider::new(
                "resource-render",
                move |request| {
                    *output.lock().unwrap() = request.prompt.to_owned();
                    vec![Ok(waltr_agent::CompletionEvent::Finished)]
                },
            ));
            let host = crate::host_tests::harness::HostSession::new(
                crate::agent_host::HostRuntimeBuilder::new(
                    waltr_agent::ModelRef::new(
                        waltr_agent::ProviderId::OpenAi,
                        "test",
                        waltr_agent::ModelApi::OpenAiResponses,
                    ),
                    provider,
                )
                .with_mcp_resources(self.runtime.clone(), ordinary)
                .with_permission_policy(policy.clone()),
            )
            .await;
            let owner = host.start(prompt);
            if policy.evaluate_tool("read_mcp_resource") == crate::permission::PermissionLevel::Ask
            {
                let pending = crate::host_tests::pending(&host.broker, &host.id).await;
                crate::host_tests::respond(
                    &host.broker,
                    &pending,
                    crate::interaction::InteractionResponse::Permission(
                        crate::permission::PermissionResponse::Deny {
                            message: None,
                            mode: crate::permission::PermissionDenyMode::ReturnToolResult,
                        },
                    ),
                )
                .unwrap();
            }
            let events = tokio::time::timeout(std::time::Duration::from_secs(5), owner)
                .await
                .expect("resource render deadline")
                .unwrap();
            assert!(events.iter().all(Result::is_ok), "{events:?}");
            host.close().await;
            captured.lock().unwrap().clone()
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
            (PermissionEffect::deny(), "permission denied"),
            (PermissionEffect::ask(), "permission denied"),
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
        let agent = crate::agent_host::HostRuntimeBuilder::new(
            crate::agent_host::ModelRef::new(
                crate::agent_host::ProviderId::OpenAi,
                "test",
                crate::agent_host::ModelApi::OpenAiResponses,
            ),
            Arc::new(ReaderProvider(
                json!({"server":"waltr","uri":"waltr://resources"}),
            )),
        )
        .with_middleware(RewriteReader)
        .with_mcp_resources(fixture.runtime.clone(), true)
        .with_permission_policy(resource_only_policy());
        let sessions = crate::agent_host::SessionCoordinator::new(
            crate::agent_host::MemorySessionStore::new(),
        );
        let id = sessions
            .create(crate::agent_host::SessionCreateRequest::new(
                crate::agent_host::SessionId::generate(),
            ))
            .await
            .unwrap()
            .id()
            .clone();
        let agent = agent
            .with_dispatch_context(
                Some(id.clone()),
                crate::permission::ServerPermissionPolicy::sessions(
                    crate::interaction::InteractionBroker::new(),
                ),
            )
            .build();
        let events = sessions
            .prompt(&agent, id, "read catalog")
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(&event.as_ref().unwrap().content,
            crate::agent_host::TimelineContent::ToolResult { result } if result.is_error && result.content.contains("permission denied"))));
        assert!(fixture.handler.reads.lock().unwrap().is_empty());
        fixture.runtime.shutdown().await.unwrap();
    }

    impl crate::agent_host::LlmStream for ReaderProvider {
        fn name(&self) -> &'static str {
            "template-permission-reader"
        }

        fn stream_request<'a>(
            &'a self,
            request: crate::agent_host::CompletionRequest<'a>,
        ) -> crate::agent_host::BoxStreamFuture<'a> {
            let event = if request.tool_results().next().is_some() {
                crate::agent_host::CompletionEvent::Finished
            } else {
                crate::agent_host::CompletionEvent::ToolCall(crate::agent_host::ToolCall::new(
                    "direct-reader",
                    "read_mcp_resource",
                    self.0.clone(),
                ))
            };
            Box::pin(async move {
                Ok(Box::pin(futures_util::stream::iter([Ok(event)]))
                    as crate::agent_host::BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn resource_reader_and_templates_share_input_policy_results() {
        use futures_util::StreamExt as _;

        let fixture = Fixture::new().await;
        for (server, uri, allowed) in [
            ("waltr", "waltr://resources", true),
            (
                "waltr",
                "waltr://resources/agents%2Fcoding.yaml/source",
                true,
            ),
            ("waltr", "waltr://sessions", false),
            ("waltr", "waltr://sessions/123", false),
            ("other", "waltr://resources", false),
        ] {
            let before = fixture.handler.reads.lock().unwrap().len();
            let policy = resource_only_policy();
            let agent = crate::agent_host::HostRuntimeBuilder::new(
                crate::agent_host::ModelRef::new(
                    crate::agent_host::ProviderId::OpenAi,
                    "test",
                    crate::agent_host::ModelApi::OpenAiResponses,
                ),
                Arc::new(ReaderProvider(json!({"server": server, "uri": uri}))),
            )
            .with_mcp_resources(fixture.runtime.clone(), true)
            .with_permission_policy(policy.clone());
            let sessions = crate::agent_host::SessionCoordinator::new(
                crate::agent_host::MemorySessionStore::new(),
            );
            let id = sessions
                .create(crate::agent_host::SessionCreateRequest::new(
                    crate::agent_host::SessionId::generate(),
                ))
                .await
                .unwrap()
                .id()
                .clone();
            let agent = agent
                .with_dispatch_context(
                    Some(id.clone()),
                    crate::permission::ServerPermissionPolicy::sessions(
                        crate::interaction::InteractionBroker::new(),
                    ),
                )
                .build();
            let events = sessions
                .prompt(&agent, id, "read resource data")
                .stream()
                .collect::<Vec<_>>()
                .await;
            let result = events
                .iter()
                .find_map(|event| match &event.as_ref().unwrap().content {
                    crate::agent_host::TimelineContent::ToolResult { result } => Some(result),
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
            rendered.contains("@resource(waltr, \"waltr://sessions\")"),
            "{rendered}"
        );
        assert_eq!(
            *fixture.handler.reads.lock().unwrap(),
            ["waltr://resources/nested/source"]
        );
        fixture.runtime.shutdown().await.unwrap();
    }
}
