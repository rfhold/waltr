use std::{collections::BTreeMap, sync::Arc};

use serde_json::{Map, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use mcp::{
    McpCompletionResult, McpPromptDefinition, McpResourceDefinition, McpResourceTemplateDefinition,
};

use crate::{AgentTool, LlmError, Result, ToolDefinition, ToolProgressSink};

use super::{
    config::McpConfig, event::McpRuntimeEvent, name::direct_tool_name, server::McpServerRuntime,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpServerPromptDefinition {
    pub(crate) server_name: String,
    pub(crate) prompt: McpPromptDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpServerResourceDefinition {
    pub(crate) server_name: String,
    pub(crate) resource: McpResourceDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpServerResourceTemplateDefinition {
    pub(crate) server_name: String,
    pub(crate) resource_template: McpResourceTemplateDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpServerInstructions {
    pub(crate) server_name: String,
    pub(crate) instructions: String,
}

pub(crate) struct McpClientRuntime {
    servers: BTreeMap<String, Arc<McpServerRuntime>>,
    #[allow(dead_code)]
    events: broadcast::Sender<McpRuntimeEvent>,
}

#[derive(Debug)]
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

impl McpClientRuntime {
    pub(crate) async fn start(
        config: McpConfig,
        root_uri: String,
        cancellation_token: CancellationToken,
    ) -> std::result::Result<Arc<Self>, McpRuntimeStartError> {
        let (servers, timeout, log_level, trace_propagation) = config.into_parts();
        let (events, _) = broadcast::channel(128);
        let mut active = BTreeMap::new();
        for server in servers {
            let server_key = server.name.clone();
            let runtime = McpServerRuntime::start(
                server,
                timeout,
                log_level,
                trace_propagation.clone(),
                root_uri.clone(),
                events.clone(),
                cancellation_token.clone(),
            )
            .await
            .map_err(|error| McpRuntimeStartError { server_key, error })?;
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
                tools.push(AgentTool::new_async(definition, move |context| {
                    let runtime = Arc::clone(&runtime);
                    let server_name = closure_server_name.clone();
                    let tool_name = tool_name.clone();
                    let arguments = context.call().arguments.clone();
                    let call_id = context.call().id.clone();
                    let progress = context.progress().clone();
                    let cancellation_token = context.cancellation_token().clone();
                    Box::pin(async move {
                        let content = runtime
                            .call_tool(
                                &server_name,
                                &tool_name,
                                arguments,
                                &call_id,
                                progress,
                                cancellation_token,
                            )
                            .await?;
                        Ok(context.result(content))
                    })
                }));
            }
        }
        tools
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
    ) -> Result<String> {
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
