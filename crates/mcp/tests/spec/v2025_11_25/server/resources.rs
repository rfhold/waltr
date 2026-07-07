use std::sync::Arc;

use mcp::{
    McpResourceDefinition, McpResourceList, McpResourceResult,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
    },
};
use reqwest::StatusCode;
use serde_json::json;

use crate::spec::support::{SpecServer, post_json};

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
    ) -> BoxFuture<mcp::server::ServerResult<McpResourceList>> {
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
                    annotations: None,
                    meta: None,
                }],
                next_cursor: Some("next-resource-page".to_owned()),
            })
        })
    }

    fn read_resource(
        &self,
        uri: String,
        _context: ServerContext,
    ) -> BoxFuture<mcp::server::ServerResult<McpResourceResult>> {
        Box::pin(async move {
            if uri != "file:///project/src/main.rs" {
                return Err(ServerError::resource_not_found(uri));
            }
            Ok(McpResourceResult::new(json!({
                "contents": [
                    {
                        "uri": "file:///project/src/main.rs",
                        "mimeType": "text/x-rust",
                        "text": "fn main() {}"
                    },
                    {
                        "uri": "file:///project/image.png",
                        "mimeType": "image/png",
                        "blob": "aW1hZ2U="
                    }
                ]
            })))
        })
    }
}

#[tokio::test]
async fn initialize_declares_resources_capability() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ResourcesServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "initialize-resources",
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
        response.body["result"]["capabilities"]["resources"]["subscribe"],
        false
    );
    assert_eq!(
        response.body["result"]["capabilities"]["resources"]["listChanged"],
        false
    );
    Ok(())
}

#[tokio::test]
async fn resources_list_returns_resource_metadata_and_next_cursor() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ResourcesServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "resources-list",
            "method": "resources/list",
            "params": { "cursor": "first-page" }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body["result"]["resources"][0]["uri"],
        "file:///project/src/main.rs"
    );
    assert_eq!(response.body["result"]["resources"][0]["name"], "main.rs");
    assert_eq!(
        response.body["result"]["resources"][0]["title"],
        "Rust Software Application Main File"
    );
    assert_eq!(
        response.body["result"]["resources"][0]["mimeType"],
        "text/x-rust"
    );
    assert_eq!(response.body["result"]["resources"][0]["size"], 42);
    assert_eq!(response.body["result"]["nextCursor"], "next-resource-page");
    assert!(response.body.get("error").is_none());
    Ok(())
}

#[tokio::test]
async fn resources_read_returns_text_and_blob_contents() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ResourcesServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "resources-read",
            "method": "resources/read",
            "params": { "uri": "file:///project/src/main.rs" }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body["result"]["contents"][0],
        json!({
            "uri": "file:///project/src/main.rs",
            "mimeType": "text/x-rust",
            "text": "fn main() {}"
        })
    );
    assert_eq!(
        response.body["result"]["contents"][1],
        json!({
            "uri": "file:///project/image.png",
            "mimeType": "image/png",
            "blob": "aW1hZ2U="
        })
    );
    assert!(response.body.get("error").is_none());
    Ok(())
}

#[tokio::test]
async fn resources_read_without_uri_returns_invalid_params_error() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ResourcesServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "resources-read-invalid",
            "method": "resources/read",
            "params": {}
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["error"]["code"], -32602);
    assert!(response.body.get("result").is_none());
    Ok(())
}

#[tokio::test]
async fn resources_read_unknown_uri_returns_resource_not_found_error() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ResourcesServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "read-missing-resource",
            "method": "resources/read",
            "params": { "uri": "file:///project/missing.rs" }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["jsonrpc"], "2.0");
    assert_eq!(response.body["id"], "read-missing-resource");
    assert_eq!(response.body["error"]["code"], -32002);
    assert!(response.body.get("result").is_none());

    Ok(())
}
