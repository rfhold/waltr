use mcp::{
    McpPromptResult, McpResourceResult, McpToolContent, McpToolResult, McpToolResultContentError,
};
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

#[test]
fn tool_result_exposes_ordered_typed_and_extension_content() {
    let tool = McpToolResult::new(json!({
        "content": [
            { "type": "text", "text": "hello" },
            { "type": "image", "data": "aW1hZ2U=", "mimeType": "image/png" },
            { "type": "audio", "data": "YXVkaW8=", "mimeType": "audio/wav" }
        ],
        "structuredContent": { "ok": true },
        "isError": true
    }));
    let parsed = tool.content().unwrap();
    assert!(parsed.is_error);
    assert_eq!(parsed.structured_content, Some(&json!({ "ok": true })));
    assert_eq!(
        parsed.content,
        vec![
            McpToolContent::Text { text: "hello" },
            McpToolContent::Image {
                data: "aW1hZ2U=",
                mime_type: "image/png"
            },
            McpToolContent::Unsupported {
                kind: "audio",
                raw: &tool.raw["content"][2]
            }
        ]
    );
    let debug = format!("{parsed:?}");
    assert!(debug.contains("encoded_bytes"));
    assert!(debug.contains("audio"));
    assert!(!debug.contains("aW1hZ2U="));
    assert!(!debug.contains("YXVkaW8="));
    assert!(!debug.contains("hello"));
}

#[test]
fn tool_result_accepts_every_json_value_as_structured_content() {
    for structured_content in [
        json!({ "ok": true }),
        json!([1, 2]),
        json!("value"),
        json!(42),
        json!(true),
        json!(null),
    ] {
        let tool = McpToolResult::new(json!({
            "content": [],
            "structuredContent": structured_content
        }));
        assert_eq!(
            tool.content()
                .expect("valid tool result")
                .structured_content,
            Some(&structured_content)
        );
    }
}

#[test]
fn tool_result_rejects_malformed_known_content() {
    let malformed = McpToolResult::new(json!({
        "content": [{ "type": "image", "data": "aW1hZ2U=" }]
    }));
    assert_eq!(
        malformed.content().unwrap_err(),
        McpToolResultContentError::ImageMimeTypeNotString { index: 0 }
    );
}
