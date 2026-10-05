use std::{collections::BTreeMap, sync::Arc};

use serde_json::{Map, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use mcp::{
    McpCompletionResult, McpPromptDefinition, McpResourceDefinition, McpResourceTemplateDefinition,
};

use crate::{AgentTool, LlmError, Result, ToolDefinition, ToolProgressSink, ToolResult};

use super::{
    config::McpConfig, event::McpRuntimeEvent, name::direct_tool_name, server::McpServerRuntime,
};

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct McpServerPromptDefinition {
    pub(crate) server_name: String,
    pub(crate) prompt: McpPromptDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct McpServerResourceDefinition {
    pub(crate) server_name: String,
    pub(crate) resource: McpResourceDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct McpServerResourceTemplateDefinition {
    pub(crate) server_name: String,
    pub(crate) resource_template: McpResourceTemplateDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct McpServerInstructions {
    pub(crate) server_name: String,
    pub(crate) instructions: String,
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) struct McpClientRuntime {
    servers: BTreeMap<String, Arc<McpServerRuntime>>,
    #[allow(dead_code)]
    events: broadcast::Sender<McpRuntimeEvent>,
}

#[derive(Debug)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct McpRuntimeStartError {
    pub(crate) server_key: String,
    pub(crate) error: LlmError,
}

impl std::fmt::Display for McpRuntimeStartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "mcp server '{}' failed to start: {}",
            self.server_key, self.error
        )
    }
}

impl std::error::Error for McpRuntimeStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

fn protocol_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "mcp",
        message: message.into(),
    }
}

impl std::fmt::Debug for McpClientRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpClientRuntime")
            .field("servers", &self.servers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

#[allow(clippy::too_many_arguments)]
impl McpClientRuntime {
    pub(crate) async fn start(
        config: McpConfig,
        root_uri: String,
        cancellation_token: CancellationToken,
    ) -> std::result::Result<Arc<Self>, McpRuntimeStartError> {
        let (servers, timeout, log_level, trace_propagation, skills) = config.into_parts();
        let mut server_labels = std::collections::BTreeSet::new();
        for server in &servers {
            if !server_labels.insert(&server.name) {
                return Err(McpRuntimeStartError {
                    server_key: server.name.clone(),
                    error: protocol_error("duplicate host-assigned MCP server label"),
                });
            }
        }
        let (events, _) = broadcast::channel(128);
        let mut active: BTreeMap<String, Arc<McpServerRuntime>> = BTreeMap::new();
        for server in servers {
            let server_key = server.name.clone();
            let runtime = match McpServerRuntime::start(
                server,
                timeout,
                log_level,
                trace_propagation,
                skills,
                root_uri.clone(),
                events.clone(),
                cancellation_token.clone(),
            )
            .await
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    for started in active.values() {
                        if let Err(shutdown_error) = started.shutdown().await {
                            tracing::warn!(
                                server = %started.name(),
                                error = %shutdown_error,
                                "failed to shut down MCP server after startup failure"
                            );
                        }
                    }
                    return Err(McpRuntimeStartError { server_key, error });
                }
            };
            active.insert(runtime.name().to_owned(), runtime);
        }
        Ok(Arc::new(Self {
            servers: active,
            events,
        }))
    }

    #[allow(dead_code)]
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<McpRuntimeEvent> {
        self.events.subscribe()
    }

    pub(crate) async fn agent_tools(self: &Arc<Self>) -> Vec<AgentTool> {
        let mut tools = Vec::new();
        for server in self.servers.values() {
            let server_name = server.name().to_owned();
            for tool in server.tools().await {
                let source = app_source(server.name(), server.source(), &tool);
                let closure_source = source.clone();
                let runtime = Arc::clone(self);
                let definition_server_name = server_name.clone();
                let tool_name = tool.name.clone();
                let definition = ToolDefinition::new(
                    direct_tool_name(&definition_server_name, &tool_name),
                    format!(
                        "MCP tool `{tool_name}` from server `{server_name}`. {}",
                        tool.description_text()
                    ),
                    tool.input_schema.clone(),
                );
                let closure_server_name = server_name.clone();
                let mut agent_tool = AgentTool::new_async(definition, move |context| {
                    let runtime = Arc::clone(&runtime);
                    let server_name = closure_server_name.clone();
                    let tool_name = tool_name.clone();
                    let arguments = context.call().arguments.clone();
                    let call_id = context.call().id.clone();
                    let progress = context.progress().clone();
                    let cancellation_token = context.cancellation_token().clone();
                    let source = closure_source.clone();
                    Box::pin(async move {
                        if !source
                            .ui
                            .visibility
                            .contains(&mcp::apps::UiVisibility::Model)
                        {
                            return Err(protocol_error("MCP tool is not model-visible"));
                        }
                        if source.ui.resource_uri.is_some()
                            && !runtime.has_current_app_declaration(&source).await
                        {
                            return Err(protocol_error(
                                "MCP app tool declaration changed before dispatch",
                            ));
                        }
                        let eligible = !source.source.is_empty();
                        if eligible
                            && source.ui.resource_uri.is_some()
                            && !runtime.app_authority_current(&server_name).await
                        {
                            return Err(protocol_error(
                                "MCP app authorization changed before dispatch",
                            ));
                        }
                        let result = runtime
                            .call_tool(
                                &server_name,
                                &tool_name,
                                arguments.clone(),
                                &call_id,
                                progress,
                                cancellation_token,
                            )
                            .await?;
                        if eligible
                            && source.ui.resource_uri.is_some()
                            && !runtime.app_authority_current(&server_name).await
                        {
                            return Err(protocol_error(
                                "MCP app authorization changed during dispatch",
                            ));
                        }
                        if let (true, Some(capture), Some(uri)) =
                            (eligible, &context.app_capture, &source.ui.resource_uri)
                        {
                            *capture.lock().expect("app capture lock poisoned") =
                                Some(super::apps::McpAppAssociation {
                                    version: 1,
                                    server: source.server,
                                    tool: source.tool,
                                    resource_uri: uri.clone(),
                                    source: source.source,
                                    declaration: source.declaration,
                                    arguments,
                                    result: result.raw.raw.clone(),
                                });
                        }
                        Ok(ToolResult::from_content(
                            context.call().id.clone(),
                            context.call().name.clone(),
                            result.content,
                            result.is_error,
                        ))
                    })
                });
                agent_tool.mcp_app_source = Some(source);
                tools.push(agent_tool);
            }
        }
        tools
    }

    async fn has_current_app_declaration(&self, source: &super::apps::McpAppSource) -> bool {
        let Some(server) = self.servers.get(&source.server) else {
            return false;
        };
        server
            .tool(&source.tool)
            .await
            .is_some_and(|tool| tool_declaration(&tool) == source.declaration)
    }

    pub(crate) async fn current_app_source(
        &self,
        server: &str,
        tool: &str,
    ) -> Option<super::apps::McpAppSource> {
        let server = self.servers.get(server)?;
        let tool = server.tool(tool).await?;
        Some(app_source(server.name(), server.source(), &tool))
    }

    pub(crate) async fn app_authority_current(&self, server: &str) -> bool {
        match self.servers.get(server) {
            Some(server) => server.app_authority_current().await,
            None => false,
        }
    }

    pub(crate) fn app_sources(&self) -> BTreeMap<String, Option<String>> {
        self.servers
            .iter()
            .filter(|(_, server)| server.has_app_authority())
            .map(|(name, server)| (name.clone(), server.source().map(str::to_owned)))
            .collect()
    }

    pub(crate) async fn prompts(&self) -> Vec<McpServerPromptDefinition> {
        let mut prompts = Vec::new();
        for server in self.servers.values() {
            let server_name = server.name().to_owned();
            for prompt in server.prompts().await {
                prompts.push(McpServerPromptDefinition {
                    server_name: server_name.clone(),
                    prompt,
                });
            }
        }
        prompts
    }

    pub(crate) async fn resources(&self) -> Vec<McpServerResourceDefinition> {
        let mut resources = Vec::new();
        for server in self.servers.values() {
            let server_name = server.name().to_owned();
            for resource in server.resources().await {
                resources.push(McpServerResourceDefinition {
                    server_name: server_name.clone(),
                    resource,
                });
            }
        }
        resources
    }

    pub(crate) async fn resource_templates(&self) -> Vec<McpServerResourceTemplateDefinition> {
        let mut resource_templates = Vec::new();
        for server in self.servers.values() {
            let server_name = server.name().to_owned();
            for resource_template in server.resource_templates().await {
                resource_templates.push(McpServerResourceTemplateDefinition {
                    server_name: server_name.clone(),
                    resource_template,
                });
            }
        }
        resource_templates
    }

    pub(crate) async fn instructions(&self) -> Vec<McpServerInstructions> {
        let mut instructions = Vec::new();
        for server in self.servers.values() {
            if let Some(server_instructions) = server.instructions().await {
                instructions.push(McpServerInstructions {
                    server_name: server.name().to_owned(),
                    instructions: server_instructions,
                });
            }
        }
        instructions
    }

    pub(crate) fn has_server(&self, server_name: &str) -> bool {
        self.servers.contains_key(server_name)
    }

    pub(crate) fn skill_origin(&self, name: &str) -> Result<String> {
        self.servers
            .get(name)
            .filter(|server| server.skills_enabled())
            .map(|server| server.skill_origin().to_owned())
            .ok_or_else(|| protocol_error("MCP Skills are not enabled for this server"))
    }

    pub(crate) async fn skill_catalog(&self) -> Result<Vec<(String, mcp::skills::McpSkillList)>> {
        self.skill_catalog_with_budget(super::skills::METADATA_BUDGET)
            .await
    }

    pub(crate) async fn skill_catalog_with_budget(
        &self,
        mut remaining: usize,
    ) -> Result<Vec<(String, mcp::skills::McpSkillList)>> {
        let mut pages = Vec::new();
        for server in self
            .servers
            .values()
            .filter(|server| server.skills_enabled())
        {
            for page in server.skill_catalog(&mut remaining).await? {
                pages.push((server.name().to_owned(), page));
            }
        }
        Ok(pages)
    }

    pub(crate) async fn tool_origins(&self, name: &str) -> Vec<String> {
        let mut origins = Vec::new();
        for server in self.servers.values() {
            if server
                .tools()
                .await
                .iter()
                .any(|tool| direct_tool_name(server.name(), &tool.name) == name)
            {
                origins.push(server.name().to_owned());
            }
        }
        origins
    }

    pub(crate) async fn get_skill(
        &self,
        server: &str,
        uri: &str,
        cancellation_token: CancellationToken,
    ) -> Result<mcp::skills::McpSkillResult> {
        self.servers
            .get(server)
            .ok_or_else(|| protocol_error("unknown skill origin"))?
            .get_skill(uri, cancellation_token)
            .await
    }

    pub(crate) async fn read_skill(
        &self,
        server: &str,
        skill: &mcp::skills::McpSkill,
        uri: &str,
        cancellation_token: CancellationToken,
    ) -> Result<Vec<u8>> {
        self.servers
            .get(server)
            .ok_or_else(|| protocol_error("unknown skill origin"))?
            .read_skill(skill, uri, cancellation_token)
            .await
    }

    pub(crate) async fn has_prompt(&self, server_name: &str, prompt_name: &str) -> bool {
        let Some(server) = self.servers.get(server_name) else {
            return false;
        };
        server
            .prompts()
            .await
            .iter()
            .any(|prompt| prompt.name == prompt_name)
    }

    pub(crate) async fn has_resource(&self, server_name: &str, uri: &str) -> bool {
        let Some(server) = self.servers.get(server_name) else {
            return false;
        };
        server
            .resources()
            .await
            .iter()
            .any(|resource| resource.uri == uri)
    }

    pub(crate) async fn has_resource_template_match(&self, server_name: &str, uri: &str) -> bool {
        let Some(server) = self.servers.get(server_name) else {
            return false;
        };
        server
            .resource_templates()
            .await
            .iter()
            .any(|resource_template| resource_template.matches_uri(uri))
    }

    async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
        call_id: &str,
        progress: ToolProgressSink,
        cancellation_token: CancellationToken,
    ) -> Result<super::server::McpAgentToolResult> {
        self.servers
            .get(server_name)
            .ok_or_else(|| protocol_error(format!("unknown MCP server `{server_name}`")))?
            .call_tool(tool_name, arguments, call_id, progress, cancellation_token)
            .await
    }

    pub(crate) async fn get_prompt(
        &self,
        server_name: &str,
        prompt_name: &str,
        arguments: Option<Value>,
        cancellation_token: CancellationToken,
    ) -> Result<String> {
        self.servers
            .get(server_name)
            .ok_or_else(|| protocol_error(format!("unknown MCP server `{server_name}`")))?
            .get_prompt(prompt_name, arguments, cancellation_token)
            .await
    }

    pub(crate) async fn call_app_tool(
        &self,
        server: &str,
        tool: &str,
        call: &crate::ToolCall,
        cancellation: CancellationToken,
    ) -> Result<(ToolResult, mcp::McpToolResult)> {
        let result = self
            .call_tool(
                server,
                tool,
                call.arguments.clone(),
                &call.id,
                ToolProgressSink::noop(),
                cancellation,
            )
            .await?;
        Ok((
            ToolResult::from_content(&call.id, &call.name, result.content, result.is_error),
            result.raw,
        ))
    }

    pub(crate) async fn read_resource(
        &self,
        server_name: &str,
        uri: &str,
        cancellation_token: CancellationToken,
    ) -> Result<String> {
        self.servers
            .get(server_name)
            .ok_or_else(|| protocol_error(format!("unknown MCP server `{server_name}`")))?
            .read_resource(uri, cancellation_token)
            .await
    }

    pub(crate) async fn read_resource_data(
        &self,
        server_name: &str,
        uri: &str,
        cancellation_token: CancellationToken,
    ) -> Result<mcp::McpResourceResult> {
        self.servers
            .get(server_name)
            .ok_or_else(|| protocol_error(format!("unknown MCP server `{server_name}`")))?
            .read_resource_data(uri, cancellation_token)
            .await
    }

    pub(crate) async fn read_app_resource(
        &self,
        server_name: &str,
        uri: &str,
        max_bytes: usize,
        cancellation_token: CancellationToken,
    ) -> Result<mcp::apps::UiResource> {
        self.servers
            .get(server_name)
            .ok_or_else(|| protocol_error(format!("unknown MCP server `{server_name}`")))?
            .read_app_resource(uri, max_bytes, cancellation_token)
            .await
    }

    pub(crate) async fn complete_resource_template(
        &self,
        server_name: &str,
        uri_template: &str,
        argument_name: &str,
        argument_value: &str,
        context_arguments: Map<String, Value>,
        cancellation_token: CancellationToken,
    ) -> Result<Option<McpCompletionResult>> {
        let Some(server) = self.servers.get(server_name) else {
            return Ok(None);
        };
        server
            .complete_resource_template(
                uri_template,
                argument_name,
                argument_value,
                context_arguments,
                cancellation_token,
            )
            .await
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let mut first_error = None;
        for server in self.servers.values() {
            if let Err(error) = server.shutdown().await {
                tracing::warn!(server = %server.name(), "failed to shut down MCP server");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn tool_declaration(tool: &mcp::McpToolDefinition) -> Value {
    serde_json::json!({"name": tool.name, "inputSchema": tool.input_schema, "outputSchema": tool.output_schema, "_meta": tool.meta, "annotations": tool.annotations})
}

fn app_source(
    server: &str,
    source: Option<&str>,
    tool: &mcp::McpToolDefinition,
) -> super::apps::McpAppSource {
    let ui = mcp::apps::UiToolDeclaration::try_from(tool)
        .map(|declaration| declaration.ui)
        .unwrap_or_else(|_| mcp::apps::UiToolMeta {
            visibility: Vec::new(),
            ..Default::default()
        });
    super::apps::McpAppSource {
        server: server.to_owned(),
        tool: tool.name.clone(),
        source: source.unwrap_or_default().to_owned(),
        declaration: tool_declaration(tool),
        ui,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mcp_apps_source_preserves_declaration_and_hides_malformed_metadata() {
        let mut tool = mcp::McpToolDefinition {
            name: "app-tool".to_owned(),
            title: None,
            description: Some("description".to_owned()),
            icons: Vec::new(),
            input_schema: json!({}),
            output_schema: None,
            annotations: None,
            meta: None,
        };
        let default = app_source("a-b", None, &tool);
        assert!(default.ui.visible_to(mcp::apps::UiVisibility::App));
        assert!(default.ui.visible_to(mcp::apps::UiVisibility::Model));
        assert!(default.ui.resource_uri.is_none());
        tool.meta = Some(json!({"ui": {"resourceUri": "ui://app", "visibility": ["app"]}}));
        let source = app_source("a-b", Some("credential-incarnation"), &tool);
        assert_eq!(source.server, "a-b");
        assert_eq!(source.tool, "app-tool");
        assert_eq!(source.source, "credential-incarnation");
        assert_eq!(source.ui.resource_uri.as_deref(), Some("ui://app"));
        assert!(source.ui.visible_to(mcp::apps::UiVisibility::App));
        assert!(!source.ui.visible_to(mcp::apps::UiVisibility::Model));
        assert_eq!(
            source.declaration,
            json!({
                "name": "app-tool", "inputSchema": {}, "outputSchema": null,
                "_meta": tool.meta, "annotations": null,
            })
        );
        for ui in [
            json!({"resourceUri": "https://invalid", "visibility": ["app"]}),
            json!({"resourceUri": "https://invalid", "visibility": []}),
            json!({"resourceUri": "https://invalid"}),
        ] {
            tool.meta = Some(json!({"ui": ui}));
            let hidden = app_source("a-b", None, &tool);
            assert!(hidden.ui.visibility.is_empty());
            assert!(hidden.ui.resource_uri.is_none());
            assert!(hidden.source.is_empty());
            assert_eq!(hidden.declaration["_meta"], tool.meta.clone().unwrap());
        }
    }
}
