use std::sync::Arc;

use mcp::server::{ServerContext, ServerResult};
use mcp::{McpCompletionRequest, McpCompletionResult};
use reqwest::StatusCode;
use serde_json::json;

use crate::spec::support::{SpecServer, post_json};

struct CompletionServer;

struct TooManyCompletionsServer;

#[mcp::mcp_server(name = "mcp-spec-completion-server", version = "0")]
impl CompletionServer {
    #[complete]
    fn complete(
        request: McpCompletionRequest,
        _context: ServerContext,
    ) -> ServerResult<McpCompletionResult> {
        if request.uri_template == "code_review" {
            assert_eq!(request.argument_name, "code");
            assert_eq!(request.argument_value, "fn");
            return Ok(McpCompletionResult {
                values: vec!["fn main".to_owned()],
                total: Some(1),
                has_more: false,
            });
        }

        assert_eq!(request.uri_template, "file:///{path}");
        assert_eq!(request.argument_name, "path");
        assert_eq!(request.argument_value, "src");
        assert_eq!(
            request.context_arguments.get("language"),
            Some(&json!("rust"))
        );
        Ok(McpCompletionResult {
            values: vec!["src/lib.rs".to_owned(), "src/main.rs".to_owned()],
            total: Some(4),
            has_more: true,
        })
    }
}

#[mcp::mcp_server(name = "mcp-spec-completion-limit-server", version = "0")]
impl TooManyCompletionsServer {
    #[complete]
    fn complete(
        _request: McpCompletionRequest,
        _context: ServerContext,
    ) -> ServerResult<McpCompletionResult> {
        Ok(McpCompletionResult {
            values: (0..101).map(|index| format!("value-{index}")).collect(),
            total: Some(101),
            has_more: false,
        })
    }
}

#[tokio::test]
async fn completion_complete_accepts_resource_ref_argument_context_and_returns_completion()
-> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(CompletionServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "complete-1",
            "method": "completion/complete",
            "params": {
                "ref": { "type": "ref/resource", "uri": "file:///{path}" },
                "argument": { "name": "path", "value": "src" },
                "context": { "arguments": { "language": "rust" } }
            }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["jsonrpc"], "2.0");
    assert_eq!(response.body["id"], "complete-1");
    assert_eq!(
        response.body["result"],
        json!({
            "completion": {
                "values": ["src/lib.rs", "src/main.rs"],
                "total": 4,
                "hasMore": true
            }
        })
    );
    assert!(response.body.get("error").is_none());

    Ok(())
}

#[tokio::test]
async fn completion_complete_rejects_missing_required_argument_as_invalid_params() -> mcp::Result<()>
{
    let server = SpecServer::start(Arc::new(CompletionServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "complete-missing-argument",
            "method": "completion/complete",
            "params": {
                "ref": { "type": "ref/resource", "uri": "file:///{path}" }
            }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["jsonrpc"], "2.0");
    assert_eq!(response.body["id"], "complete-missing-argument");
    assert_eq!(response.body["error"]["code"], -32602);
    assert!(response.body.get("result").is_none());

    Ok(())
}

#[tokio::test]
async fn completion_complete_accepts_prompt_ref_argument() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(CompletionServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "complete-prompt-ref",
            "method": "completion/complete",
            "params": {
                "ref": { "type": "ref/prompt", "name": "code_review" },
                "argument": { "name": "code", "value": "fn" }
            }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["id"], "complete-prompt-ref");
    assert_eq!(
        response.body["result"]["completion"]["values"],
        json!(["fn main"])
    );
    assert!(response.body.get("error").is_none());

    Ok(())
}

#[tokio::test]
async fn completion_complete_rejects_more_than_one_hundred_values() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TooManyCompletionsServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "complete-too-many",
            "method": "completion/complete",
            "params": {
                "ref": { "type": "ref/resource", "uri": "file:///{path}" },
                "argument": { "name": "path", "value": "src" }
            }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["id"], "complete-too-many");
    assert_eq!(response.body["error"]["code"], -32602);
    assert!(response.body.get("result").is_none());

    Ok(())
}
