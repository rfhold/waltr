use mcp::{
    McpResourceDefinition, McpResourceList, McpResourceTemplateDefinition, McpResourceTemplateList,
    McpToolDefinition, McpToolList,
};
use serde_json::json;

#[test]
fn feature_annotations_and_metadata_round_trip_without_interpretation() -> mcp::Result<()> {
    let annotations = json!({
        "audience": ["user", "assistant"], "priority": 0.75,
        "lastModified": "2026-07-28T00:00:00Z"
    });
    let tool_annotations = json!({
        "title": "Safe reader", "readOnlyHint": true, "destructiveHint": false,
        "idempotentHint": true, "openWorldHint": false
    });
    let tools = McpToolList {
        tools: vec![McpToolDefinition {
            name: "read".to_owned(),
            title: None,
            description: None,
            icons: Vec::new(),
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            annotations: Some(tool_annotations.clone()),
            meta: Some(json!({ "example.test/tool": true })),
        }],
        next_cursor: None,
    };
    let parsed_tools = McpToolList::parse(&tools.to_value())?;
    assert_eq!(parsed_tools.tools[0].annotations, Some(tool_annotations));
    assert_eq!(
        parsed_tools.tools[0].meta,
        Some(json!({ "example.test/tool": true }))
    );

    let resources = McpResourceList {
        resources: vec![McpResourceDefinition {
            uri: "file:///resource".to_owned(),
            name: "resource".to_owned(),
            title: None,
            description: None,
            mime_type: None,
            icons: Vec::new(),
            size: None,
            annotations: Some(annotations.clone()),
            meta: Some(json!({ "example.test/resource": true })),
        }],
        next_cursor: None,
    };
    let parsed_resources = McpResourceList::parse(&resources.to_value())?;
    assert_eq!(
        parsed_resources.resources[0].annotations,
        Some(annotations.clone())
    );
    assert_eq!(
        parsed_resources.resources[0].meta,
        Some(json!({ "example.test/resource": true }))
    );

    let templates = McpResourceTemplateList {
        resource_templates: vec![McpResourceTemplateDefinition {
            uri_template: "file:///{path}".to_owned(),
            name: "files".to_owned(),
            title: None,
            description: None,
            mime_type: None,
            icons: Vec::new(),
            annotations: Some(annotations),
            meta: Some(json!({ "example.test/template": true })),
        }],
        next_cursor: None,
    };
    let parsed_templates = McpResourceTemplateList::parse(&templates.to_value())?;
    assert_eq!(
        parsed_templates.resource_templates[0].meta,
        Some(json!({ "example.test/template": true }))
    );
    Ok(())
}
