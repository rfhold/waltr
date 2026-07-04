//! Shared Waltr shell client reducer, executor, and stable view models.

#![allow(clippy::future_not_send)]

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
    ClientEffect, ClientEvent, ConfigEffect, ConfigEvent, ConfigPatch, ProviderEditBaseline,
    SessionEffect, SessionEvent, StatusEffect, StatusEvent, StreamEvent, StreamId,
};
pub use coordination::runtime::{
    ClientCommand, ClientCommandRequest, ClientOperationView, ClientRuntime,
    ClientSessionTemplateKind, ClientSessionTemplateSearchTarget, ClientSnapshot,
    ClientStateStream, ClientStateStreamEvent, ClientTimelineDirection, ConfigSnapshot,
    OperationCollectionSnapshot, OperationSnapshot, RollbackTargetView, SessionSnapshot,
    SessionTemplateOptionView, SessionTemplateSearchSnapshot, StatusSnapshot,
};
pub use coordination::state::{
    ClientOperationState, ClientState, ConfigClientState, OperationKey, OperationState,
    OperationStatus, SessionClientState, SessionTemplateSearchState, StatusClientState,
    StreamClientState, StreamState, StreamStatus, TimelineWatchKey, WatchState,
    watch_reconnect_delay_ms,
};
pub use error::ClientError;
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
    ConfigWatchUpdate, DefaultGrpcTransport, WaltrTransport, SessionTimelineWatchEvent,
    SessionTimelineWatchUpdate, SessionWatchEvent, SessionWatchUpdate,
};
pub use view::{
    CatalogMatchSourceView, ConfigView, ExternalRequestView, McpServerTypeView, McpServerView,
    ModelCapabilitiesView, ModelCatalogMatchView, ModelCostView, ModelLimitsView, ModelOptionsView,
    ModelTypeView, ModelView, OAuthJwtMetadataView, OAuthProviderView, OAuthTokenFieldView,
    OAuthTokenRequestStyleView, OAuthUrlParamView, ProviderKindView, ProviderView,
    RuntimeConfigMatchView, RuntimeDefaultsView, SessionMcpServerRuntimeView, SessionMetricsView,
    SessionPhaseView, SessionProviderRuntimeView, SessionRuntimeConfigView, SessionStatusView,
    SessionTimelineView, SessionView, TimelineContentView, TimelineEventKindView,
    TimelineEventView, TimelineStatusView, TimestampView,
};
