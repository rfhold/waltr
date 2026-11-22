//! Session service coordination.

use futures_util::Stream;
use std::{pin::Pin, sync::Arc};
use tonic::service::RoutesBuilder;
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

pub mod image_objects;
pub mod image_uploads;
pub mod service;
pub mod storage;

mod convert;
mod error;
mod inference_settings;
pub mod interactions;
mod mcp_apps;
mod timeline_convert;

pub(crate) use service::resolve_mcp_connections;
pub use service::{SessionCatalogSource, SessionRegistrationInput, SessionService};
pub use storage::SessionStorageConfigError;

pub(crate) use storage::SessionStorageConfig;

pub(crate) fn pinned_inference_settings(
    provider: String,
    model: String,
    configuration: &waltr::grpc::waltr::config::v1::Configuration,
) -> Result<waltr_agent::InferenceSettings, tonic::Status> {
    inference_settings::resolve_inference_settings(
        waltr::grpc::waltr::session::v1::InferenceSettingsRequest {
            provider_key: provider,
            model_id: model,
            reasoning_effort: None,
        },
        configuration,
    )
    .map(waltr_agent::InferenceSettings::with_reasoning_disabled)
}

/// Object-safe access to the coordinator owned by the registered gRPC session service.
#[tonic::async_trait]
pub trait SessionHost: Send + Sync {
    /// Install an opaque private self binding composed by the owning application.
    fn install_self_mcp(&self, _binding: crate::self_mcp::SelfMcpBinding) {}
    /// Compose the application's single Routine runtime with this concrete normal coordinator.
    async fn compose_routines(
        &self,
        _input: crate::routine::production::ProductionInput,
    ) -> Result<Arc<crate::routine::production::ProductionRuntime>, crate::routine::ports::PortError>
    {
        Err(crate::routine::ports::PortError::UnsupportedCapability)
    }
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
    /// Admit durable work with current authentication; later credential expiry does not cancel it.
    async fn submit_prompt_authorized(
        &self,
        _id: SessionId,
        _prompt: String,
        _principal: crate::access_auth::AuthenticatedPrincipal,
    ) -> Result<OriginSubmission, tonic::Status> {
        Err(tonic::Status::permission_denied(
            "origin binding unavailable",
        ))
    }
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
    /// Subscribe before starting a helper; reset notifications must never be ignored.
    fn observe_timeline(
        &self,
        _id: SessionId,
    ) -> Option<waltr_agent::SessionTimelineProjectionSubscription> {
        None
    }
    /// Permanently stop run admission and cancel process-local session work.
    async fn shutdown(&self);
}

/// Admission receipt for a run retained by its origin-lifetime owner.
#[derive(Debug)]
pub struct OriginSubmission {
    /// Durable target session.
    pub session_id: SessionId,
    /// Exact coordinator submission identifier.
    pub run_id: u64,
    /// Ordinary admission disposition.
    pub disposition: waltr_agent::AgentSessionRunDisposition,
}

/// Load storage from the environment and register a session host with a live worker.
///
/// # Errors
///
/// Returns an error for invalid storage configuration.
pub fn add_service_from_env(
    routes: &mut RoutesBuilder,
    input: SessionRegistrationInput,
) -> Result<Arc<dyn SessionHost>, SessionStorageConfigError> {
    add_storage_service(routes, SessionStorageConfig::from_env()?, input)
}

/// Register the configured session store.
pub(crate) fn add_storage_service(
    routes: &mut RoutesBuilder,
    storage: SessionStorageConfig,
    input: SessionRegistrationInput,
) -> Result<Arc<dyn SessionHost>, SessionStorageConfigError> {
    match storage {
        SessionStorageConfig::Memory => {
            Ok(add_store_service(routes, MemorySessionStore::new(), input))
        }
        SessionStorageConfig::Filesystem { root } => {
            let images = image_objects::SessionImageObjects::filesystem(&root).map_err(|_| {
                SessionStorageConfigError::Invalid {
                    variable: "WALTR_SESSION_FILESYSTEM_ROOT",
                    requirement: "must have a usable sibling directory for message images",
                }
            })?;
            Ok(add_image_sessions_service(
                routes,
                SessionCoordinator::new(FilesystemSessionStore::new(root)),
                input,
                images,
            ))
        }
        SessionStorageConfig::S3(config) => {
            let (store, images) = config.into_stores()?;
            Ok(add_image_sessions_service(
                routes,
                SessionCoordinator::new(store),
                input,
                images,
            ))
        }
    }
}

/// Register a session service whose records and images are both process-local.
pub fn add_store_service(
    routes: &mut RoutesBuilder,
    store: MemorySessionStore,
    input: SessionRegistrationInput,
) -> Arc<dyn SessionHost> {
    add_image_sessions_service(
        routes,
        SessionCoordinator::new(store),
        input,
        image_objects::SessionImageObjects::memory(service::MEMORY_IMAGE_OBJECT_BYTES),
    )
}

/// Register a test session service backed by an existing coordinator.
#[cfg(any(debug_assertions, feature = "test"))]
pub(crate) fn add_sessions_service(
    routes: &mut RoutesBuilder,
    sessions: SessionCoordinator<MemorySessionStore>,
    input: SessionRegistrationInput,
) -> Arc<dyn SessionHost> {
    add_image_sessions_service(
        routes,
        sessions,
        input,
        image_objects::SessionImageObjects::memory(service::MEMORY_IMAGE_OBJECT_BYTES),
    )
}

/// Register a service with explicitly paired record and image stores.
pub fn add_image_sessions_service<S>(
    routes: &mut RoutesBuilder,
    sessions: SessionCoordinator<S>,
    input: SessionRegistrationInput,
    images: image_objects::SessionImageObjects,
) -> Arc<dyn SessionHost>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    let service = SessionService::from_sessions_with_images(sessions, input, images);
    routes.add_service(
        waltr::grpc::waltr::session::v1::interaction_service_server::InteractionServiceServer::new(
            interactions::InteractionService::new(service.clone(), service.interactions.clone()),
        ),
    );
    routes.add_service(SessionServiceServer::from_arc(Arc::clone(&service)));
    service
}

#[derive(Debug)]
pub(crate) struct UnboundSessionEvents;

impl waltr_agent::SessionEventSink for UnboundSessionEvents {
    fn send(&self, _: waltr_agent::SessionEvent) -> waltr_agent::SessionEventSinkFuture {
        Box::pin(async { Ok(()) })
    }
}
