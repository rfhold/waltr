//! Stdio MCP test server used by integration tests.

use rmcp::{ErrorData as McpError, service::RequestContext};
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    handler::server::{
        router::{prompt::PromptRouter, tool::ToolRouter},
        wrapper::Parameters,
    },
    model::{
        Annotated, CompleteRequestParams, CompleteResult, CompletionInfo, GetPromptRequestParams,
        GetPromptResult, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
        LoggingLevel, LoggingMessageNotificationParam, PaginatedRequestParams,
        ProgressNotificationParam, PromptArgument, PromptMessage, PromptMessageRole, RawResource,
        RawResourceTemplate, ReadResourceRequestParams, ReadResourceResult, Reference,
        ResourceContents, ServerCapabilities, ServerInfo, SetLevelRequestParams,
    },
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use rmcp::{prompt, prompt_router};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoMarkerRequest {
    value: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DelayedWriteRequest {
    path: String,
    value: String,
    delay_ms: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EmitLogRequest {
    message: String,
}

#[derive(Debug, Clone)]
struct EchoMarkerServer {
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

impl EchoMarkerServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }
}

#[tool_router]
impl EchoMarkerServer {
    #[tool(name = "echo_marker", description = "Echo a deterministic marker.")]
    fn echo_marker(
        &self,
        Parameters(EchoMarkerRequest { value }): Parameters<EchoMarkerRequest>,
    ) -> String {
        let _ = &self.tool_router;
        value
    }

    #[tool(
        name = "delayed_write_marker",
        description = "Write a marker after a delay."
    )]
    fn delayed_write_marker(
        &self,
        Parameters(DelayedWriteRequest {
            path,
            value,
            delay_ms,
        }): Parameters<DelayedWriteRequest>,
    ) -> String {
        let _ = &self.tool_router;
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        std::fs::write(path, &value).expect("delayed marker write should succeed");
        value
    }

    #[tool(
        name = "emit_log_marker",
        description = "Emit a deterministic MCP log message."
    )]
    async fn emit_log_marker(
        &self,
        Parameters(EmitLogRequest { message }): Parameters<EmitLogRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, McpError> {
        let _ = &self.tool_router;
        context
            .peer
            .notify_logging_message(LoggingMessageNotificationParam {
                level: LoggingLevel::Warning,
                logger: Some("waltr.test".to_owned()),
                data: serde_json::json!({
                    "message": message,
                    "marker": "stdio-mcp-log-notification-818"
                }),
            })
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(message)
    }

    #[tool(
        name = "progress_marker",
        description = "Emit progress and echo a deterministic marker."
    )]
    async fn progress_marker(
        &self,
        Parameters(EchoMarkerRequest { value }): Parameters<EchoMarkerRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, McpError> {
        let _ = &self.tool_router;
        let token = context
            .meta
            .get_progress_token()
            .ok_or_else(|| McpError::invalid_params("missing progress token", None))?;
        context
            .peer
            .notify_progress(
                ProgressNotificationParam::new(token, 1.0)
                    .with_total(2.0)
                    .with_message("stdio-mcp-progress-marker"),
            )
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(value)
    }
}

#[prompt_router]
impl EchoMarkerServer {
    #[prompt(
        name = "status_prompt",
        description = "Return a deterministic status prompt."
    )]
    fn status_prompt(&self) -> Vec<PromptMessage> {
        let _ = &self.prompt_router;
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            "stdio-mcp-prompt-status-313",
        )]
    }

    #[prompt(name = "greet_prompt", description = "Greet a named test subject.")]
    fn greet_prompt(
        &self,
        Parameters(GreetPromptRequest { name }): Parameters<GreetPromptRequest>,
    ) -> Vec<PromptMessage> {
        let _ = &self.prompt_router;
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            format!("hello {name} from stdio-mcp-prompt-414"),
        )]
    }

    #[prompt(
        name = "review.code",
        description = "Spec-valid prompt name outside Waltr slash syntax."
    )]
    fn dotted_prompt(&self) -> Vec<PromptMessage> {
        let _ = &self.prompt_router;
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            "stdio-mcp-prompt-dotted-515",
        )]
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GreetPromptRequest {
    name: String,
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EchoMarkerServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_logging()
                .enable_completions()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        )
        .with_instructions("Echo deterministic test markers")
    }

    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        if let Ok(path) = std::env::var("WALTR_TEST_MCP_LOG_LEVEL_FILE") {
            std::fs::write(path, format!("{:?}", request.level).to_lowercase())
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        }
        Ok(())
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        let Reference::Resource(reference) = request.r#ref else {
            return Ok(CompleteResult::default());
        };
        if reference.uri != "test://items/{id}" || request.argument.name != "id" {
            return Ok(CompleteResult::default());
        }

        let values = ["42", "420", "4242"]
            .into_iter()
            .filter(|value| value.starts_with(&request.argument.value))
            .map(ToOwned::to_owned)
            .collect();
        let completion = CompletionInfo::with_all_values(values)
            .map_err(|error| McpError::internal_error(error, None))?;
        Ok(CompleteResult::new(completion))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        let mut result = ListPromptsResult::default();
        result.prompts = self.prompt_router.list_all();
        if let Some(prompt) = result
            .prompts
            .iter_mut()
            .find(|prompt| prompt.name == "greet_prompt")
        {
            prompt.arguments = Some(vec![
                PromptArgument::new("name")
                    .with_description("Name to greet")
                    .with_required(true),
            ]);
        }
        Ok(result)
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        let prompt_context = rmcp::handler::server::prompt::PromptContext::new(
            self,
            request.name,
            request.arguments,
            context,
        );
        self.prompt_router.get_prompt(prompt_context).await
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(vec![Annotated::new(
            RawResource::new("test://status", "status_resource")
                .with_title("Status Resource")
                .with_description("Return deterministic status resource text")
                .with_mime_type("text/plain")
                .with_size(28),
            None,
        )]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            Annotated::new(
                RawResourceTemplate::new("test://items/{id}", "item_resource")
                    .with_title("Item Resource")
                    .with_description("Return deterministic templated item resource text")
                    .with_mime_type("text/plain"),
                None,
            ),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        match request.uri.as_str() {
            "test://status" => Ok(ReadResourceResult::new(vec![ResourceContents::text(
                "stdio-mcp-resource-status-616",
                request.uri,
            )])),
            "test://items/42" => Ok(ReadResourceResult::new(vec![ResourceContents::text(
                "stdio-mcp-resource-template-item-42-717",
                request.uri,
            )])),
            _ => Err(McpError::invalid_params("unknown test resource", None)),
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

    EchoMarkerServer::new()
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
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
    let service: StreamableHttpService<EchoMarkerServer, LocalSessionManager> =
        StreamableHttpService::new(
            || Ok(EchoMarkerServer::new()),
            std::sync::Arc::default(),
            StreamableHttpServerConfig::default().with_sse_keep_alive(None),
        );
    let app = axum::Router::new().nest_service("/mcp", service);
    axum::serve(listener, app).await?;
    Ok(())
}
