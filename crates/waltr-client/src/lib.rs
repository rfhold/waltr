#![allow(
    clippy::result_large_err,
    reason = "preserve the established ClientError API and payloads"
)]

//! Shared Waltr shell client reducer, executor, and stable view models.

mod client;
mod coordination;
mod error;
mod executor;
mod resource;
mod routine;
mod transport;
mod view;

#[cfg(all(target_arch = "wasm32", feature = "wasm"))]
mod wasm;

pub use client::{Client, ClientUpdate};
pub use coordination::event::{
    AccessEvent, AuthEffect, AuthEvent, CatalogEffect, CatalogEvent, ClientEffect, ClientEvent,
    ConfigEffect, ConfigEvent, ProviderEditBaseline, ResourceWrite, RoutineEffect, RoutineEvent,
    SessionEffect, SessionEvent, StatusEffect, StatusEvent, StreamEvent, StreamId,
};
pub use coordination::runtime::{
    AccessSnapshot, AuthAttemptSnapshot, AuthSnapshot, AuthStatusSnapshot,
    AuthenticationFailureSnapshot, CatalogMatchSuggestionSnapshot, CatalogModelSearchSnapshot,
    CatalogSnapshot, ClientAuthTarget, ClientCommand, ClientCommandRequest, ClientOperationView,
    ClientRuntime, ClientSessionTemplateKind, ClientSessionTemplateSearchTarget, ClientSnapshot,
    ClientStateStream, ClientStateStreamEvent, ClientTimelineDirection, ConfigSnapshot,
    McpAuthorizationContextSnapshot, OperationCollectionSnapshot, OperationSnapshot,
    ResourceEditBaselineView, RollbackTargetView, SessionAgentCatalogSnapshot, SessionSnapshot,
    SessionTemplateOptionView, SessionTemplateSearchSnapshot, StatusSnapshot,
};
pub use coordination::state::{
    AccessAttemptId, AccessClientState, AccessStatus, AuthAttemptState, AuthClientState,
    CatalogClientState, CatalogMatchSuggestionState, CatalogModelSearchState, ClientOperationState,
    ClientState, ConfigClientState, OperationKey, OperationState, OperationStatus,
    RoutineClientState, RoutineDetailState, RoutinePageState, RoutineStartOperationState,
    SessionAgentCatalogState, SessionClientState, SessionRoutineHistoryState,
    SessionTemplateSearchState, StatusClientState, StreamClientState, StreamState, StreamStatus,
    TimelineWatchKey, WatchState, watch_reconnect_delay_ms,
};
pub use error::{
    ClientError, ClientErrorKind, McpAuthorizationContext, OutboundAuthenticationFailure,
};
pub use executor::{
    ConfigChange, ConfigChangeStream, GrpcEffectExecutor, SessionChangeStream, StatusChangeStream,
};
pub use waltr::grpc::waltr::catalog::v1::{
    CatalogDefaultField, CatalogDriftField, CatalogDriftSeverity, CatalogMatch,
    CatalogMatchConfidence, CatalogModelCapabilitySummary, CatalogModelCostSummary,
    CatalogModelLimitsSummary, CatalogModelReasoningSummary, CatalogModelRef, CatalogModelSummary,
    GetModelCatalogDriftRequest, GetModelCatalogDriftResponse, SearchCatalogModelsRequest,
    SearchCatalogModelsResponse, SuggestCatalogMatchesRequest, SuggestCatalogMatchesResponse,
};
pub use waltr::grpc::waltr::config::v1::{
    CatalogMatchSource, Configuration, InferenceDefaults, McpServer, McpServerType, Model,
    ModelCapabilities, ModelCatalogMatch, ModelCost, ModelLimits, ModelOptions, ModelType,
    OAuthProvider, OauthJwtMetadata, OauthTokenField, OauthTokenRequestStyle, OauthUrlParam,
    Provider, ProviderType,
};
pub use waltr::grpc::waltr::resource::v1::{
    Resource, ResourceCatalog, ResourceInspection, ResourceKind, ResourceList, ResourceObject,
    ResourceOrigin, ResourceValue,
};
pub use waltr::grpc::waltr::session::v1::{
    ExternalRequest, ExternalRequestKind, InferenceProviderSettings, InferenceSettings,
    InferenceSettingsMatch, InferenceSettingsRequest, ListAgentsResponse, PendingWork,
    PendingWorkStatus, RollbackTarget, SearchSessionTemplatesRequest,
    SearchSessionTemplatesResponse, SelectableAgent, SessionMetrics, SessionPhase, SessionStatus,
    SessionSummary, SessionTemplateKind, SessionTemplateOption, SessionTemplateSearchTarget,
    Suspension, SuspensionKind, TimelineContent, TimelineDirection, TimelineEvent,
    TimelineEventKind, TimelineQuery, TimelineStatus, TimelineTruncation, rollback_target,
    search_session_templates_response,
};
pub use waltr::grpc::waltr::status::v1::SystemStatus;
pub use routine::{
    ClientManualRoutineDefinition, ClientRoutineAdmission, ClientRoutineDefinitionSnapshot,
    ClientRoutineEventKind, ClientRoutineFailure, ClientRoutineFieldViolation,
    ClientRoutineInputDefinition, ClientRoutineInvocationDetail, ClientRoutineInvocationEvent,
    ClientRoutineInvocationPage, ClientRoutineInvocationSnapshot, ClientRoutineInvocationStatus,
    ClientRoutineInvocationSummary, ClientRoutineInvocationWatchUpdate, ClientRoutineResult,
    ClientRoutineResultDefinition, ClientRoutineScope, ClientRoutineStartOutcome,
    ClientRoutineTask, ClientRoutineTaskDescriptor, ClientRoutineTaskStatus,
    ClientRoutineTriggerKind, ClientRoutineValidationFailure, ClientRoutineValue,
    ClientRoutineValueType, MAX_RETAINED_ROUTINE_DETAILS, MAX_RETAINED_ROUTINE_INVOCATIONS,
    MAX_RETAINED_ROUTINE_STARTS, ROUTINE_INVOCATION_SNAPSHOT_SIZE,
    ROUTINE_SOURCE_SESSION_ID_METADATA_KEY, StartManualRoutineArgs,
};
pub use transport::{
    AuthStatus, AuthTarget, ConfigWatchUpdate, DefaultGrpcTransport, GenericAuthAttempt,
    GenericAuthAttemptStatus, WaltrTransport, RoutineDefinitionWatchStream,
    RoutineInvocationWatchStream, SessionTimelineWatchEvent, SessionTimelineWatchUpdate,
    SessionWatchEvent, SessionWatchUpdate,
};
#[cfg(not(target_arch = "wasm32"))]
pub use transport::{NativeCredentialLease, NativeCredentialProvider, NativeGrpcTransport};
pub use view::{
    CatalogMatchConfidenceView, CatalogMatchSourceView, CatalogMatchView,
    CatalogModelCapabilitySummaryView, CatalogModelCostSummaryView, CatalogModelLimitsSummaryView,
    CatalogModelReasoningSummaryView, CatalogModelRefView, CatalogModelSummaryView, ConfigView,
    ExternalRequestView, InferenceDefaultsView, InferenceProviderSettingsView,
    InferenceSettingsMatchView, InferenceSettingsView, McpServerTypeView, McpServerView,
    ModelCapabilitiesView, ModelCatalogMatchView, ModelCostView, ModelLimitsView, ModelOptionsView,
    ModelTypeView, ModelView, OAuthJwtMetadataView, OAuthProviderView, OAuthTokenFieldView,
    OAuthTokenRequestStyleView, OAuthUrlParamView, ProviderKindView, ProviderView,
    SelectableAgentView, SessionAssociationsView, SessionMetricsView, SessionPhaseView,
    SessionStatusView, SessionSuspensionKindView, SessionTimelineView, SessionView,
    TimelineContentView, TimelineEventKindView, TimelineEventView, TimelineStatusView,
    TimestampView,
};

#[cfg(test)]
mod direct_transport_tests {
    #[test]
    fn wasm_binding_does_not_reference_a_direct_default_transport() {
        let source = include_str!("wasm.rs");
        let forbidden = concat!("Default", "GrpcTransport");

        assert!(!source.contains(forbidden));
    }
}
