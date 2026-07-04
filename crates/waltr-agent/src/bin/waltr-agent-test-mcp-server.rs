//! Stdio MCP test server used by integration tests.

use rmcp::{ErrorData as McpError, service::RequestContext};
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    handler::server::{
        router::{prompt::PromptRouter, tool::ToolRouter},
        wrapper::Parameters,
    },
    model::{
        GetPromptRequestParams, GetPromptResult, ListPromptsResult, PaginatedRequestParams,
        PromptArgument, PromptMessage, PromptMessageRole, ServerCapabilities, ServerInfo,
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
