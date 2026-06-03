use std::{collections::HashMap, sync::Arc, time::Duration};

use serde_json::Value;

use crate::{AgentTool, Result, ToolDefinition};

use super::{
    config::McpServerConfig, name::direct_tool_name, protocol::protocol_error, server::McpServer,
};

pub(in crate::mcp) struct McpRuntime {
    servers: HashMap<String, Arc<McpServer>>,
}

impl McpRuntime {
    pub(in crate::mcp) fn new(configs: Vec<McpServerConfig>, timeout: Duration) -> Self {
        Self {
            servers: configs
                .into_iter()
                .map(|config| {
                    let name = config.name.clone();
                    (name, Arc::new(McpServer::new(config, timeout)))
                })
                .collect(),
        }
    }

    pub(in crate::mcp) async fn direct_agent_tools(self: &Arc<Self>) -> Result<Vec<AgentTool>> {
        let mut tools = Vec::new();
        for server in self.servers.values() {
            for tool in server.list_tools().await? {
                let runtime = Arc::clone(self);
                let server_name = server.name().to_owned();
                let tool_name = tool.name.clone();
                let definition = ToolDefinition::new(
                    direct_tool_name(&server_name, &tool_name),
                    format!(
                        "MCP tool `{tool_name}` from server `{server_name}`. {}",
                        tool.description
                    ),
                    tool.input_schema.clone(),
                );
                tools.push(AgentTool::with_context_async(
                    definition,
                    move |call, _context| {
                        let runtime = Arc::clone(&runtime);
                        let server_name = server_name.clone();
                        let tool_name = tool_name.clone();
                        Box::pin(async move {
                            runtime
                                .call_tool(&server_name, &tool_name, call.arguments.clone())
                                .await
                        })
                    },
                ));
            }
        }
        Ok(tools)
    }

    async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<String> {
        self.server(server_name)?
            .call_tool(tool_name, arguments)
            .await
    }

    fn server(&self, server_name: &str) -> Result<Arc<McpServer>> {
        self.servers
            .get(server_name)
            .cloned()
            .ok_or_else(|| protocol_error(format!("unknown MCP server `{server_name}`")))
    }
}
