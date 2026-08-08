use mcp::{McpPromptResult, McpResourceResult, McpToolResult};
use serde_json::json;

#[test]
fn active_content_variants_preserve_wire_shapes_and_formatting() {
    let tool = McpToolResult::new(json!({
        "content": [
            { "type": "text", "text": "hello" },
            { "type": "image", "data": "aW1hZ2U=", "mimeType": "image/png" },
            { "type": "audio", "data": "YXVkaW8=", "mimeType": "audio/wav" },
            { "type": "resource_link", "uri": "file:///linked", "name": "linked" },
            { "type": "resource", "resource": { "uri": "file:///embedded", "text": "body" } },
            { "type": "resource", "resource": { "uri": "file:///blob", "blob": "YmxvYg==" } }
        ],
        "structuredContent": { "ok": true },
        "isError": true
    }));
    assert_eq!(tool.raw["content"][1]["mimeType"], "image/png");
    assert_eq!(tool.raw["content"][2]["type"], "audio");
    assert_eq!(tool.raw["content"][3]["uri"], "file:///linked");
    assert_eq!(tool.raw["content"][4]["resource"]["text"], "body");
    assert_eq!(tool.raw["content"][5]["resource"]["blob"], "YmxvYg==");
    assert!(tool.format_text().starts_with("MCP tool error: hello"));

    let prompt = McpPromptResult::text_user("review this");
    assert_eq!(prompt.raw["messages"][0]["role"], "user");
    assert_eq!(prompt.format_text(), "user: review this");

    let resource = McpResourceResult::new(json!({
        "contents": [
            { "uri": "file:///text", "mimeType": "text/plain", "text": "body" },
            { "uri": "file:///blob", "mimeType": "image/png", "blob": "YmxvYg==" }
        ]
    }));
    assert!(resource.format_text().contains("file:///text:\nbody"));
    assert!(
        resource
            .format_text()
            .contains("unsupported MCP resource blob content: image/png")
    );
}
