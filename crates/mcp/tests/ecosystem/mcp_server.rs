use std::net::SocketAddr;

use mcp::{
    McpCompletionRequest, McpPromptList, McpPromptResult, McpResourceList, McpResourceResult,
    McpResourceTemplateList, McpToolCall, McpToolList, McpToolResult,
    client::McpTransportConfig,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
        ServerResult,
    },
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};

use super::{
    standard_surface::{
        ECHO_TOOL, GREET_PROMPT, ITEM_42, ITEM_TEMPLATE, LOG_TOOL, PROGRESS_TOOL, STATUS_PROMPT,
        STATUS_RESOURCE, item_completions, prompts, resource_templates, resources, tools,
    },
    support::RunningServer,
};

pub struct McpStreamableHttpServer {
    address: SocketAddr,
    _task: JoinHandle<()>,
}

impl McpStreamableHttpServer {
    pub async fn start() -> mcp::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = mcp::server::streamable_http_router(std::sync::Arc::new(StandardMcpServer));
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::debug!(%error, "mcp ecosystem server stopped");
            }
        });
        Ok(Self {
            address,
            _task: task,
        })
    }
}

impl RunningServer for McpStreamableHttpServer {
    fn transport(&self) -> McpTransportConfig {
        McpTransportConfig::StreamableHttp {
            endpoint: format!("http://{}/mcp", self.address),
        }
    }
}

struct StandardMcpServer;

impl ServerHandler for StandardMcpServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-ecosystem-server", "0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new()
            .tools()
            .prompts()
            .resources()
            .completions()
            .logging()
    }

    fn list_tools(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolList>> {
        Box::pin(async {
            Ok(McpToolList {
                tools: tools(),
                next_cursor: None,
            })
        })
    }

    fn call_tool(
        &self,
        call: McpToolCall,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolResult>> {
        Box::pin(async move { call_tool(call, context).await })
    }

    fn set_log_level(
        &self,
        _level: String,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn list_prompts(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpPromptList>> {
        Box::pin(async {
            Ok(McpPromptList {
                prompts: prompts(),
                next_cursor: None,
            })
        })
    }

    fn get_prompt(
        &self,
        name: String,
        arguments: Option<Value>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpPromptResult>> {
        Box::pin(async move { get_prompt(&name, arguments.as_ref()) })
    }

    fn list_resources(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpResourceList>> {
        Box::pin(async {
            Ok(McpResourceList {
                resources: resources(),
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
                resource_templates: resource_templates(),
                next_cursor: None,
            })
        })
    }

    fn read_resource(
        &self,
        uri: String,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpResourceResult>> {
        Box::pin(async move { read_resource(&uri) })
    }

    fn complete(
        &self,
        request: McpCompletionRequest,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<mcp::McpCompletionResult>> {
        Box::pin(async move { complete(request) })
    }
}

async fn call_tool(call: McpToolCall, context: ServerContext) -> ServerResult<McpToolResult> {
    match call.name.as_str() {
        ECHO_TOOL => Ok(McpToolResult::text(
            call.arguments
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )),
        LOG_TOOL => {
            let message = call
                .arguments
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            context
                .log(
                    "warning",
                    Some("ecosystem.test"),
                    json!({ "message": message, "marker": "ecosystem-log-notification-818" }),
                )
                .await
                .map_err(|error| ServerError::internal(error.to_string()))?;
            Ok(McpToolResult::text(message))
        }
        PROGRESS_TOOL => {
            let token = call
                .progress_token
                .ok_or_else(|| ServerError::invalid_params("missing progress token"))?;
            context
                .progress(
                    token,
                    1.0,
                    Some(2.0),
                    Some("ecosystem-progress-marker".to_owned()),
                )
                .await
                .map_err(|error| ServerError::internal(error.to_string()))?;
            Ok(McpToolResult::text(
                call.arguments
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ))
        }
        _ => Err(ServerError::invalid_params("unknown test tool")),
    }
}

fn get_prompt(name: &str, arguments: Option<&Value>) -> ServerResult<McpPromptResult> {
    let text = match name {
        STATUS_PROMPT => "ecosystem-prompt-status-313".to_owned(),
        GREET_PROMPT => {
            let name = arguments
                .and_then(|arguments| arguments.get("name"))
                .and_then(Value::as_str)
                .ok_or_else(|| ServerError::invalid_params("missing prompt argument: name"))?;
            format!("hello {name} from ecosystem-prompt-414")
        }
        _ => return Err(ServerError::invalid_params("unknown test prompt")),
    };
    Ok(McpPromptResult::text_user(text))
}

fn read_resource(uri: &str) -> ServerResult<McpResourceResult> {
    match uri {
        STATUS_RESOURCE => Ok(McpResourceResult::text(
            uri,
            "text/plain",
            "ecosystem-resource-status-616",
        )),
        ITEM_42 => Ok(McpResourceResult::text(
            uri,
            "text/plain",
            "ecosystem-resource-template-item-42-717",
        )),
        _ => Err(ServerError::invalid_params("unknown test resource")),
    }
}

fn complete(request: McpCompletionRequest) -> ServerResult<mcp::McpCompletionResult> {
    let values = if request.uri_template == ITEM_TEMPLATE && request.argument_name == "id" {
        item_completions(&request.argument_value)
    } else {
        Vec::new()
    };
    Ok(mcp::McpCompletionResult {
        total: Some(values.len() as u64),
        values,
        has_more: false,
    })
}
