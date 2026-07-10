use std::sync::Arc;

use mcp::{
    McpPromptArgument, McpPromptDefinition, McpPromptResult,
    server::{ServerContext, ServerError, ServerResult},
};
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::spec::support::{SpecServer, post_json};

struct PromptsServer;

#[mcp::mcp_server(
    name = "mcp-spec-prompts-server",
    version = "0",
    prompts_next_cursor = "next-prompt-page"
)]
impl PromptsServer {
    #[prompt(name = "code_review", definition = code_review_prompt_definition())]
    fn get_code_review_prompt(
        arguments: Option<Value>,
        _context: ServerContext,
    ) -> ServerResult<McpPromptResult> {
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
    }
}

fn code_review_prompt_definition() -> McpPromptDefinition {
    McpPromptDefinition {
        name: "code_review".to_owned(),
        title: Some("Request Code Review".to_owned()),
        description: Some("Asks the LLM to analyze code quality".to_owned()),
        icons: Vec::new(),
        arguments: vec![McpPromptArgument {
            name: "code".to_owned(),
            description: Some("The code to review".to_owned()),
            required: true,
        }],
        meta: None,
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
                "protocolVersion": "2025-11-25",
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
