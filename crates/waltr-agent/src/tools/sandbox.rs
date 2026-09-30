//! Agent-side translation of worker claims and single-use tool tickets.

use std::path::PathBuf;

use crate::{
    AgentTool, LlmError, PermissionClaim, PermissionOperation, PermissionResource, Result,
    ToolDefinition, ToolExecutionContext,
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

pub(crate) fn agent_tool(definition: ToolDefinition, sandbox: SandboxHandle) -> AgentTool {
    AgentTool::new_async(definition, move |context: ToolExecutionContext| {
        let sandbox = sandbox.clone();
        Box::pin(async move {
            let mut context = context;
            let cancellation = context.cancellation_token().clone();
            let progress = context.progress().clone();
            let prepared = match context.prepared_sandbox_call() {
                Some(prepared) => prepared,
                None => return Err(invalid_claim("missing approved sandbox ticket".into())),
            };
            let output = sandbox.execute(prepared, cancellation, progress).await?;
            let result = context.result(output.content);
            Ok(if output.is_error {
                result.with_error()
            } else {
                result
            })
        })
    })
    .with_sandbox_backing()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolCall, tools::filesystem::FilesystemToolKind};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;

    #[tokio::test]
    async fn worker_tool_never_executes_without_an_approved_ticket() {
        let sandbox = crate::sandbox_gateway::SandboxGateway::new("test".into()).handle();
        let tool = agent_tool(FilesystemToolKind::Read.definition(), sandbox);
        let error = tool
            .call(ToolExecutionContext::new(ToolCall::new(
                "call",
                "read",
                serde_json::json!({ "filePath": "note" }),
            )))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing approved sandbox ticket")
        );
    }

    #[tokio::test]
    async fn worker_claims_and_ticket_are_used_for_one_execution() {
        let root = tempfile::tempdir().unwrap();
        let gateway = crate::sandbox_gateway::SandboxGateway::new("test".into());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_gateway = gateway.clone();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(server_gateway.service())
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let root_path = root.path().to_path_buf();
        let worker = tokio::spawn(async move {
            waltr_agent_sandbox::run_worker(channel, root_path, "test")
                .await
                .unwrap();
        });
        gateway.wait_ready().await.unwrap();
        let handle = gateway.handle();
        let arguments = serde_json::json!({"filePath": "note", "content": "from worker"});
        let prepared = handle.prepare("write", &arguments).await.unwrap();
        let claims = claims(&prepared).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].operation, PermissionOperation::Write);
        assert_eq!(
            claims[0].resource,
            PermissionResource::FilesystemPath(root.path().canonicalize().unwrap().join("note"))
        );
        assert!(!root.path().join("note").exists());
        let tool = agent_tool(FilesystemToolKind::Write.definition(), handle);
        let result = tool
            .call(
                ToolExecutionContext::new(ToolCall::new("call", "write", arguments))
                    .with_prepared_sandbox_call(prepared),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(root.path().join("note")).unwrap(),
            "from worker"
        );
        gateway.shutdown();
        worker.abort();
        server.abort();
    }
}
