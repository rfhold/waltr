// Macro fixtures intentionally retain the owned and fallible handler signatures under test.
#![allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]

use std::net::SocketAddr;

use mcp::{
    McpCompletionRequest, McpPromptResult, McpResourceResult, McpToolCall, McpToolResult,
    client::McpTransportConfig,
    server::{ServerContext, ServerError, ServerResult},
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};

use super::{
    standard_surface::{
        ECHO_TOOL, GREET_PROMPT, ITEM_42, ITEM_TEMPLATE, LOG_TOOL, PROGRESS_TOOL, STATUS_PROMPT,
        STATUS_RESOURCE, echo_tool, greet_prompt, item_completions, item_resource_template,
        log_tool, progress_tool, status_prompt, status_resource,
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
        let router = StandardMcpServer.streamable_http_router();
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
            bearer_token: None,
        }
    }
}

#[derive(Clone, Copy)]
struct StandardMcpServer;

#[mcp::mcp_server(name = "mcp-ecosystem-server", version = "0")]
impl StandardMcpServer {
    #[tool(name = ECHO_TOOL, definition = echo_tool())]
    #[tool(name = LOG_TOOL, definition = log_tool())]
    #[tool(name = PROGRESS_TOOL, definition = progress_tool())]
    async fn call_tool(call: McpToolCall, context: ServerContext) -> ServerResult<McpToolResult> {
        call_tool(call, context).await
    }

    #[log_level]
    fn set_log_level(_level: String, _context: ServerContext) -> ServerResult<()> {
        Ok(())
    }

    #[prompt(name = STATUS_PROMPT, definition = status_prompt())]
    fn get_status_prompt(
        _arguments: Option<Value>,
        _context: ServerContext,
    ) -> ServerResult<McpPromptResult> {
        get_prompt(STATUS_PROMPT, None)
    }

    #[prompt(name = GREET_PROMPT, definition = greet_prompt())]
    fn get_greet_prompt(
        arguments: Option<Value>,
        _context: ServerContext,
    ) -> ServerResult<McpPromptResult> {
        get_prompt(GREET_PROMPT, arguments.as_ref())
    }

    #[resource(uri = STATUS_RESOURCE, definition = status_resource())]
    #[resource_template(uri_template = ITEM_TEMPLATE, definition = item_resource_template())]
    fn read_resource(uri: String, _context: ServerContext) -> ServerResult<McpResourceResult> {
        read_resource(&uri)
    }

    #[complete]
    fn complete_item(
        request: McpCompletionRequest,
        _context: ServerContext,
    ) -> ServerResult<mcp::McpCompletionResult> {
        complete(request)
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
