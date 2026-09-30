//! Session service coordination.

use futures_util::Stream;
use waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer;
use waltr_agent::{
    AgentRunLimits, AgentSession, AgentSessionActiveRunState, AgentSessionRunError,
    AgentSessionRunSubmission, CompletedSessionCapture, FilesystemSessionStore, MemorySessionStore,
    MetadataSetCondition, MetadataSetOutcome, SessionCaptureError,
    SessionCompletionCoordinationHandler, SessionCoordinationReason, SessionCoordinator,
    SessionCreateRequest, SessionId, SessionListUpdate, SessionMaskMutation,
    SessionMaskMutationOutcome, SessionPauseError, SessionPauseLease, SessionStore, SessionSummary,
    TimelineCursor,
};
use std::{pin::Pin, sync::Arc};
use tonic::service::RoutesBuilder;

pub mod service;
pub mod storage;

mod convert;
mod error;
mod inference_settings;
mod timeline_convert;

pub use service::{SessionCatalogSource, SessionRegistrationInput, SessionService};
pub use storage::SessionStorageConfigError;

pub(crate) use storage::SessionStorageConfig;

/// Object-safe access to the coordinator owned by the registered gRPC session service.
#[tonic::async_trait]
pub trait SessionHost: Send + Sync {
    /// Install or remove synchronous terminal-run admission.
    fn set_completion_admission_handler(
        &self,
        handler: Option<Arc<dyn SessionCompletionCoordinationHandler>>,
    );
    /// Create a durable session using server inference defaults and an optional agent selection.
    async fn create(&self, agent_name: Option<String>) -> Result<AgentSession, tonic::Status>;
    /// Create a durable session from an exact host-owned request.
    async fn create_exact(
        &self,
        request: SessionCreateRequest,
    ) -> Result<AgentSession, waltr_agent::LlmError>;
    /// Capture one immutable completed source history seed.
    async fn capture_latest_completed(
        &self,
        id: &SessionId,
    ) -> Result<CompletedSessionCapture, SessionCaptureError>;
    /// Acquire an exclusive process-local pause without waiting for active work.
    async fn acquire_pause_reject_busy(
        &self,
        id: &SessionId,
        reason: SessionCoordinationReason,
    ) -> Result<SessionPauseLease, SessionPauseError>;
    /// Capture mask-effective context through the boundary owned by `lease`.
    async fn capture_paused_effective(
        &self,
        lease: &SessionPauseLease,
    ) -> Result<CompletedSessionCapture, SessionPauseError>;
    /// Apply one atomic mask mutation through the pause's pinned boundary.
    async fn apply_paused_mask_mutation(
        &self,
        lease: &SessionPauseLease,
        mutation: SessionMaskMutation,
    ) -> Result<SessionMaskMutationOutcome, SessionPauseError>;
    /// Arm a continuation from the mutation cursor committed by this pause.
    async fn arm_paused_continuation(
        &self,
        lease: &SessionPauseLease,
        mutation_cursor: TimelineCursor,
        limits: AgentRunLimits,
    ) -> Result<AgentSessionRunSubmission, SessionPauseError>;
    /// Release an owned pause and drain its privileged and ordinary queues.
    async fn release_pause(&self, lease: &SessionPauseLease) -> Result<(), SessionPauseError>;
    /// Start an already durable prompt with ordinary replay semantics.
    async fn start_durable_prompt(
        &self,
        id: SessionId,
        limits: AgentRunLimits,
    ) -> Result<AgentSessionRunSubmission, AgentSessionRunError>;
    /// List durable sessions.
    async fn list(&self) -> Result<Vec<SessionSummary>, waltr_agent::LlmError>;
    /// Load one durable session.
    async fn load(&self, id: &SessionId) -> Result<AgentSession, waltr_agent::LlmError>;
    /// Delete every durable session and clear cached MCP runtimes.
    async fn wipe(&self) -> Result<(), waltr_agent::LlmError>;
    /// Interrupt one session through its coordinator.
    async fn interrupt(&self, id: &SessionId) -> Result<AgentSession, waltr_agent::LlmError>;
    /// Read coordinator-local active-run and queue state.
    async fn active_state(&self, id: &SessionId) -> Option<AgentSessionActiveRunState>;
    /// Submit a prompt using queue-if-active admission.
    async fn submit_prompt(
        &self,
        id: SessionId,
        prompt: String,
    ) -> Result<AgentSessionRunSubmission, AgentSessionRunError>;
    /// Atomically set one metadata value.
    async fn set_metadata_value_conditionally(
        &self,
        id: &SessionId,
        key: String,
        value: serde_json::Value,
        condition: MetadataSetCondition,
    ) -> Result<MetadataSetOutcome, waltr_agent::LlmError>;
    /// Subscribe to coordinator-local session updates.
    fn watch_sessions(&self) -> Pin<Box<dyn Stream<Item = SessionListUpdate> + Send>>;
    /// Permanently stop run admission and cancel process-local session work.
    async fn shutdown(&self);
}

/// Load storage from the environment and register a session host with a live worker.
///
/// # Errors
///
/// Returns an error for invalid storage configuration or a workspace overlap.
pub fn add_service_from_env(
    routes: &mut RoutesBuilder,
    input: SessionRegistrationInput,
) -> Result<Arc<dyn SessionHost>, SessionStorageConfigError> {
    add_storage_service(routes, SessionStorageConfig::from_env()?, input)
}

pub(crate) fn validate_storage_workspace(
    storage: &SessionStorageConfig,
    workspace: &std::path::Path,
) -> Result<(), SessionStorageConfigError> {
    if let SessionStorageConfig::Filesystem { root } = storage {
        let root = if root.is_absolute() {
            root.clone()
        } else {
            workspace.join(root)
        };
        if !crate::workspace_paths::overlaps(workspace, &root).is_ok_and(|overlap| !overlap) {
            return Err(SessionStorageConfigError::Invalid {
                variable: "WALTR_SESSION_FILESYSTEM_ROOT",
                requirement: "must not overlap the worker workspace and must be resolvable",
            });
        }
    }
    Ok(())
}

/// Register the selected storage only after its workspace policy has been validated.
pub(crate) fn add_storage_service(
    routes: &mut RoutesBuilder,
    storage: SessionStorageConfig,
    input: SessionRegistrationInput,
) -> Result<Arc<dyn SessionHost>, SessionStorageConfigError> {
    validate_storage_workspace(&storage, &input.workspace_root)?;
    Ok(match storage {
        SessionStorageConfig::Memory => add_store_service(routes, MemorySessionStore::new(), input),
        SessionStorageConfig::Filesystem { root } => {
            add_store_service(routes, FilesystemSessionStore::new(root), input)
        }
        SessionStorageConfig::S3(config) => add_store_service(routes, config.into_store(), input),
    })
}

/// Register a session service backed by an explicit store.
pub fn add_store_service<S>(
    routes: &mut RoutesBuilder,
    store: S,
    input: SessionRegistrationInput,
) -> Arc<dyn SessionHost>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    add_sessions_service(routes, SessionCoordinator::new(store), input)
}

/// Register a session service backed by an existing coordinator.
pub fn add_sessions_service<S>(
    routes: &mut RoutesBuilder,
    sessions: SessionCoordinator<S>,
    input: SessionRegistrationInput,
) -> Arc<dyn SessionHost>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    let service = SessionService::from_sessions(sessions, input);
    routes.add_service(SessionServiceServer::from_arc(Arc::clone(&service)));
    service
}

#[cfg(test)]
mod sandbox_root_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn session_storage_cannot_alias_the_worker_root() {
        let workspace = crate::test_support::temp_root();
        let check = |root| {
            validate_storage_workspace(&SessionStorageConfig::Filesystem { root }, &workspace)
        };
        assert!(check(workspace.clone()).is_err());
        assert!(check(PathBuf::from(".")).is_err());
        assert!(check(workspace.join("sessions")).is_err());
        assert!(check(workspace.parent().unwrap().to_path_buf()).is_err());
        assert!(check(workspace.parent().unwrap().join("other-sessions")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_parent_and_missing_nested_store_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = crate::test_support::temp_root();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        symlink(&workspace, root.join("alias")).unwrap();
        let check = |root| {
            validate_storage_workspace(&SessionStorageConfig::Filesystem { root }, &workspace)
        };
        assert!(check(root.join("alias/sessions/missing")).is_err());
        assert!(check(root.join("alias")).is_err());
        let session = root.join("sessions");
        std::fs::create_dir(&session).unwrap();
        symlink(&root, root.join("root-alias")).unwrap();
        assert!(
            validate_storage_workspace(
                &SessionStorageConfig::Filesystem { root: session },
                &root.join("root-alias"),
            )
            .is_err()
        );
    }
}
