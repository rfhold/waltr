use super::*;
use crate::agent_host::sandbox_gateway::{SandboxHandle, WorkspaceScope, peers::MachineRegistry};
use waltr::grpc::waltr::session::v1::WorkspaceSelection;
use waltr_agent::{AgentSession, SessionCreateRequest};

impl WorkspaceScope {
    pub(crate) fn cwd(&self) -> PathBuf {
        if self.working_directory == "." {
            self.canonical_root.clone()
        } else {
            self.canonical_root.join(&self.working_directory)
        }
    }
}

impl SandboxHandle {
    pub(crate) fn export_root(&self) -> Option<PathBuf> {
        self.scope()
            .map(|scope| scope.canonical_root.clone())
            .or_else(|| self.canonical_root())
    }
}

impl ServerAgentRuntimeFactory {
    pub(crate) fn workspace_machines(&self) -> &MachineRegistry {
        &self.workspace_machines
    }

    pub(crate) fn with_workspace_machines(mut self, machines: MachineRegistry) -> Self {
        self.workspace_machines = machines;
        self
    }

    #[must_use]
    pub(crate) fn with_workspace_account(mut self, account: Option<WorkspaceAccount>) -> Self {
        self.workspace_account = account;
        self
    }

    pub(crate) async fn resolve_workspace(
        &self,
        selection: &WorkspaceSelection,
    ) -> Result<SandboxHandle, tonic::Status> {
        let cwd = selection.working_directory.as_deref();
        if selection.machine_id == "local" {
            self.sandbox.bind_directory(cwd.unwrap_or("")).await
        } else {
            let account = self.workspace_account.as_ref().ok_or_else(|| {
                tonic::Status::unauthenticated("remote workspace requires a verified account")
            })?;
            self.workspace_machines
                .resolve(&selection.machine_id, cwd, account)
                .await
        }
        .map_err(|e| tonic::Status::failed_precondition(e.to_string()))
    }

    fn workspace_account_for_scope(
        &self,
        scope: &WorkspaceScope,
    ) -> Result<&WorkspaceAccount, tonic::Status> {
        let owner = scope.account.as_ref().ok_or_else(|| {
            tonic::Status::failed_precondition("remote workspace has no durable account")
        })?;
        let account = self.workspace_account.as_ref().ok_or_else(|| {
            tonic::Status::unauthenticated("remote workspace requires a verified account")
        })?;
        if account != owner {
            return Err(tonic::Status::permission_denied(
                "remote workspace belongs to another account",
            ));
        }
        Ok(account)
    }

    async fn restore_workspace(
        &self,
        scope: &WorkspaceScope,
    ) -> Result<SandboxHandle, tonic::Status> {
        if scope.machine_id == "local" {
            let handle = self
                .sandbox
                .bind_directory(&format!("./{}", scope.working_directory))
                .await
                .map_err(|e| tonic::Status::failed_precondition(e.to_string()))?;
            if handle.scope() != Some(scope) {
                return Err(tonic::Status::failed_precondition(
                    "workspace export or directory changed",
                ));
            }
            return Ok(handle);
        }
        let account = self.workspace_account_for_scope(scope)?;
        self.workspace_machines
            .restore(scope, account)
            .await
            .map_err(|e| tonic::Status::failed_precondition(e.to_string()))
    }

    fn stored_workspace(record: &SessionRecord) -> Result<Option<WorkspaceScope>, tonic::Status> {
        let bundle = record
            .agent_definition
            .clone()
            .map(serde_json::from_value::<AgentDefinitionBundle>)
            .transpose()
            .map_err(|_| {
                tonic::Status::failed_precondition("invalid stored agent configuration")
            })?;
        Ok(bundle.and_then(|bundle| bundle.workspace))
    }

    /// Check a remote workspace before a dispatch path can mutate the durable session.
    /// Local and unbound sessions do not need a caller account or worker preflight.
    /// Returns whether the record uses a remote workspace.
    pub(crate) async fn authorize_session_workspace(
        &self,
        record: &SessionRecord,
    ) -> Result<bool, tonic::Status> {
        if let Some(scope) = Self::stored_workspace(record)?
            && scope.machine_id != "local"
        {
            self.restore_workspace(&scope).await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Resolve a requested or trusted source workspace before the single durable create.
    pub(crate) async fn create_with_workspace<S>(
        &self,
        sessions: &SessionCoordinator<S>,
        mut request: SessionCreateRequest,
        explicit: Option<&WorkspaceSelection>,
        source_definition: Option<&serde_json::Value>,
    ) -> Result<AgentSession, tonic::Status>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let invalid = |_| tonic::Status::failed_precondition("invalid stored agent configuration");
        let mut bundle = request
            .agent_definition
            .take()
            .map(serde_json::from_value::<AgentDefinitionBundle>)
            .transpose()
            .map_err(invalid)?;
        let handle = if let Some(selection) = explicit {
            Some(self.resolve_workspace(selection).await)
        } else {
            let source = source_definition
                .cloned()
                .map(serde_json::from_value::<AgentDefinitionBundle>)
                .transpose()
                .map_err(invalid)?;
            match source
                .as_ref()
                .or(bundle.as_ref())
                .and_then(|b| b.workspace.as_ref())
            {
                Some(scope) => Some(self.restore_workspace(scope).await),
                None => None,
            }
        }
        .transpose()?;
        if let Some(handle) = handle {
            let mut destination = match bundle.take() {
                Some(bundle) => bundle,
                None => self
                    .bundle_for_agent(self.default_agent_name())
                    .map_err(|e| tonic::Status::failed_precondition(e.to_string()))?,
            };
            destination.workspace = Some(
                handle
                    .scope()
                    .cloned()
                    .ok_or_else(|| tonic::Status::internal("worker returned an unbound handle"))?,
            );
            bundle = Some(destination);
        } else if let Some(destination) = &mut bundle {
            destination.workspace = None;
        }
        request.agent_definition = bundle
            .map(serde_json::to_value)
            .transpose()
            .map_err(invalid)?;
        sessions
            .create(request)
            .await
            .map_err(|e| tonic::Status::internal(e.to_string()))
    }

    pub(crate) async fn for_session_workspace(
        &self,
        record: &SessionRecord,
    ) -> Result<Self, AgentSessionRuntimeError> {
        let mut factory = self.clone();
        if let Some(scope) = Self::stored_workspace(record).map_err(workspace_runtime_error)? {
            factory.sandbox = self
                .restore_workspace(&scope)
                .await
                .map_err(workspace_runtime_error)?;
            factory.root = scope.canonical_root;
        } else {
            match self.sandbox_root() {
                Ok(root) => factory.root = root,
                Err(error) => {
                    let features = self.definition_for_session(record)?.features;
                    if features.mcp.is_none()
                        || features.filesystem.is_some()
                        || features.agents.is_some()
                        || features.skills.is_some()
                        || features.shell.is_some()
                    {
                        return Err(error);
                    }
                }
            }
        }
        Ok(factory)
    }
}

fn workspace_runtime_error(error: tonic::Status) -> AgentSessionRuntimeError {
    AgentSessionRuntimeError::FailedPrecondition(error.message().to_owned())
}
