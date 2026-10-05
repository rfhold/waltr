//! Server-owned readiness app, exposed through ordinary MCP tool/resource metadata.

use mcp::server::ServerContext;
use serde_json::json;

pub(crate) const URI: &str = "ui://waltr/status";

pub(super) const APP: super::AppResource = super::AppResource {
    uri: URI,
    name: "Waltr Status",
    description: "Interactive readiness observation and refresh.",
    html: super::STATUS_HTML,
};

pub(crate) fn tool(context: &ServerContext) -> mcp::McpToolDefinition {
    let mut definition = json!({"name":"status", "description":"Read current Waltr readiness without inference.",
        "inputSchema":{"type":"object","properties":{},"additionalProperties":false}});
    if APP.supported(context) {
        definition["_meta"] = json!({"ui": {"resourceUri": URI}});
    }
    mcp::parse_tool_definition(&definition).expect("static status declaration")
}
