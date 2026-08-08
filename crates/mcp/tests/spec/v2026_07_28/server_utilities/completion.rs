use std::sync::Arc;

use mcp::{
    McpCompletionReference, McpCompletionRequest, McpCompletionResult,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerHandler, ServerInfo, ServerResult,
    },
};
use serde_json::json;

use crate::spec::support::{SpecServer, modern_params, post_json};

struct CompletionServer {
    too_many: bool,
}

impl ServerHandler for CompletionServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-completion-server", "0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().completions()
    }

    fn complete(
        &self,
        request: McpCompletionRequest,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpCompletionResult>> {
        let too_many = self.too_many;
        Box::pin(async move {
            if too_many {
                return Ok(McpCompletionResult {
                    values: (0..101).map(|index| format!("value-{index}")).collect(),
                    total: Some(101),
                    has_more: false,
                });
            }
            match request.reference {
                McpCompletionReference::Resource(uri) => {
                    assert_eq!(uri, "file:///{path}");
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
                McpCompletionReference::Prompt(name) => {
                    assert_eq!(name, "code_review");
                    assert_eq!(request.argument_name, "code");
                    assert_eq!(request.argument_value, "fn");
                    Ok(McpCompletionResult {
                        values: vec!["fn main".to_owned()],
                        total: Some(1),
                        has_more: false,
                    })
                }
            }
        })
    }
}

#[tokio::test]
async fn completion_accepts_resource_reference_argument_and_context() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(CompletionServer { too_many: false })).await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "complete-resource", "method": "completion/complete",
            "params": modern_params(json!({
                "ref": { "type": "ref/resource", "uri": "file:///{path}" },
                "argument": { "name": "path", "value": "src" },
                "context": { "arguments": { "language": "rust" } }
            }))
        }),
    )
    .await?;
    assert_eq!(
        response.body["result"]["completion"],
        json!({
            "values": ["src/lib.rs", "src/main.rs"], "total": 4, "hasMore": true
        })
    );
    Ok(())
}

#[tokio::test]
async fn completion_accepts_prompt_reference() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(CompletionServer { too_many: false })).await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "complete-prompt", "method": "completion/complete",
            "params": modern_params(json!({
                "ref": { "type": "ref/prompt", "name": "code_review" },
                "argument": { "name": "code", "value": "fn" }
            }))
        }),
    )
    .await?;
    assert_eq!(
        response.body["result"]["completion"]["values"],
        json!(["fn main"])
    );
    assert_eq!(response.body["result"]["completion"]["total"], 1);
    assert_eq!(response.body["result"]["completion"]["hasMore"], false);
    Ok(())
}

#[tokio::test]
async fn completion_rejects_missing_argument_and_more_than_one_hundred_values() -> mcp::Result<()> {
    for (id, server, params) in [
        (
            "missing-argument",
            CompletionServer { too_many: false },
            modern_params(json!({
                "ref": { "type": "ref/resource", "uri": "file:///{path}" }
            })),
        ),
        (
            "too-many",
            CompletionServer { too_many: true },
            modern_params(json!({
                "ref": { "type": "ref/resource", "uri": "file:///{path}" },
                "argument": { "name": "path", "value": "src" }
            })),
        ),
    ] {
        let server = SpecServer::start(Arc::new(server)).await?;
        let response = post_json(
            &server.endpoint(),
            json!({
                "jsonrpc": "2.0", "id": id, "method": "completion/complete", "params": params
            }),
        )
        .await?;
        assert_eq!(response.body["error"]["code"], -32602);
        assert!(response.body.get("result").is_none());
    }
    Ok(())
}
