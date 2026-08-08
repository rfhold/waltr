use std::sync::{Arc, Mutex};

use mcp::{
    McpPromptList, McpResourceList, McpResourceTemplateList, McpToolDefinition, McpToolList,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
        ServerResult,
    },
};
use serde_json::json;

use crate::spec::support::{SpecServer, modern_params, post_json};

#[derive(Debug, Default)]
struct SeenCursors {
    tools: Vec<Option<String>>,
    prompts: Vec<Option<String>>,
    resources: Vec<Option<String>>,
    templates: Vec<Option<String>>,
}

struct PaginatedServer {
    seen: Arc<Mutex<SeenCursors>>,
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
                    icons: Vec::new(),
                    input_schema: json!({ "type": "object" }),
                    output_schema: None,
                    annotations: None,
                    meta: None,
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
            seen.lock().expect("cursors lock").templates.push(cursor);
            Ok(McpResourceTemplateList {
                resource_templates: Vec::new(),
                next_cursor: Some("opaque-next-template-cursor".to_owned()),
            })
        })
    }
}

#[tokio::test]
async fn list_operations_propagate_optional_opaque_cursors_and_return_next_cursor()
-> mcp::Result<()> {
    let seen = Arc::new(Mutex::new(SeenCursors::default()));
    let server = SpecServer::start(Arc::new(PaginatedServer {
        seen: Arc::clone(&seen),
    }))
    .await?;

    for (id, method, cursor, expected) in [
        ("tools-first", "tools/list", None, "opaque-next-tool-cursor"),
        (
            "tools-second",
            "tools/list",
            Some("opaque-next-tool-cursor"),
            "opaque-next-tool-cursor",
        ),
        (
            "prompts",
            "prompts/list",
            Some("opaque-next-prompt-cursor"),
            "opaque-next-prompt-cursor",
        ),
        (
            "resources",
            "resources/list",
            Some("opaque-next-resource-cursor"),
            "opaque-next-resource-cursor",
        ),
        (
            "templates",
            "resources/templates/list",
            Some("opaque-next-template-cursor"),
            "opaque-next-template-cursor",
        ),
    ] {
        let params = cursor.map_or_else(
            || modern_params(json!({})),
            |cursor| modern_params(json!({ "cursor": cursor })),
        );
        let response = post_json(
            &server.endpoint(),
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        )
        .await?;
        assert_eq!(response.body["result"]["nextCursor"], expected);
    }

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
        seen.templates,
        vec![Some("opaque-next-template-cursor".to_owned())]
    );
    drop(seen);
    Ok(())
}

#[tokio::test]
async fn invalid_cursor_returns_invalid_params() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(PaginatedServer {
        seen: Arc::new(Mutex::new(SeenCursors::default())),
    }))
    .await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "bad-cursor", "method": "tools/list",
            "params": modern_params(json!({ "cursor": "bad" }))
        }),
    )
    .await?;
    assert_eq!(response.body["error"]["code"], -32602);
    Ok(())
}
