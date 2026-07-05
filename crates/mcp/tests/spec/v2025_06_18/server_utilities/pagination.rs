use std::sync::{Arc, Mutex};

use mcp::server::{
    BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
    ServerResult,
};
use mcp::{
    McpPromptList, McpResourceList, McpResourceTemplateList, McpToolDefinition, McpToolList,
};
use reqwest::StatusCode;
use serde_json::json;

use crate::spec::support::{SpecServer, post_json};

#[derive(Debug, Default)]
struct SeenCursors {
    tools: Vec<Option<String>>,
    prompts: Vec<Option<String>>,
    resources: Vec<Option<String>>,
    resource_templates: Vec<Option<String>>,
}

struct PaginatedServer {
    seen: Arc<Mutex<SeenCursors>>,
}

impl PaginatedServer {
    fn new(seen: Arc<Mutex<SeenCursors>>) -> Self {
        Self { seen }
    }
}

impl ServerHandler for PaginatedServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-pagination-server", "0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().tools().prompts().resources()
    }

    fn list_tools(
        &self,
        cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolList>> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            seen.lock()
                .expect("cursors lock")
                .tools
                .push(cursor.clone());
            if cursor.as_deref() == Some("bad") {
                return Err(ServerError::invalid_params("invalid cursor"));
            }
            Ok(McpToolList {
                tools: vec![McpToolDefinition {
                    name: cursor.unwrap_or_else(|| "first-tool".to_owned()),
                    title: None,
                    description: None,
                    input_schema: json!({ "type": "object" }),
                    output_schema: None,
                    annotations: None,
                }],
                next_cursor: Some("opaque-next-tool-cursor".to_owned()),
            })
        })
    }

    fn list_prompts(
        &self,
        cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpPromptList>> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            seen.lock().expect("cursors lock").prompts.push(cursor);
            Ok(McpPromptList {
                prompts: Vec::new(),
                next_cursor: Some("opaque-next-prompt-cursor".to_owned()),
            })
        })
    }

    fn list_resources(
        &self,
        cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpResourceList>> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            seen.lock().expect("cursors lock").resources.push(cursor);
            Ok(McpResourceList {
                resources: Vec::new(),
                next_cursor: Some("opaque-next-resource-cursor".to_owned()),
            })
        })
    }

    fn list_resource_templates(
        &self,
        cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpResourceTemplateList>> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            seen.lock()
                .expect("cursors lock")
                .resource_templates
                .push(cursor);
            Ok(McpResourceTemplateList {
                resource_templates: Vec::new(),
                next_cursor: Some("opaque-next-template-cursor".to_owned()),
            })
        })
    }
}

#[tokio::test]
async fn paginated_operations_accept_optional_cursor_and_return_next_cursor() -> mcp::Result<()> {
    let seen = Arc::new(Mutex::new(SeenCursors::default()));
    let server = SpecServer::start(Arc::new(PaginatedServer::new(Arc::clone(&seen)))).await?;

    let first_tools = post_json(
        &server.endpoint(),
        json!({ "jsonrpc": "2.0", "id": "tools-1", "method": "tools/list" }),
    )
    .await?;
    let second_tools = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "tools-2",
            "method": "tools/list",
            "params": { "cursor": "opaque-next-tool-cursor" }
        }),
    )
    .await?;
    let prompts = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "prompts-1",
            "method": "prompts/list",
            "params": { "cursor": "opaque-next-prompt-cursor" }
        }),
    )
    .await?;
    let resources = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "resources-1",
            "method": "resources/list",
            "params": { "cursor": "opaque-next-resource-cursor" }
        }),
    )
    .await?;
    let resource_templates = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "templates-1",
            "method": "resources/templates/list",
            "params": { "cursor": "opaque-next-template-cursor" }
        }),
    )
    .await?;

    assert_eq!(first_tools.status, StatusCode::OK);
    assert_eq!(
        first_tools.body["result"]["nextCursor"],
        "opaque-next-tool-cursor"
    );
    assert_eq!(
        second_tools.body["result"]["tools"][0]["name"],
        "opaque-next-tool-cursor"
    );
    assert_eq!(
        prompts.body["result"]["nextCursor"],
        "opaque-next-prompt-cursor"
    );
    assert_eq!(
        resources.body["result"]["nextCursor"],
        "opaque-next-resource-cursor"
    );
    assert_eq!(
        resource_templates.body["result"]["nextCursor"],
        "opaque-next-template-cursor"
    );

    let seen = seen.lock().expect("cursors lock");
    assert_eq!(
        seen.tools,
        vec![None, Some("opaque-next-tool-cursor".to_owned())]
    );
    assert_eq!(
        seen.prompts,
        vec![Some("opaque-next-prompt-cursor".to_owned())]
    );
    assert_eq!(
        seen.resources,
        vec![Some("opaque-next-resource-cursor".to_owned())]
    );
    assert_eq!(
        seen.resource_templates,
        vec![Some("opaque-next-template-cursor".to_owned())]
    );

    Ok(())
}

#[tokio::test]
async fn invalid_cursor_returns_invalid_params_error() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(PaginatedServer::new(Arc::new(Mutex::new(
        SeenCursors::default(),
    )))))
    .await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "bad-cursor",
            "method": "tools/list",
            "params": { "cursor": "bad" }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["jsonrpc"], "2.0");
    assert_eq!(response.body["id"], "bad-cursor");
    assert_eq!(response.body["error"]["code"], -32602);
    assert!(response.body.get("result").is_none());

    Ok(())
}
