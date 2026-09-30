use serde_json::Value;

/// Server capabilities from a discovery result.
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
    /// Extension capabilities keyed by extension identifier.
    pub extensions: Option<Value>,
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

impl McpServerCapabilities {
    /// Whether the server advertises Skills and its required Resources capability.
    #[must_use]
    pub fn supports_skills(&self) -> bool {
        self.skills().is_some()
    }

    /// Parsed extension settings, only when the required Resources capability is present.
    #[must_use]
    pub fn skills(&self) -> Option<crate::skills::McpSkillsCapability> {
        self.resources.as_ref()?;
        let value = self
            .extensions
            .as_ref()?
            .get(crate::skills::SKILLS_EXTENSION)?;
        if !value.is_object() {
            return None;
        }
        serde_json::from_value(value.clone()).ok()
    }
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
            extensions: value.get("extensions").cloned(),
            raw: value.clone(),
        }
    }
}
