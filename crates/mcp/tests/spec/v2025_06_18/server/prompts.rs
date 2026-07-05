use std::sync::Arc;

use mcp::{
    McpPromptArgument, McpPromptDefinition, McpPromptList, McpPromptResult,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
    },
};
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::spec::support::{SpecServer, post_json};

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
    ) -> BoxFuture<mcp::server::ServerResult<McpPromptList>> {
        Box::pin(async {
            Ok(McpPromptList {
                prompts: vec![McpPromptDefinition {
                    name: "code_review".to_owned(),
                    title: Some("Request Code Review".to_owned()),
                    description: Some("Asks the LLM to analyze code quality".to_owned()),
                    arguments: vec![McpPromptArgument {
                        name: "code".to_owned(),
                        description: Some("The code to review".to_owned()),
                        required: true,
                    }],
                    meta: None,
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
    ) -> BoxFuture<mcp::server::ServerResult<McpPromptResult>> {
        Box::pin(async move {
            if name != "code_review" {
                return Err(ServerError::invalid_params(format!(
                    "Invalid prompt name: {name}"
                )));
            }
            let code = arguments
                .as_ref()
                .and_then(|arguments| arguments.get("code"))
                .and_then(Value::as_str)
                .ok_or_else(|| ServerError::invalid_params("Missing required argument: code"))?;

            Ok(McpPromptResult::new(json!({
                "description": "Code review prompt",
                "messages": [{
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": format!("Please review this code:\n{code}")
                    }
                }]
            })))
        })
    }
}

#[tokio::test]
async fn initialize_declares_prompts_capability() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(PromptsServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "initialize-prompts",
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "spec-test", "version": "0" }
            }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body["result"]["capabilities"]["prompts"]["listChanged"],
        false
    );
    Ok(())
}

#[tokio::test]
async fn prompts_list_returns_prompt_arguments_and_next_cursor() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(PromptsServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "prompts-list",
            "method": "prompts/list",
            "params": { "cursor": "first-page" }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["result"]["prompts"][0]["name"], "code_review");
    assert_eq!(
        response.body["result"]["prompts"][0]["title"],
        "Request Code Review"
    );
    assert_eq!(
        response.body["result"]["prompts"][0]["arguments"][0],
        json!({
            "name": "code",
            "required": true,
            "description": "The code to review"
        })
    );
    assert_eq!(response.body["result"]["nextCursor"], "next-prompt-page");
    assert!(response.body.get("error").is_none());
    Ok(())
}

#[tokio::test]
async fn prompts_get_returns_description_and_prompt_messages() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(PromptsServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "prompts-get",
            "method": "prompts/get",
            "params": {
                "name": "code_review",
                "arguments": { "code": "fn main() {}" }
            }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["result"]["description"], "Code review prompt");
    assert_eq!(response.body["result"]["messages"][0]["role"], "user");
    assert_eq!(
        response.body["result"]["messages"][0]["content"]["type"],
        "text"
    );
    assert_eq!(
        response.body["result"]["messages"][0]["content"]["text"],
        "Please review this code:\nfn main() {}"
    );
    assert!(response.body.get("error").is_none());
    Ok(())
}

#[tokio::test]
async fn prompts_get_missing_required_argument_returns_invalid_params_error() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(PromptsServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "prompts-get-invalid",
            "method": "prompts/get",
            "params": { "name": "code_review", "arguments": {} }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["error"]["code"], -32602);
    assert!(response.body.get("result").is_none());
    Ok(())
}
