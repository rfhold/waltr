//! Agent-side translation of worker claims and single-use tool tickets.

use std::path::PathBuf;

use crate::agent_host::{
    Capability, LlmError, PermissionClaim, PermissionOperation, PermissionResource, Result,
    ToolDefinition,
    sandbox_gateway::{PreparedSandboxCall, SandboxHandle},
};

pub(crate) fn claims(prepared: &PreparedSandboxCall) -> Result<Vec<PermissionClaim>> {
    prepared
        .claims
        .iter()
        .map(|claim| {
            let operation = match claim.operation.as_str() {
                "read" => PermissionOperation::Read,
                "write" => PermissionOperation::Write,
                "delete" => PermissionOperation::Delete,
                "execute" => PermissionOperation::Execute,
                other => return Err(invalid_claim(format!("unknown operation: {other}"))),
            };
            let resource = match claim.resource_kind.as_str() {
                "filesystem_path" => {
                    PermissionResource::FilesystemPath(PathBuf::from(&claim.resource))
                }
                "working_directory" => {
                    PermissionResource::WorkingDirectory(PathBuf::from(&claim.resource))
                }
                "shell_command" => PermissionResource::ShellCommand(claim.resource.clone()),
                other => return Err(invalid_claim(format!("unknown resource kind: {other}"))),
            };
            Ok(PermissionClaim::new(
                operation,
                resource,
                format!("{} {}", claim.operation, claim.resource),
            ))
        })
        .collect()
}

fn invalid_claim(message: String) -> LlmError {
    LlmError::Protocol {
        provider: "sandbox",
        message,
    }
}

pub(crate) fn agent_tool(definition: ToolDefinition, sandbox: SandboxHandle) -> Capability {
    Capability::worker(definition, sandbox)
}
