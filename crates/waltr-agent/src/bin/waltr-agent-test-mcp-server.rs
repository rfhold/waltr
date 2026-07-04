//! Stdio MCP test server used by integration tests.

use rmcp::{ErrorData as McpError, service::RequestContext};
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    handler::server::{
        router::{prompt::PromptRouter, tool::ToolRouter},
        wrapper::Parameters,
    },
    model::{
        Annotated, GetPromptRequestParams, GetPromptResult, ListPromptsResult, ListResourcesResult,
        PaginatedRequestParams, PromptArgument, PromptMessage, PromptMessageRole, RawResource,
        ReadResourceRequestParams, ReadResourceResult, ResourceContents, ServerCapabilities,
        ServerInfo,
    },
    schemars, tool, tool_handler, tool_router,
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
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        )
        .with_instructions("Echo deterministic test markers")
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
            _ => Err(McpError::invalid_params("unknown test resource", None)),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    EchoMarkerServer::new()
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
