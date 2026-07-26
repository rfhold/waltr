//! Shared Waltr shell client reducer, executor, and stable view models.

mod client;
mod coordination;
mod error;
mod executor;
mod transport;
mod view;

#[cfg(all(target_arch = "wasm32", feature = "wasm"))]
mod wasm;

pub use client::{Client, ClientUpdate};
pub use coordination::event::{
    AccessEvent, AuthEffect, AuthEvent, CatalogEffect, CatalogEvent, ClientEffect, ClientEvent,
    ConfigEffect, ConfigEvent, ConfigPatch, ProviderEditBaseline, SessionEffect, SessionEvent,
    StatusEffect, StatusEvent, StreamEvent, StreamId,
};
pub use coordination::runtime::{
    AccessSnapshot, AuthAttemptSnapshot, AuthRequirementSnapshot, AuthSnapshot,
    CatalogMatchSuggestionSnapshot, CatalogModelSearchSnapshot, CatalogSnapshot, ClientAuthTarget,
    ClientCommand, ClientCommandRequest, ClientOperationView, ClientRuntime,
    ClientSessionTemplateKind, ClientSessionTemplateSearchTarget, ClientSnapshot,
    ClientStateStream, ClientStateStreamEvent, ClientTimelineDirection, ConfigSnapshot,
    McpAuthorizationRequirementSnapshot, OperationCollectionSnapshot, OperationSnapshot,
    RollbackTargetView, SessionSnapshot, SessionTemplateOptionView, SessionTemplateSearchSnapshot,
    StatusSnapshot,
};
pub use coordination::state::{
    AccessAttemptId, AccessClientState, AccessStatus, AuthAttemptState, AuthClientState,
    AuthRequirement, CatalogClientState, CatalogMatchSuggestionState, CatalogModelSearchState,
    ClientOperationState, ClientState, ConfigClientState, McpAuthorizationRequirement,
    OperationKey, OperationState, OperationStatus, SessionClientState, SessionTemplateSearchState,
    StatusClientState, StreamClientState, StreamState, StreamStatus, TimelineWatchKey, WatchState,
    watch_reconnect_delay_ms,
};
pub use error::{ClientError, ClientErrorKind};
pub use executor::{
    ConfigChange, ConfigChangeStream, GrpcEffectExecutor, SessionChangeStream, StatusChangeStream,
};
pub use waltr::grpc::waltr::catalog::v1::{
    ApplyCatalogModelDefaultsRequest, ApplyCatalogModelDefaultsResponse, CatalogDefaultField,
    CatalogDriftField, CatalogDriftSeverity, CatalogMatch, CatalogMatchConfidence,
    CatalogModelCapabilitySummary, CatalogModelCostSummary, CatalogModelLimitsSummary,
    CatalogModelReasoningSummary, CatalogModelRef, CatalogModelSummary,
    GetModelCatalogDriftRequest, GetModelCatalogDriftResponse, SearchCatalogModelsRequest,
    SearchCatalogModelsResponse, SetModelCatalogMatchRequest, SetModelCatalogMatchResponse,
    SuggestCatalogMatchesRequest, SuggestCatalogMatchesResponse,
};
pub use waltr::grpc::waltr::config::v1::{
    CatalogMatchSource, Configuration, McpServer, McpServerType, Model, ModelCapabilities,
    ModelCatalogMatch, ModelCost, ModelLimits, ModelOptions, ModelType, OAuthProvider,
    OauthJwtMetadata, OauthTokenField, OauthTokenRequestStyle, OauthUrlParam, Provider,
    ProviderType, RuntimeDefaults,
};
pub use waltr::grpc::waltr::session::v1::{
    ExternalRequest, ExternalRequestKind, PendingWork, PendingWorkStatus, RollbackTarget,
    RuntimeConfigMatch, SearchSessionTemplatesRequest, SearchSessionTemplatesResponse,
    SessionMcpServerRuntime, SessionMetrics, SessionPhase, SessionProviderRuntime,
    SessionRuntimeConfig, SessionRuntimeConfigRequest, SessionStatus, SessionSummary,
    SessionTemplateKind, SessionTemplateOption, SessionTemplateSearchTarget, Suspension,
    SuspensionKind, TimelineContent, TimelineDirection, TimelineEvent, TimelineEventKind,
    TimelineQuery, TimelineStatus, TimelineTruncation, rollback_target,
    search_session_templates_response,
};
pub use waltr::grpc::waltr::status::v1::SystemStatus;
pub use transport::{
    AuthAttempt, AuthStatus, AuthTarget, ConfigWatchUpdate, DefaultGrpcTransport,
    GenericAuthAttempt, GenericAuthAttemptStatus, WaltrTransport, ProviderAuthStatus,
    SessionTimelineWatchEvent, SessionTimelineWatchUpdate, SessionWatchEvent, SessionWatchUpdate,
};
#[cfg(not(target_arch = "wasm32"))]
pub use transport::{NativeCredentialLease, NativeCredentialProvider, NativeGrpcTransport};
pub use view::{
    CatalogMatchConfidenceView, CatalogMatchSourceView, CatalogMatchView,
    CatalogModelCapabilitySummaryView, CatalogModelCostSummaryView, CatalogModelLimitsSummaryView,
    CatalogModelReasoningSummaryView, CatalogModelRefView, CatalogModelSummaryView, ConfigView,
    ExternalRequestView, McpServerTypeView, McpServerView, ModelCapabilitiesView,
    ModelCatalogMatchView, ModelCostView, ModelLimitsView, ModelOptionsView, ModelTypeView,
    ModelView, OAuthJwtMetadataView, OAuthProviderView, OAuthTokenFieldView,
    OAuthTokenRequestStyleView, OAuthUrlParamView, ProviderKindView, ProviderView,
    RuntimeConfigMatchView, RuntimeDefaultsView, SessionMcpServerRuntimeView, SessionMetricsView,
    SessionPhaseView, SessionProviderRuntimeView, SessionRuntimeConfigView, SessionStatusView,
    SessionTimelineView, SessionView, TimelineContentView, TimelineEventKindView,
    TimelineEventView, TimelineStatusView, TimestampView,
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
