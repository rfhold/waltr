use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ShellCommandParser, ShellScan, tool_input};
use crate::{
    AgentTool, Result, ToolDefinition, ToolExecutionContext, ToolResult,
    middleware::tool_metadata::RootHintsMiddleware,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_OUTPUT_BYTES: usize = 20 * 1024;
pub const SHELL_TOOL_NAME: &str = "shell";

/// Shell tool configuration for an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellConfig {
    /// Root directory used for relative workdir and path resolution.
    pub root: PathBuf,
    /// Default working directory for shell commands.
    pub default_workdir: PathBuf,
    /// Default command timeout.
    pub default_timeout: Duration,
    /// Maximum output bytes returned to the model.
    pub max_output_bytes: usize,
}

impl ShellConfig {
    /// Create shell tool configuration rooted at one directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            default_workdir: root.clone(),
            root,
            default_timeout: DEFAULT_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Override the default working directory.
    #[must_use]
    pub fn with_default_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.default_workdir = workdir.into();
        self
    }

    /// Override the default timeout.
    #[must_use]
    pub const fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    /// Override the maximum output bytes returned to the model.
    #[must_use]
    pub const fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    /// Build the per-request root-hint metadata middleware for the `shell` tool.
    pub(crate) fn root_hints_middleware(&self) -> RootHintsMiddleware {
        let root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        RootHintsMiddleware::new(
            vec![SHELL_TOOL_NAME.to_owned()],
            format!(
                " Relative workdirs and path-like command arguments resolve from root: {}.",
                root.display()
            ),
        )
    }

    /// Convert this configuration into the `shell` agent tool.
    #[must_use]
    pub fn into_agent_tool(self) -> AgentTool {
        let definition = ToolDefinition::new(
            SHELL_TOOL_NAME,
            "Execute a Bash/POSIX shell command on the host after permission checks. Commands run with /bin/bash -lc. Provide a short description explaining why the command is needed.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Bash command to execute."
                    },
                    "description": {
                        "type": "string",
                        "description": "Short reason for running the command."
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional timeout in seconds."
                    },
                    "workdir": {
                        "type": "string",
                        "description": "Optional working directory. Relative paths resolve under the configured root."
                    }
                },
                "required": ["command", "description"],
                "additionalProperties": false
            }),
        );
        AgentTool::new_async(definition, move |context| {
            let config = self.clone();
            Box::pin(async move {
                tokio::task::spawn_blocking(move || config.call_agent_tool(context))
                    .await
                    .map_err(|error| crate::LlmError::Protocol {
                        provider: "shell",
                        message: format!("shell executor task failed: {error}"),
                    })?
            })
        })
    }

    /// Scan a shell command and return parsed commands, claims, and reusable patterns.
    ///
    /// # Errors
    ///
    /// Returns an error if the Bash parser cannot be initialized or the requested workdir cannot be
    /// resolved.
    pub fn scan(&self, input: &ShellInput) -> Result<ShellScan> {
        let workdir = self.resolved_workdir(input.workdir.as_deref());
        ShellCommandParser::new()?.scan(&input.command, &self.root_path(), &workdir)
    }

    fn call_agent_tool(&self, context: ToolExecutionContext) -> Result<ToolResult> {
        let content =
            self.execute_with_progress(&tool_input(context.call())?, context.progress())?;
        Ok(context.result(content))
    }
}

/// Input for the `shell` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellInput {
    /// Bash command to execute.
    pub command: String,
    /// Short model-supplied reason for the command.
    pub description: String,
    /// Optional timeout in seconds.
    pub timeout: Option<u64>,
    /// Optional working directory.
    pub workdir: Option<String>,
}

impl ShellInput {
    /// Create shell input.
    pub fn new(command: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            description: description.into(),
            timeout: None,
            workdir: None,
        }
    }

    /// Set a timeout in seconds.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: u64) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Set a working directory.
    #[must_use]
    pub fn with_workdir(mut self, workdir: impl Into<String>) -> Self {
        self.workdir = Some(workdir.into());
        self
    }
}
