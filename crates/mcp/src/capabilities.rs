use serde_json::Value;

/// Server capabilities from initialize result.
#[derive(Debug, Clone, Default)]
pub struct McpServerCapabilities {
    /// Tools capability.
    pub tools: Option<McpToolsCapability>,
    /// Prompts capability.
    pub prompts: Option<McpPromptsCapability>,
    /// Resources capability.
    pub resources: Option<McpResourcesCapability>,
    /// Completion support.
    pub completions: bool,
    /// Logging support.
    pub logging: bool,
    /// Tasks capability.
    pub tasks: Option<McpTasksCapability>,
    /// Sampling capability.
    pub sampling: Option<McpSamplingCapability>,
    /// Elicitation capability.
    pub elicitation: Option<McpElicitationCapability>,
    /// Raw capabilities object.
    pub raw: Value,
}

/// Tools capability.
#[derive(Debug, Clone, Default)]
pub struct McpToolsCapability {
    /// Whether list changed notifications are supported.
    pub list_changed: bool,
}

/// Prompts capability.
#[derive(Debug, Clone, Default)]
pub struct McpPromptsCapability {
    /// Whether list changed notifications are supported.
    pub list_changed: bool,
}

/// Resources capability.
#[derive(Debug, Clone, Default)]
pub struct McpResourcesCapability {
    /// Whether resource subscriptions are supported.
    pub subscribe: bool,
    /// Whether list changed notifications are supported.
    pub list_changed: bool,
}

/// Tasks capability.
#[derive(Debug, Clone, Default)]
pub struct McpTasksCapability {
    /// Raw tasks capability object.
    pub raw: Value,
}

/// Sampling capability.
#[derive(Debug, Clone, Default)]
pub struct McpSamplingCapability {
    /// Whether sampling tool calls are supported.
    pub tools: bool,
    /// Whether sampling context controls are supported.
    pub context: bool,
    /// Raw sampling capability object.
    pub raw: Value,
}

/// Elicitation capability.
#[derive(Debug, Clone, Default)]
pub struct McpElicitationCapability {
    /// Whether form elicitation is supported.
    pub form: bool,
    /// Whether URL elicitation is supported.
    pub url: bool,
    /// Raw elicitation capability object.
    pub raw: Value,
}

impl McpServerCapabilities {
    /// Parse capabilities from an optional JSON object.
    #[must_use]
    pub fn parse(value: Option<&Value>) -> Self {
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
        let resources = value
            .get("resources")
            .map(|resources| McpResourcesCapability {
                subscribe: resources
                    .get("subscribe")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                list_changed: resources
                    .get("listChanged")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        Self {
            tools,
            prompts,
            resources,
            completions: value.get("completions").is_some(),
            logging: value.get("logging").is_some(),
            tasks: value
                .get("tasks")
                .map(|tasks| McpTasksCapability { raw: tasks.clone() }),
            sampling: value.get("sampling").map(|sampling| McpSamplingCapability {
                tools: sampling.get("tools").is_some(),
                context: sampling.get("context").is_some(),
                raw: sampling.clone(),
            }),
            elicitation: value
                .get("elicitation")
                .map(|elicitation| McpElicitationCapability {
                    form: elicitation
                        .as_object()
                        .is_none_or(|object| object.is_empty())
                        || elicitation.get("form").is_some(),
                    url: elicitation.get("url").is_some(),
                    raw: elicitation.clone(),
                }),
            raw: value.clone(),
        }
    }
}
