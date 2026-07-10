use std::sync::Arc;

use mcp::{
    McpResourceResult, McpResourceTemplateDefinition,
    server::{ServerContext, ServerError, ServerResult},
};
use reqwest::StatusCode;
use serde_json::json;

use crate::spec::support::{SpecServer, post_json};

struct ResourceTemplatesServer;

#[mcp::mcp_server(
    name = "mcp-spec-resource-templates-server",
    version = "0",
    resource_templates_next_cursor = "next-template-page"
)]
impl ResourceTemplatesServer {
    #[resource_template(uri_template = "file:///{path}", definition = project_files_template())]
    fn read_project_file(uri: String, _context: ServerContext) -> ServerResult<McpResourceResult> {
        Err(ServerError::resource_not_found(uri))
    }
}

fn project_files_template() -> McpResourceTemplateDefinition {
    McpResourceTemplateDefinition {
        uri_template: "file:///{path}".to_owned(),
        name: "Project Files".to_owned(),
        title: Some("Project Files".to_owned()),
        description: Some("Access files in the project directory".to_owned()),
        mime_type: Some("application/octet-stream".to_owned()),
        icons: Vec::new(),
        annotations: None,
        meta: None,
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
