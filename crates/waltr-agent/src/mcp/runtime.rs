use std::{collections::BTreeMap, sync::Arc};

use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::{AgentTool, Result, ToolDefinition};

use super::{
    config::McpConfig, event::McpRuntimeEvent, name::direct_tool_name, protocol::protocol_error,
    server::McpServerRuntime,
};

pub(crate) struct McpClientRuntime {
    servers: BTreeMap<String, Arc<McpServerRuntime>>,
    #[allow(dead_code)]
    events: broadcast::Sender<McpRuntimeEvent>,
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
    ) -> Result<Arc<Self>> {
        let (servers, timeout) = config.into_parts();
        let (events, _) = broadcast::channel(128);
        let mut active = BTreeMap::new();
        for server in servers {
            let runtime = McpServerRuntime::start(
                server,
                timeout,
                root_uri.clone(),
                events.clone(),
                cancellation_token.clone(),
            )
            .await?;
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
                    Box::pin(async move {
                        let content = runtime
                            .call_tool(
                                &server_name,
                                &tool_name,
                                arguments,
                                context.cancellation_token().clone(),
                            )
                            .await?;
                        Ok(context.result(content))
                    })
                }));
            }
        }
        tools
    }

    async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> Result<String> {
        self.servers
            .get(server_name)
            .ok_or_else(|| protocol_error(format!("unknown MCP server `{server_name}`")))?
            .call_tool(tool_name, arguments, cancellation_token)
            .await
    }

    pub(crate) async fn shutdown(&self) {
        for server in self.servers.values() {
            server.shutdown().await;
        }
    }
}
