use std::sync::Arc;

use mcp::{
    McpResourceTemplateDefinition, McpResourceTemplateList,
    server::{BoxFuture, ServerCapabilities, ServerContext, ServerHandler, ServerInfo},
};
use reqwest::StatusCode;
use serde_json::json;

use crate::spec::support::{SpecServer, post_json};

struct ResourceTemplatesServer;

impl ServerHandler for ResourceTemplatesServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-resource-templates-server", "0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().resources()
    }

    fn list_resource_templates(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<mcp::server::ServerResult<McpResourceTemplateList>> {
        Box::pin(async {
            Ok(McpResourceTemplateList {
                resource_templates: vec![McpResourceTemplateDefinition {
                    uri_template: "file:///{path}".to_owned(),
                    name: "Project Files".to_owned(),
                    title: Some("Project Files".to_owned()),
                    description: Some("Access files in the project directory".to_owned()),
                    mime_type: Some("application/octet-stream".to_owned()),
                    icons: Vec::new(),
                    annotations: None,
                    meta: None,
                }],
                next_cursor: Some("next-template-page".to_owned()),
            })
        })
    }
}

#[tokio::test]
async fn initialize_declares_resources_capability_for_templates() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ResourceTemplatesServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "initialize-resource-templates",
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
    assert!(
        response.body["result"]["capabilities"]
            .get("resources")
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn resource_templates_list_returns_template_metadata_and_next_cursor() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ResourceTemplatesServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "resource-templates-list",
            "method": "resources/templates/list",
            "params": { "cursor": "first-page" }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body["result"]["resourceTemplates"][0]["uriTemplate"],
        "file:///{path}"
    );
    assert_eq!(
        response.body["result"]["resourceTemplates"][0]["name"],
        "Project Files"
    );
    assert_eq!(
        response.body["result"]["resourceTemplates"][0]["title"],
        "Project Files"
    );
    assert_eq!(
        response.body["result"]["resourceTemplates"][0]["description"],
        "Access files in the project directory"
    );
    assert_eq!(
        response.body["result"]["resourceTemplates"][0]["mimeType"],
        "application/octet-stream"
    );
    assert_eq!(response.body["result"]["nextCursor"], "next-template-page");
    assert!(response.body.get("error").is_none());
    Ok(())
}
