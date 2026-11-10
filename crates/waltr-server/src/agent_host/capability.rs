use super::sandbox_gateway::SandboxHandle;
use waltr_agent::{AgentTool, Result, ToolDefinition, ToolExecutionContext, ToolFuture};

/// A host implementation and its private execution metadata, never a model alias grant.
#[derive(Clone, Debug)]
pub struct Capability {
    pub(crate) tool: AgentTool,
    pub(crate) concrete_target: Option<super::authorized_tools::ConcreteToolTarget>,
    pub(crate) mcp_app_source: Option<super::mcp::apps::McpAppSource>,
    pub(crate) mcp_source_revision: Option<String>,
    pub(crate) sandbox: Option<SandboxHandle>,
    #[cfg(feature = "shell")]
    pub(crate) shell_config: Option<super::shell::ShellConfig>,
}

impl From<AgentTool> for Capability {
    fn from(tool: AgentTool) -> Self {
        Self {
            tool,
            concrete_target: None,
            mcp_app_source: None,
            mcp_source_revision: None,
            sandbox: None,
            #[cfg(feature = "shell")]
            shell_config: None,
        }
    }
}

impl Capability {
    pub fn with_concrete_target(
        mut self,
        target: super::authorized_tools::ConcreteToolTarget,
    ) -> Self {
        self.tool = self.tool.with_identity(target.permission_name());
        self.concrete_target = Some(target);
        self
    }
    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.tool = self.tool.with_identity(identity);
        self
    }

    pub fn with_host_dispatch(
        mut self,
        prepare: impl Fn(&waltr_agent::ToolCall) -> Result<waltr_agent::HostDispatchBinding>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.tool = self.tool.with_host_dispatch(prepare);
        self
    }
    pub fn new(
        definition: ToolDefinition,
        handler: impl Fn(ToolExecutionContext) -> Result<waltr_agent::ToolResult>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        AgentTool::new(definition, handler).into()
    }
    pub fn new_async(
        definition: ToolDefinition,
        handler: impl Fn(ToolExecutionContext) -> ToolFuture + Send + Sync + 'static,
    ) -> Self {
        AgentTool::new_async(definition, handler).into()
    }
    pub fn worker(definition: ToolDefinition, sandbox: SandboxHandle) -> Self {
        let mut capability = Self::new(definition, |context| {
            Ok(context.result("worker dispatch required").with_error())
        });
        capability.sandbox = Some(sandbox);
        capability
    }
    pub fn definition(&self) -> &ToolDefinition {
        self.tool.definition()
    }
    pub fn identity(&self) -> String {
        self.tool.identity().map_or_else(
            || format!("builtin:{}", self.definition().name),
            str::to_owned,
        )
    }
    pub fn with_model_exposure(mut self, exposed: bool) -> Self {
        self.tool = self.tool.with_model_exposure(exposed);
        self
    }
    pub fn is_sandbox_backed(&self) -> bool {
        self.sandbox.is_some()
    }
    #[cfg(feature = "shell")]
    pub fn with_shell_config(mut self, config: super::shell::ShellConfig) -> Self {
        self.shell_config = Some(config);
        self
    }
    pub fn shell_defaults(
        &self,
    ) -> Option<waltr::grpc::waltr::sandbox::v1::ShellExecutionDefaults> {
        #[cfg(feature = "shell")]
        if let Some(config) = &self.shell_config {
            return Some(waltr::grpc::waltr::sandbox::v1::ShellExecutionDefaults {
                default_workdir: config.default_workdir.to_string_lossy().into_owned(),
                default_timeout_seconds: config.default_timeout.as_secs(),
                default_timeout_nanos: config.default_timeout.subsec_nanos(),
                max_output_bytes: u64::try_from(config.max_output_bytes).unwrap_or(u64::MAX),
            });
        }
        None
    }
}
