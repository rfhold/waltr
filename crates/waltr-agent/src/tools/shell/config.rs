use std::{fmt::Write as _, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ShellCommandParser, ShellScan, tool_input};
use crate::{AgentTool, PermissionClaim, Result, ToolCall, ToolDefinition};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_OUTPUT_BYTES: usize = 20 * 1024;
const SHELL_TOOL_NAME: &str = "shell";

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

    /// Convert this configuration into the `shell` agent tool.
    #[must_use]
    pub fn into_agent_tool(self) -> AgentTool {
        let mut definition = ToolDefinition::new(
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
        let root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        // TODO(agent-runtime): Move root-specific shell hints into dedicated edit_tool_metadata middleware.
        write!(
            definition.description,
            " Relative workdirs and path-like command arguments resolve from root: {}.",
            root.display()
        )
        .expect("write tool description");

        let claims_config = self.clone();
        AgentTool::new(definition, move |call| self.call_agent_tool(call))
            .with_permission_claims(move |call| claims_config.permission_claims(call))
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

    fn call_agent_tool(&self, call: &ToolCall) -> Result<String> {
        self.execute(&tool_input(call)?)
    }

    fn permission_claims(&self, call: &ToolCall) -> Result<Vec<PermissionClaim>> {
        // TODO(agent-runtime): Move shell permission scans into before_tool_call middleware.
        let input = tool_input::<ShellInput>(call)?;
        Ok(self.scan(&input)?.claims)
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
