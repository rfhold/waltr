use mcp::{
    McpCompletionRequest, McpPromptArgument, McpPromptDefinition, McpPromptList,
    McpResourceDefinition, McpResourceList, McpResourceTemplateDefinition, McpResourceTemplateList,
    McpRoot, McpToolCall, McpToolDefinition, McpToolList, parse_tool_definition,
};
use serde_json::json;

#[test]
fn tool_definition_preserves_base_metadata_and_json_schemas() {
    let value = json!({
        "name": "get_weather_data",
        "title": "Weather Data Retriever",
        "description": "Get current weather data for a location",
        "inputSchema": {
            "type": "object",
            "properties": {
                "location": { "type": "string" }
            },
            "required": ["location"]
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "temperature": { "type": "number" }
            },
            "required": ["temperature"]
        }
    });

    let tool = parse_tool_definition(&value).expect("tool definition parses");

    assert_eq!(tool.name, "get_weather_data");
    assert_eq!(tool.title.as_deref(), Some("Weather Data Retriever"));
    assert_eq!(
        tool.description.as_deref(),
        Some("Get current weather data for a location")
    );
    assert_eq!(tool.input_schema, value["inputSchema"]);
    assert_eq!(tool.output_schema, Some(value["outputSchema"].clone()));
}

#[test]
fn list_results_emit_spec_collection_names_and_next_cursor() {
    let tools = McpToolList {
        tools: vec![McpToolDefinition {
            name: "get_weather".to_owned(),
            title: Some("Weather Information Provider".to_owned()),
            description: None,
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            annotations: None,
        }],
        next_cursor: Some("next-tool-page".to_owned()),
    };
    let prompts = McpPromptList {
        prompts: vec![McpPromptDefinition {
            name: "code_review".to_owned(),
            title: Some("Request Code Review".to_owned()),
            description: None,
            arguments: vec![McpPromptArgument {
                name: "code".to_owned(),
                description: Some("The code to review".to_owned()),
                required: true,
            }],
            meta: None,
        }],
        next_cursor: Some("next-prompt-page".to_owned()),
    };
    let resources = McpResourceList {
        resources: vec![McpResourceDefinition {
            uri: "file:///project/src/main.rs".to_owned(),
            name: "main.rs".to_owned(),
            title: Some("Main file".to_owned()),
            description: Some("Primary entry point".to_owned()),
            mime_type: Some("text/x-rust".to_owned()),
            size: Some(123),
            annotations: None,
            meta: None,
        }],
        next_cursor: Some("next-resource-page".to_owned()),
    };
    let resource_templates = McpResourceTemplateList {
        resource_templates: vec![McpResourceTemplateDefinition {
            uri_template: "file:///{path}".to_owned(),
            name: "Project Files".to_owned(),
            title: Some("Project Files".to_owned()),
            description: Some("Access files in the project directory".to_owned()),
            mime_type: Some("application/octet-stream".to_owned()),
            annotations: None,
            meta: None,
        }],
        next_cursor: Some("next-template-page".to_owned()),
    };

    assert_eq!(tools.to_value()["tools"][0]["name"], "get_weather");
    assert_eq!(tools.to_value()["nextCursor"], "next-tool-page");
    assert_eq!(
        prompts.to_value()["prompts"][0]["arguments"][0]["required"],
        true
    );
    assert_eq!(prompts.to_value()["nextCursor"], "next-prompt-page");
    assert_eq!(
        resources.to_value()["resources"][0]["mimeType"],
        "text/x-rust"
    );
    assert_eq!(resources.to_value()["resources"][0]["size"], 123);
    assert_eq!(resources.to_value()["nextCursor"], "next-resource-page");
    assert_eq!(
        resource_templates.to_value()["resourceTemplates"][0]["uriTemplate"],
        "file:///{path}"
    );
    assert_eq!(
        resource_templates.to_value()["nextCursor"],
        "next-template-page"
    );
}

#[test]
fn prompt_resource_and_template_metadata_fields_round_trip() -> mcp::Result<()> {
    let prompts = McpPromptList::parse(&json!({
        "prompts": [{
            "name": "code_review",
            "_meta": { "source": "spec" }
        }]
    }))?;
    assert_eq!(prompts.prompts[0].meta, Some(json!({ "source": "spec" })));
    assert_eq!(
        prompts.to_value()["prompts"][0]["_meta"],
        json!({ "source": "spec" })
    );

    let resources = McpResourceList::parse(&json!({
        "resources": [{
            "uri": "file:///project/src/main.rs",
            "name": "main.rs",
            "annotations": { "audience": ["assistant"] },
            "_meta": { "etag": "abc123" }
        }]
    }))?;
    assert_eq!(
        resources.resources[0].annotations,
        Some(json!({ "audience": ["assistant"] }))
    );
    assert_eq!(
        resources.resources[0].meta,
        Some(json!({ "etag": "abc123" }))
    );
    assert_eq!(
        resources.to_value()["resources"][0]["annotations"],
        json!({ "audience": ["assistant"] })
    );
    assert_eq!(
        resources.to_value()["resources"][0]["_meta"],
        json!({ "etag": "abc123" })
    );

    let templates = McpResourceTemplateList::parse(&json!({
        "resourceTemplates": [{
            "uriTemplate": "file:///{path}",
            "name": "Project Files",
            "annotations": { "priority": 0.5 },
            "_meta": { "owner": "tests" }
        }]
    }))?;
    assert_eq!(
        templates.resource_templates[0].annotations,
        Some(json!({ "priority": 0.5 }))
    );
    assert_eq!(
        templates.resource_templates[0].meta,
        Some(json!({ "owner": "tests" }))
    );
    assert_eq!(
        templates.to_value()["resourceTemplates"][0]["annotations"],
        json!({ "priority": 0.5 })
    );
    assert_eq!(
        templates.to_value()["resourceTemplates"][0]["_meta"],
        json!({ "owner": "tests" })
    );

    Ok(())
}

#[test]
fn tool_call_params_use_arguments_and_progress_token_meta_shape() {
    let call = McpToolCall::new("get_weather", json!({ "location": "New York" }))
        .with_progress_token("progress-1");

    let params = call.to_params();
    let parsed = McpToolCall::from_params(&params).expect("tool call params parse");

    assert_eq!(
        params,
        json!({
            "name": "get_weather",
            "arguments": { "location": "New York" },
            "_meta": { "progressToken": "progress-1" }
        })
    );
    assert_eq!(parsed.name, "get_weather");
    assert_eq!(parsed.arguments, json!({ "location": "New York" }));
    assert_eq!(parsed.progress_token, Some(json!("progress-1")));
}

#[test]
fn completion_request_uses_resource_reference_argument_and_context_shape() {
    let mut request = McpCompletionRequest::new("file:///{path}", "path", "src");
    request
        .context_arguments
        .insert("language".to_owned(), json!("rust"));

    assert_eq!(
        request.to_params(),
        json!({
            "ref": { "type": "ref/resource", "uri": "file:///{path}" },
            "argument": { "name": "path", "value": "src" },
            "context": { "arguments": { "language": "rust" } }
        })
    );
}

#[test]
fn root_wire_shape_includes_file_uri_and_optional_name_only_when_present() {
    assert_eq!(
        McpRoot::new("file:///home/user/project", Some("Project".to_owned())).to_value(),
        json!({ "uri": "file:///home/user/project", "name": "Project" })
    );
    assert_eq!(
        McpRoot::new("file:///home/user/project", None).to_value(),
        json!({ "uri": "file:///home/user/project" })
    );
}
