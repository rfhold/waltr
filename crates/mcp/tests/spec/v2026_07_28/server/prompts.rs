use std::sync::Arc;

use mcp::{
    McpPromptArgument, McpPromptDefinition, McpPromptList, McpPromptResult,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
        ServerResult,
    },
};
use serde_json::{Value, json};

use crate::spec::support::{SpecServer, modern_params, post_json};

struct PromptsServer;

impl ServerHandler for PromptsServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-prompts-server", "0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().prompts()
    }

    fn list_prompts(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpPromptList>> {
        Box::pin(async {
            Ok(McpPromptList {
                prompts: vec![McpPromptDefinition {
                    name: "code_review".to_owned(),
                    title: Some("Request Code Review".to_owned()),
                    description: Some("Asks the LLM to analyze code quality".to_owned()),
                    icons: Vec::new(),
                    arguments: vec![McpPromptArgument {
                        name: "code".to_owned(),
                        title: Some("Source Code".to_owned()),
                        description: Some("The code to review".to_owned()),
                        required: true,
                    }],
                    meta: Some(json!({ "example.test/prompt": "opaque" })),
                }],
                next_cursor: Some("next-prompt-page".to_owned()),
            })
        })
    }

    fn get_prompt(
        &self,
        name: String,
        arguments: Option<Value>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpPromptResult>> {
        Box::pin(async move {
            if name != "code_review" {
                return Err(ServerError::invalid_params("unknown prompt"));
            }
            let code = arguments
                .as_ref()
                .and_then(|arguments| arguments.get("code"))
                .and_then(Value::as_str)
                .ok_or_else(|| ServerError::invalid_params("missing required argument: code"))?;
            Ok(McpPromptResult::new(json!({
                "description": "Code review prompt",
                "messages": [{
                    "role": "user",
                    "content": { "type": "text", "text": format!("Please review this code:\n{code}") }
                }]
            })))
        })
    }
}

#[tokio::test]
async fn prompts_list_returns_arguments_metadata_and_cursor() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(PromptsServer)).await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "prompts-list", "method": "prompts/list",
            "params": modern_params(json!({ "cursor": "first-page" }))
        }),
    )
    .await?;

    let prompt = &response.body["result"]["prompts"][0];
    assert_eq!(prompt["name"], "code_review");
    assert_eq!(prompt["title"], "Request Code Review");
    assert_eq!(
        prompt["arguments"][0],
        json!({
            "name": "code", "title": "Source Code",
            "description": "The code to review", "required": true
        })
    );
    assert_eq!(prompt["_meta"]["example.test/prompt"], "opaque");
    assert_eq!(response.body["result"]["nextCursor"], "next-prompt-page");
    Ok(())
}

#[tokio::test]
async fn prompts_get_returns_description_and_messages() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(PromptsServer)).await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "prompts-get", "method": "prompts/get",
            "params": modern_params(json!({
                "name": "code_review", "arguments": { "code": "fn main() {}" }
            }))
        }),
    )
    .await?;

    assert_eq!(response.body["result"]["description"], "Code review prompt");
    assert_eq!(response.body["result"]["messages"][0]["role"], "user");
    assert_eq!(
        response.body["result"]["messages"][0]["content"],
        json!({ "type": "text", "text": "Please review this code:\nfn main() {}" })
    );
    Ok(())
}

#[tokio::test]
async fn prompts_get_missing_required_argument_returns_invalid_params() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(PromptsServer)).await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "prompts-get-invalid", "method": "prompts/get",
            "params": modern_params(json!({ "name": "code_review", "arguments": {} }))
        }),
    )
    .await?;
    assert_eq!(response.body["error"]["code"], -32602);
    Ok(())
}
