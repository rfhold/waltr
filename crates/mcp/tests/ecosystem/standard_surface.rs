use mcp::{
    McpPromptArgument, McpPromptDefinition, McpResourceDefinition, McpResourceTemplateDefinition,
    McpToolDefinition,
};
use serde_json::{Value, json};

pub const ECHO_TOOL: &str = "echo_marker";
pub const LOG_TOOL: &str = "emit_log_marker";
pub const PROGRESS_TOOL: &str = "progress_marker";
pub const STATUS_PROMPT: &str = "status_prompt";
pub const GREET_PROMPT: &str = "greet_prompt";
pub const STATUS_RESOURCE: &str = "test://status";
pub const ITEM_TEMPLATE: &str = "test://items/{id}";
pub const ITEM_42: &str = "test://items/42";

pub fn string_value_schema(property: &str) -> Value {
    json!({
        "type": "object",
        "properties": { property: { "type": "string" } },
        "required": [property]
    })
}

pub fn tools() -> Vec<McpToolDefinition> {
    vec![echo_tool(), log_tool(), progress_tool()]
}

pub fn echo_tool() -> McpToolDefinition {
    tool(
        ECHO_TOOL,
        "Echo a deterministic marker.",
        string_value_schema("value"),
    )
}

pub fn log_tool() -> McpToolDefinition {
    tool(
        LOG_TOOL,
        "Emit a deterministic MCP log message.",
        string_value_schema("message"),
    )
}

pub fn progress_tool() -> McpToolDefinition {
    tool(
        PROGRESS_TOOL,
        "Emit progress and echo a deterministic marker.",
        string_value_schema("value"),
    )
}

pub fn prompts() -> Vec<McpPromptDefinition> {
    vec![status_prompt(), greet_prompt()]
}

pub fn status_prompt() -> McpPromptDefinition {
    McpPromptDefinition {
        name: STATUS_PROMPT.to_owned(),
        title: None,
        description: Some("Return deterministic status prompt.".to_owned()),
        icons: Vec::new(),
        arguments: Vec::new(),
        meta: None,
    }
}

pub fn greet_prompt() -> McpPromptDefinition {
    McpPromptDefinition {
        name: GREET_PROMPT.to_owned(),
        title: None,
        description: Some("Greet a named test subject.".to_owned()),
        icons: Vec::new(),
        arguments: vec![McpPromptArgument {
            name: "name".to_owned(),
            description: Some("Name to greet".to_owned()),
            required: true,
        }],
        meta: None,
    }
}

pub fn resources() -> Vec<McpResourceDefinition> {
    vec![status_resource()]
}

pub fn status_resource() -> McpResourceDefinition {
    McpResourceDefinition {
        uri: STATUS_RESOURCE.to_owned(),
        name: "status_resource".to_owned(),
        title: Some("Status Resource".to_owned()),
        description: Some("Return deterministic status resource text".to_owned()),
        mime_type: Some("text/plain".to_owned()),
        icons: Vec::new(),
        size: Some(29),
        annotations: None,
        meta: None,
    }
}

pub fn resource_templates() -> Vec<McpResourceTemplateDefinition> {
    vec![item_resource_template()]
}

pub fn item_resource_template() -> McpResourceTemplateDefinition {
    McpResourceTemplateDefinition {
        uri_template: ITEM_TEMPLATE.to_owned(),
        name: "item_resource".to_owned(),
        title: Some("Item Resource".to_owned()),
        description: Some("Return deterministic templated item resource text".to_owned()),
        mime_type: Some("text/plain".to_owned()),
        icons: Vec::new(),
        annotations: None,
        meta: None,
    }
}

pub fn item_completions(prefix: &str) -> Vec<String> {
    ["42", "420", "4242"]
        .into_iter()
        .filter(|value| value.starts_with(prefix))
        .map(ToOwned::to_owned)
        .collect()
}

fn tool(name: &str, description: &str, input_schema: Value) -> McpToolDefinition {
    McpToolDefinition {
        name: name.to_owned(),
        title: None,
        description: Some(description.to_owned()),
        icons: Vec::new(),
        input_schema,
        output_schema: None,
        task_support: None,
        annotations: None,
    }
}
