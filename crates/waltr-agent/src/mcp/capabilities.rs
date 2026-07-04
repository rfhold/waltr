use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub(in crate::mcp) struct McpServerCapabilities {
    pub(in crate::mcp) tools: Option<McpToolsCapability>,
    pub(in crate::mcp) prompts: Option<McpPromptsCapability>,
    #[allow(dead_code)]
    pub(in crate::mcp) logging: bool,
    #[allow(dead_code)]
    pub(in crate::mcp) raw: Value,
}

#[derive(Debug, Clone, Default)]
pub(in crate::mcp) struct McpToolsCapability {
    #[allow(dead_code)]
    pub(in crate::mcp) list_changed: bool,
}

#[derive(Debug, Clone, Default)]
pub(in crate::mcp) struct McpPromptsCapability {
    #[allow(dead_code)]
    pub(in crate::mcp) list_changed: bool,
}

impl McpServerCapabilities {
    pub(in crate::mcp) fn parse(value: Option<&Value>) -> Self {
        let Some(value) = value else {
            return Self::default();
        };
        let tools = value.get("tools").map(|tools| McpToolsCapability {
            list_changed: tools
                .get("listChanged")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
        let prompts = value.get("prompts").map(|prompts| McpPromptsCapability {
            list_changed: prompts
                .get("listChanged")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
        Self {
            tools,
            prompts,
            logging: value.get("logging").is_some(),
            raw: value.clone(),
        }
    }
}
