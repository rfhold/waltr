use mcp::{McpPromptResult, McpResourceResult, McpToolResult};
use serde_json::json;

#[test]
fn tool_text_result_uses_content_array_with_text_block() {
    let result = McpToolResult::text("Tool result text");

    assert_eq!(
        result.raw,
        json!({
            "content": [{
                "type": "text",
                "text": "Tool result text"
            }]
        })
    );
}

#[test]
fn prompt_text_result_uses_user_role_and_text_content_block() {
    let result = McpPromptResult::text_user("Prompt text");

    assert_eq!(
        result.raw,
        json!({
            "messages": [{
                "role": "user",
                "content": {
                    "type": "text",
                    "text": "Prompt text"
                }
            }]
        })
    );
}

#[test]
fn resource_text_result_uses_contents_array_with_uri_mime_type_and_text() {
    let result = McpResourceResult::text("file:///example.txt", "text/plain", "Resource content");

    assert_eq!(
        result.raw,
        json!({
            "contents": [{
                "uri": "file:///example.txt",
                "mimeType": "text/plain",
                "text": "Resource content"
            }]
        })
    );
}

#[test]
fn raw_tool_result_can_carry_all_spec_content_block_variants() {
    let result = McpToolResult::new(json!({
        "content": [
            { "type": "text", "text": "hello" },
            { "type": "image", "data": "aW1hZ2U=", "mimeType": "image/png" },
            { "type": "audio", "data": "YXVkaW8=", "mimeType": "audio/wav" },
            {
                "type": "resource_link",
                "uri": "file:///project/src/main.rs",
                "name": "main.rs",
                "title": "Main file",
                "description": "Primary entry point",
                "mimeType": "text/x-rust",
                "size": 123
            },
            {
                "type": "resource",
                "resource": {
                    "uri": "file:///project/README.md",
                    "mimeType": "text/markdown",
                    "text": "# Project"
                }
            },
            {
                "type": "resource",
                "resource": {
                    "uri": "file:///project/logo.png",
                    "mimeType": "image/png",
                    "blob": "iVBORw0KGgo="
                }
            }
        ]
    }));

    assert_eq!(result.raw["content"].as_array().map(Vec::len), Some(6));
    assert_eq!(result.raw["content"][3]["type"], "resource_link");
    assert_eq!(result.raw["content"][4]["resource"]["text"], "# Project");
    assert_eq!(result.raw["content"][5]["resource"]["blob"], "iVBORw0KGgo=");
}
