use std::sync::Arc;

use mcp::{
    McpResourceDefinition, McpResourceList, McpResourceResult,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
        ServerResult,
    },
};
use serde_json::json;

use crate::spec::support::{SpecServer, modern_params, post_json};

struct ResourcesServer;

impl ServerHandler for ResourcesServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-resources-server", "0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().resources()
    }

    fn list_resources(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpResourceList>> {
        Box::pin(async {
            Ok(McpResourceList {
                resources: vec![McpResourceDefinition {
                    uri: "file:///project/src/main.rs".to_owned(),
                    name: "main.rs".to_owned(),
                    title: Some("Rust Software Application Main File".to_owned()),
                    description: Some("Primary application entry point".to_owned()),
                    mime_type: Some("text/x-rust".to_owned()),
                    icons: Vec::new(),
                    size: Some(42),
                    annotations: Some(json!({ "audience": ["assistant"], "priority": 0.8 })),
                    meta: Some(json!({ "example.test/resource": "opaque" })),
                }],
                next_cursor: Some("next-resource-page".to_owned()),
            })
        })
    }

    fn read_resource(
        &self,
        uri: String,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpResourceResult>> {
        Box::pin(async move {
            if uri != "file:///project/src/main.rs" {
                return Err(ServerError::resource_not_found(uri));
            }
            Ok(McpResourceResult::new(json!({
                "contents": [
                    {
                        "uri": "file:///project/src/main.rs", "mimeType": "text/x-rust",
                        "text": "fn main() {}"
                    },
                    {
                        "uri": "file:///project/image.png", "mimeType": "image/png",
                        "blob": "aW1hZ2U="
                    }
                ]
            })))
        })
    }
}

#[tokio::test]
async fn resources_list_returns_metadata_annotations_and_cursor() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ResourcesServer)).await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "resources-list", "method": "resources/list",
            "params": modern_params(json!({ "cursor": "first-page" }))
        }),
    )
    .await?;

    let resource = &response.body["result"]["resources"][0];
    assert_eq!(resource["uri"], "file:///project/src/main.rs");
    assert_eq!(resource["name"], "main.rs");
    assert_eq!(resource["title"], "Rust Software Application Main File");
    assert_eq!(resource["mimeType"], "text/x-rust");
    assert_eq!(resource["size"], 42);
    assert_eq!(resource["annotations"]["audience"], json!(["assistant"]));
    assert_eq!(resource["_meta"]["example.test/resource"], "opaque");
    assert_eq!(response.body["result"]["nextCursor"], "next-resource-page");
    Ok(())
}

#[tokio::test]
async fn resources_read_returns_text_and_blob_contents() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ResourcesServer)).await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "resources-read", "method": "resources/read",
            "params": modern_params(json!({ "uri": "file:///project/src/main.rs" }))
        }),
    )
    .await?;

    assert_eq!(
        response.body["result"]["contents"],
        json!([
            {
                "uri": "file:///project/src/main.rs", "mimeType": "text/x-rust",
                "text": "fn main() {}"
            },
            {
                "uri": "file:///project/image.png", "mimeType": "image/png",
                "blob": "aW1hZ2U="
            }
        ])
    );
    Ok(())
}

#[tokio::test]
async fn resources_read_missing_or_unknown_uri_returns_invalid_params() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ResourcesServer)).await?;
    for (id, params) in [
        ("missing", modern_params(json!({}))),
        (
            "unknown",
            modern_params(json!({ "uri": "file:///project/missing.rs" })),
        ),
    ] {
        let response = post_json(
            &server.endpoint(),
            json!({
                "jsonrpc": "2.0", "id": id, "method": "resources/read", "params": params
            }),
        )
        .await?;
        assert_eq!(response.body["error"]["code"], -32602);
    }
    Ok(())
}
