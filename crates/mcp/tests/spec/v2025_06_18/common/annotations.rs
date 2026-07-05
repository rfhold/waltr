use mcp::{McpToolDefinition, McpToolList, parse_tool_definition};
use serde_json::json;

#[test]
fn tool_annotations_use_spec_wire_keys_and_boolean_hints() {
    let value = json!({
        "name": "search",
        "title": "Search",
        "inputSchema": { "type": "object" },
        "annotations": {
            "title": "Trusted Search",
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": true
        }
    });

    let tool = parse_tool_definition(&value).expect("tool definition parses");

    assert_eq!(
        tool.annotations,
        Some(json!({
            "title": "Trusted Search",
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": true
        }))
    );
}

#[test]
fn tool_list_reemits_tool_annotations_without_renaming() {
    let tools = McpToolList {
        tools: vec![McpToolDefinition {
            name: "search".to_owned(),
            title: None,
            description: None,
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            annotations: Some(json!({
                "title": "Trusted Search",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            })),
        }],
        next_cursor: None,
    };

    assert_eq!(
        tools.to_value(),
        json!({
            "tools": [{
                "name": "search",
                "inputSchema": { "type": "object" },
                "annotations": {
                    "title": "Trusted Search",
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                }
            }]
        })
    );
}

#[test]
fn content_annotations_allow_audience_priority_and_last_modified() {
    let content = json!({
        "type": "text",
        "text": "Read this first",
        "annotations": {
            "audience": ["user", "assistant"],
            "priority": 0.75,
            "lastModified": "2025-01-12T15:00:58Z"
        }
    });

    assert_eq!(
        content["annotations"]["audience"],
        json!(["user", "assistant"])
    );
    assert_eq!(content["annotations"]["priority"], 0.75);
    assert_eq!(
        content["annotations"]["lastModified"],
        "2025-01-12T15:00:58Z"
    );
}
