//! MCP test server used by integration tests.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use mcp::{
    McpCompletionRequest, McpPromptArgument, McpPromptDefinition, McpPromptList, McpPromptResult,
    McpResourceDefinition, McpResourceList, McpResourceResult, McpResourceTemplateDefinition,
    McpResourceTemplateList, McpToolCall, McpToolDefinition, McpToolList, McpToolResult,
    protocol::JsonRpcNotification,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
        ServerResult, serve_stdio, streamable_http_router,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct EchoMarkerRequest {
    value: String,
}

#[derive(Debug, Deserialize)]
struct DelayedWriteRequest {
    path: String,
    value: String,
    delay_ms: u64,
}

#[derive(Debug, Deserialize)]
struct EmitLogRequest {
    message: String,
}

#[derive(Debug, Clone, Default)]
struct EchoMarkerServer {
    cancelled: Arc<Mutex<BTreeSet<String>>>,
}

impl EchoMarkerServer {
    fn cancel_request(&self, id: String) {
        if let Ok(mut cancelled) = self.cancelled.lock() {
            cancelled.insert(id);
        }
    }

    fn is_cancelled(&self, id: &str) -> bool {
        self.cancelled
            .lock()
            .map(|cancelled| cancelled.contains(id))
            .unwrap_or(false)
    }

    async fn delay_until_done_or_cancelled(&self, request_id: &str, delay_ms: u64) -> bool {
        let mut remaining = delay_ms;
        while remaining > 0 {
            if self.is_cancelled(request_id) {
                return false;
            }
            let chunk = remaining.min(25);
            tokio::time::sleep(Duration::from_millis(chunk)).await;
            remaining -= chunk;
        }
        !self.is_cancelled(request_id)
    }
}

impl ServerHandler for EchoMarkerServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("waltr-agent-test-mcp-server", env!("CARGO_PKG_VERSION"))
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new()
            .tools()
            .prompts()
            .resources()
            .completions()
            .logging()
    }

    fn instructions(&self) -> Option<String> {
        std::env::var("WALTR_TEST_MCP_INSTRUCTIONS")
            .ok()
            .filter(|instructions| !instructions.trim().is_empty())
    }

    fn list_tools(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolList>> {
        Box::pin(async {
            Ok(McpToolList {
                tools: tools_list(),
                next_cursor: None,
            })
        })
    }

    fn call_tool(
        &self,
        call: McpToolCall,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolResult>> {
        let server = self.clone();
        Box::pin(async move { server.call_tool(call, context).await })
    }

    fn set_log_level(&self, level: String, _context: ServerContext) -> BoxFuture<ServerResult<()>> {
        Box::pin(async move { set_log_level(&level) })
    }

    fn list_prompts(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpPromptList>> {
        Box::pin(async {
            Ok(McpPromptList {
                prompts: prompts_list(),
                next_cursor: None,
            })
        })
    }

    fn get_prompt(
        &self,
        name: String,
        arguments: Option<Value>,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpPromptResult>> {
        Box::pin(async move { get_prompt(&name, arguments.as_ref(), &context) })
    }

    fn list_resources(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpResourceList>> {
        Box::pin(async {
            Ok(McpResourceList {
                resources: resources_list(),
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
                resource_templates: resource_templates_list(),
                next_cursor: None,
            })
        })
    }

    fn read_resource(
        &self,
        uri: String,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpResourceResult>> {
        Box::pin(async move { read_resource(&uri, &context) })
    }

    fn complete(
        &self,
        request: McpCompletionRequest,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<mcp::McpCompletionResult>> {
        Box::pin(async move { complete(request, &context) })
    }

    fn handle_notification(
        &self,
        notification: JsonRpcNotification,
        _context: ServerContext,
    ) -> BoxFuture<()> {
        let server = self.clone();
        Box::pin(async move {
            if notification.method == "notifications/cancelled" {
                if let Some(id) = notification
                    .params
                    .as_ref()
                    .and_then(|params| params.get("requestId"))
                    .map(request_id_string)
                {
                    server.cancel_request(id);
                }
            }
        })
    }
}

impl EchoMarkerServer {
    async fn call_tool(
        &self,
        call: McpToolCall,
        context: ServerContext,
    ) -> ServerResult<McpToolResult> {
        match call.name.as_str() {
            "echo_marker" => match serde_json::from_value::<EchoMarkerRequest>(call.arguments) {
                Ok(arguments) => {
                    let mut text = arguments.value;
                    if let Some(marker) = trace_marker(&context, "tool") {
                        text.push('\n');
                        text.push_str(&marker);
                    }
                    Ok(McpToolResult::text(text))
                }
                Err(error) => Err(ServerError::invalid_params(error.to_string())),
            },
            "delayed_write_marker" => {
                let request_id = context
                    .request_id()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| call.name.clone());
                match serde_json::from_value::<DelayedWriteRequest>(call.arguments) {
                    Ok(arguments) => {
                        if !self
                            .delay_until_done_or_cancelled(&request_id, arguments.delay_ms)
                            .await
                        {
                            return Err(ServerError::internal("request cancelled"));
                        }
                        if let Err(error) = std::fs::write(arguments.path, &arguments.value) {
                            return Err(ServerError::internal(error.to_string()));
                        }
                        Ok(McpToolResult::text(arguments.value))
                    }
                    Err(error) => Err(ServerError::invalid_params(error.to_string())),
                }
            }
            "emit_log_marker" => match serde_json::from_value::<EmitLogRequest>(call.arguments) {
                Ok(arguments) => {
                    if let Err(error) = context
                        .log(
                            "warning",
                            Some("waltr.test"),
                            json!({
                                "message": arguments.message,
                                "marker": "stdio-mcp-log-notification-818"
                            }),
                        )
                        .await
                    {
                        return Err(ServerError::internal(error.to_string()));
                    }
                    Ok(McpToolResult::text(arguments.message))
                }
                Err(error) => Err(ServerError::invalid_params(error.to_string())),
            },
            "progress_marker" => {
                match serde_json::from_value::<EchoMarkerRequest>(call.arguments) {
                    Ok(arguments) => {
                        let token = call
                            .progress_token
                            .ok_or_else(|| ServerError::invalid_params("missing progress token"))?;
                        if let Err(error) = context
                            .progress(
                                token,
                                1.0,
                                Some(2.0),
                                Some("stdio-mcp-progress-marker".to_owned()),
                            )
                            .await
                        {
                            return Err(ServerError::internal(error.to_string()));
                        }
                        Ok(McpToolResult::text(arguments.value))
                    }
                    Err(error) => Err(ServerError::invalid_params(error.to_string())),
                }
            }
            _ => Err(ServerError::invalid_params("unknown test tool")),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--http") {
        let bind = args.next().unwrap_or_else(|| "127.0.0.1:0".to_owned());
        let address_file = args.next();
        serve_http(&bind, address_file.as_deref()).await?;
        return Ok(());
    }

    serve_stdio(Arc::new(EchoMarkerServer::default())).await?;
    Ok(())
}

async fn serve_http(
    bind: &str,
    address_file: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let address = listener.local_addr()?;
    if let Some(path) = address_file {
        std::fs::write(path, address.to_string())?;
    }
    axum::serve(
        listener,
        streamable_http_router(Arc::new(EchoMarkerServer::default())),
    )
    .await?;
    Ok(())
}

fn tools_list() -> Vec<McpToolDefinition> {
    vec![
        tool_definition(
            "echo_marker",
            "Echo a deterministic marker.",
            string_schema("value"),
        ),
        tool_definition(
            "delayed_write_marker",
            "Write a marker after a delay.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "value": { "type": "string" },
                    "delay_ms": { "type": "integer" }
                },
                "required": ["path", "value", "delay_ms"]
            }),
        ),
        tool_definition(
            "emit_log_marker",
            "Emit a deterministic MCP log message.",
            string_schema("message"),
        ),
        tool_definition(
            "progress_marker",
            "Emit progress and echo a deterministic marker.",
            string_schema("value"),
        ),
    ]
}

fn tool_definition(name: &str, description: &str, input_schema: Value) -> McpToolDefinition {
    McpToolDefinition {
        name: name.to_owned(),
        title: None,
        description: Some(description.to_owned()),
        input_schema,
        output_schema: None,
        annotations: None,
    }
}

fn string_schema(property: &str) -> Value {
    json!({
        "type": "object",
        "properties": { property: { "type": "string" } },
        "required": [property]
    })
}

fn trace_marker(context: &ServerContext, label: &str) -> Option<String> {
    context
        .meta_value("traceparent")
        .and_then(Value::as_str)
        .map(|_| format!("trace:{label}:present"))
}

fn set_log_level(level: &str) -> ServerResult<()> {
    if let Ok(path) = std::env::var("WALTR_TEST_MCP_LOG_LEVEL_FILE") {
        if let Err(error) = std::fs::write(path, level) {
            return Err(ServerError::internal(error.to_string()));
        }
    }
    Ok(())
}

fn prompts_list() -> Vec<McpPromptDefinition> {
    vec![
        prompt_definition(
            "status_prompt",
            "Return a deterministic status prompt.",
            Vec::new(),
        ),
        prompt_definition(
            "greet_prompt",
            "Greet a named test subject.",
            vec![McpPromptArgument {
                name: "name".to_owned(),
                description: Some("Name to greet".to_owned()),
                required: true,
            }],
        ),
        prompt_definition(
            "review.code",
            "Spec-valid prompt name outside Waltr slash syntax.",
            Vec::new(),
        ),
    ]
}

fn prompt_definition(
    name: &str,
    description: &str,
    arguments: Vec<McpPromptArgument>,
) -> McpPromptDefinition {
    McpPromptDefinition {
        name: name.to_owned(),
        title: None,
        description: Some(description.to_owned()),
        arguments,
        meta: None,
    }
}

fn get_prompt(
    name: &str,
    arguments: Option<&Value>,
    context: &ServerContext,
) -> ServerResult<McpPromptResult> {
    let mut text = match name {
        "status_prompt" => "stdio-mcp-prompt-status-313".to_owned(),
        "greet_prompt" => {
            let Some(name) = arguments
                .and_then(|arguments| arguments.get("name"))
                .and_then(Value::as_str)
            else {
                return Err(ServerError::invalid_params("missing prompt argument: name"));
            };
            format!("hello {name} from stdio-mcp-prompt-414")
        }
        "review.code" => "stdio-mcp-prompt-dotted-515".to_owned(),
        _ => return Err(ServerError::invalid_params("unknown test prompt")),
    };
    if let Some(marker) = trace_marker(context, "prompt") {
        text.push('\n');
        text.push_str(&marker);
    }
    Ok(McpPromptResult::text_user(text))
}

fn resources_list() -> Vec<McpResourceDefinition> {
    vec![McpResourceDefinition {
        uri: "test://status".to_owned(),
        name: "status_resource".to_owned(),
        title: Some("Status Resource".to_owned()),
        description: Some("Return deterministic status resource text".to_owned()),
        mime_type: Some("text/plain".to_owned()),
        size: Some(28),
        annotations: None,
        meta: None,
    }]
}

fn resource_templates_list() -> Vec<McpResourceTemplateDefinition> {
    vec![McpResourceTemplateDefinition {
        uri_template: "test://items/{id}".to_owned(),
        name: "item_resource".to_owned(),
        title: Some("Item Resource".to_owned()),
        description: Some("Return deterministic templated item resource text".to_owned()),
        mime_type: Some("text/plain".to_owned()),
        annotations: None,
        meta: None,
    }]
}

fn read_resource(uri: &str, context: &ServerContext) -> ServerResult<McpResourceResult> {
    let mut text = match uri {
        "test://status" => "stdio-mcp-resource-status-616",
        "test://items/42" => "stdio-mcp-resource-template-item-42-717",
        _ => return Err(ServerError::invalid_params("unknown test resource")),
    }
    .to_owned();
    if let Some(marker) = trace_marker(context, "resource") {
        text.push('\n');
        text.push_str(&marker);
    }
    Ok(McpResourceResult::text(uri, "text/plain", text))
}

fn complete(
    request: McpCompletionRequest,
    context: &ServerContext,
) -> ServerResult<mcp::McpCompletionResult> {
    let mut values = if request.uri_template == "test://items/{id}" && request.argument_name == "id"
    {
        ["42", "420", "4242"]
            .into_iter()
            .filter(|value| value.starts_with(&request.argument_value))
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if trace_marker(context, "completion").is_some() {
        values.push("trace:completion:present".to_owned());
    }
    Ok(mcp::McpCompletionResult {
        total: Some(values.len() as u64),
        values,
        has_more: false,
    })
}

fn request_id_string(value: &Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}
