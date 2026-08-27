//! Stable shell-facing view models.

mod catalog;
mod config;
mod session;

pub use catalog::{
    CatalogMatchConfidenceView, CatalogMatchView, CatalogModelCapabilitySummaryView,
    CatalogModelCostSummaryView, CatalogModelLimitsSummaryView, CatalogModelReasoningSummaryView,
    CatalogModelRefView, CatalogModelSummaryView,
};
pub use config::{
    CatalogMatchSourceView, ConfigView, InferenceDefaultsView, McpServerTypeView, McpServerView,
    ModelCapabilitiesView, ModelCatalogMatchView, ModelCostView, ModelLimitsView, ModelOptionsView,
    ModelTypeView, ModelView, OAuthJwtMetadataView, OAuthProviderView, OAuthTokenFieldView,
    OAuthTokenRequestStyleView, OAuthUrlParamView, ProviderKindView, ProviderView,
};
pub use session::{
    ExternalRequestView, InferenceProviderSettingsView, InferenceSettingsMatchView,
    InferenceSettingsView, ProjectedTimelineEntryView, SelectableAgentView,
    SessionAssociationsView, SessionMetricsView, SessionPhaseView, SessionStatusView,
    SessionSuspensionKindView, SessionTimelineView, SessionView, SyntheticTimelineEventView,
    TimelineContentView, TimelineEventKindView, TimelineEventView, TimelinePresentationView,
    TimelineStatusView, TimestampView,
};

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs, path::PathBuf};

    use ts_rs::TS as _;

    use crate::{
        AccessSnapshot, AccessStatus, AuthAttemptSnapshot, AuthSnapshot, AuthStatusSnapshot,
        AuthenticationFailureSnapshot, CatalogMatchConfidenceView, CatalogMatchSource,
        CatalogMatchSuggestionSnapshot, CatalogMatchView, CatalogModelCapabilitySummaryView,
        CatalogModelCostSummaryView, CatalogModelLimitsSummaryView,
        CatalogModelReasoningSummaryView, CatalogModelRefView, CatalogModelSearchSnapshot,
        CatalogModelSummaryView, CatalogSnapshot, ClientAuthTarget, ClientCommand,
        ClientManualRoutineDefinition, ClientOperationView, ClientRoutineFailure,
        ClientRoutineFieldViolation, ClientRoutineInputDefinition, ClientRoutineInvocationDetail,
        ClientRoutineInvocationStatus, ClientRoutineInvocationSummary, ClientRoutineResult,
        ClientRoutineResultDefinition, ClientRoutineScope, ClientRoutineTask,
        ClientRoutineTaskStatus, ClientRoutineTriggerKind, ClientRoutineValidationFailure,
        ClientRoutineValue, ClientRoutineValueType, ClientSessionTemplateKind,
        ClientSessionTemplateSearchTarget, ClientSnapshot, ClientTimelineDirection, ConfigSnapshot,
        Configuration, McpAuthorizationContextSnapshot, Model, ModelCapabilities,
        ModelCatalogMatch, ModelLimits, ModelOptions, ModelType, OperationCollectionSnapshot,
        OperationSnapshot, Provider, ProviderType, ResourceEditBaselineView, RollbackTargetView,
        RoutineDefinitionSnapshot, RoutineDetailSnapshot, RoutineHistorySnapshot, RoutineSnapshot,
        RoutineStartOperationView, SessionAgentCatalogSnapshot, SessionSnapshot,
        SessionTemplateOptionView, SessionTemplateSearchSnapshot, StatusSnapshot,
    };

    use super::*;

    #[test]
    fn config_view_round_trips_known_provider_types() {
        let config = Configuration {
            providers: HashMap::from([(
                "anthropic".to_owned(),
                Provider {
                    r#type: ProviderType::AnthropicMessages as i32,
                    name: "anthropic".to_owned(),
                    url: "https://api.anthropic.com/v1/messages".to_owned(),
                    headers: HashMap::from([("x-test".to_owned(), "true".to_owned())]),
                    oauth_compatible: true,
                    oauth_settings: None,
                    models: HashMap::new(),
                },
            )]),
            etag: "v1".to_owned(),
            mcp_servers: HashMap::new(),
            inference_defaults: None,
        };

        let view = ConfigView::from(config.clone());
        assert_eq!(
            view.providers["anthropic"].kind,
            ProviderKindView::AnthropicMessages
        );

        let round_trip = Configuration::from(view);
        assert_eq!(round_trip, config);
    }

    #[test]
    fn config_view_maps_unknown_provider_type_to_unknown() {
        let provider = Provider {
            r#type: 99,
            ..Provider::default()
        };

        assert_eq!(ProviderView::from(provider).kind, ProviderKindView::Unknown);
    }

    #[test]
    fn config_view_round_trips_model_catalog_match() {
        let model = Model {
            name: "gpt-5".to_owned(),
            r#type: ModelType::Llm as i32,
            options: Some(ModelOptions::default()),
            limits: Some(ModelLimits::default()),
            capabilities: Some(ModelCapabilities::default()),
            catalog_match: Some(ModelCatalogMatch {
                provider_id: "openai".to_owned(),
                model_id: "gpt-5".to_owned(),
                upstream_provider_id: Some("openai".to_owned()),
                upstream_model_id: Some("gpt-5-2025-08-07".to_owned()),
                source: CatalogMatchSource::Applied as i32,
            }),
            ..Model::default()
        };

        let view = ModelView::from(model.clone());
        assert_eq!(
            view.catalog_match
                .as_ref()
                .map(|match_view| match_view.source),
            Some(CatalogMatchSourceView::Applied)
        );

        assert_eq!(Model::from(view), model);
    }

    #[test]
    fn config_view_round_trips_absent_upstream_catalog_identity() {
        let model = Model {
            name: "gpt-5".to_owned(),
            r#type: ModelType::Llm as i32,
            options: Some(ModelOptions::default()),
            limits: Some(ModelLimits::default()),
            capabilities: Some(ModelCapabilities::default()),
            catalog_match: Some(ModelCatalogMatch {
                provider_id: "openai".to_owned(),
                model_id: "gpt-5".to_owned(),
                upstream_provider_id: None,
                upstream_model_id: None,
                source: CatalogMatchSource::Manual as i32,
            }),
            ..Model::default()
        };

        let view = ModelView::from(model.clone());
        let catalog_match = view
            .catalog_match
            .as_ref()
            .expect("catalog match should be exposed");
        assert_eq!(catalog_match.upstream_provider_id, None);
        assert_eq!(catalog_match.upstream_model_id, None);

        assert_eq!(Model::from(view), model);
    }

    #[test]
    fn failed_session_view_retains_structured_authentication_failure() {
        use waltr::grpc::waltr::auth::v1::{
            AuthTarget, AuthenticationDomain, AuthenticationFailure, AuthenticationFailureReason,
            McpAuthorizationContext, auth_target,
        };

        let view = SessionStatusView::from(crate::SessionStatus {
            phase: crate::SessionPhase::Failed as i32,
            failed_message: "safe watched failure".to_owned(),
            authentication_failure: Some(AuthenticationFailure {
                version: 1,
                domain: AuthenticationDomain::OutboundMcp as i32,
                reason: AuthenticationFailureReason::ResponseInvalid as i32,
                target: Some(AuthTarget {
                    target: Some(auth_target::Target::McpServerKey("github".to_owned())),
                }),
                mcp_authorization: Some(McpAuthorizationContext {
                    endpoint: "https://mcp.example/rpc".to_owned(),
                    http_status: 401,
                    resource_metadata_url: Some("javascript:unsafe".to_owned()),
                    scope: Some("repo:read".to_owned()),
                    ..McpAuthorizationContext::default()
                }),
            }),
            ..crate::SessionStatus::default()
        });

        assert_eq!(view.failed_message, "safe watched failure");
        let failure = view.authentication_failure.expect("authentication failure");
        assert_eq!(failure.version, 1);
        assert_eq!(failure.domain, "outboundMcp");
        assert_eq!(failure.reason, "responseInvalid");
        assert_eq!(failure.target.key, "github");
        let context = failure.mcp_context.expect("MCP context");
        assert_eq!(context.endpoint.as_deref(), Some("https://mcp.example/rpc"));
        assert_eq!(context.scope.as_deref(), Some("repo:read"));
        assert!(context.resource_metadata_url.is_none());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn exports_web_typescript_bindings() {
        let bindings = [
            ProviderKindView::decl(),
            ModelTypeView::decl(),
            CatalogMatchSourceView::decl(),
            ModelOptionsView::decl(),
            ModelLimitsView::decl(),
            ModelCapabilitiesView::decl(),
            ModelCostView::decl(),
            ModelCatalogMatchView::decl(),
            ModelView::decl(),
            OAuthTokenRequestStyleView::decl(),
            OAuthUrlParamView::decl(),
            OAuthTokenFieldView::decl(),
            OAuthJwtMetadataView::decl(),
            OAuthProviderView::decl(),
            McpServerTypeView::decl(),
            McpServerView::decl(),
            InferenceDefaultsView::decl(),
            ProviderView::decl(),
            ConfigView::decl(),
            CatalogModelRefView::decl(),
            CatalogModelLimitsSummaryView::decl(),
            CatalogModelCostSummaryView::decl(),
            CatalogModelCapabilitySummaryView::decl(),
            CatalogModelReasoningSummaryView::decl(),
            CatalogModelSummaryView::decl(),
            CatalogMatchConfidenceView::decl(),
            CatalogMatchView::decl(),
            CatalogModelSearchSnapshot::decl(),
            CatalogMatchSuggestionSnapshot::decl(),
            CatalogSnapshot::decl(),
            TimestampView::decl(),
            SessionPhaseView::decl(),
            SessionSuspensionKindView::decl(),
            SessionStatusView::decl(),
            InferenceSettingsMatchView::decl(),
            InferenceProviderSettingsView::decl(),
            InferenceSettingsView::decl(),
            SessionMetricsView::decl(),
            SelectableAgentView::decl(),
            SessionAssociationsView::decl(),
            SessionView::decl(),
            TimelineEventKindView::decl(),
            TimelineStatusView::decl(),
            ExternalRequestView::decl(),
            TimelineContentView::decl(),
            TimelineEventView::decl(),
            TimelinePresentationView::decl(),
            SyntheticTimelineEventView::decl(),
            ProjectedTimelineEntryView::decl(),
            SessionTimelineView::decl(),
            SessionTemplateOptionView::decl(),
            SessionTemplateSearchSnapshot::decl(),
            RollbackTargetView::decl(),
            AccessStatus::decl(),
            AccessSnapshot::decl(),
            ClientAuthTarget::decl(),
            AuthStatusSnapshot::decl(),
            AuthAttemptSnapshot::decl(),
            McpAuthorizationContextSnapshot::decl(),
            AuthenticationFailureSnapshot::decl(),
            AuthSnapshot::decl(),
            ClientOperationView::decl(),
            OperationCollectionSnapshot::decl(),
            OperationSnapshot::decl(),
            ResourceEditBaselineView::decl(),
            ConfigSnapshot::decl(),
            StatusSnapshot::decl(),
            SessionAgentCatalogSnapshot::decl(),
            SessionSnapshot::decl(),
            ClientRoutineValue::decl(),
            ClientRoutineValueType::decl(),
            ClientRoutineScope::decl(),
            ClientRoutineInputDefinition::decl(),
            ClientRoutineResultDefinition::decl(),
            ClientManualRoutineDefinition::decl(),
            ClientRoutineResult::decl(),
            ClientRoutineFieldViolation::decl(),
            ClientRoutineValidationFailure::decl(),
            ClientRoutineFailure::decl(),
            ClientRoutineTriggerKind::decl(),
            ClientRoutineInvocationStatus::decl(),
            ClientRoutineTaskStatus::decl(),
            ClientRoutineInvocationSummary::decl(),
            ClientRoutineTask::decl(),
            ClientRoutineInvocationDetail::decl(),
            RoutineDefinitionSnapshot::decl(),
            RoutineHistorySnapshot::decl(),
            RoutineDetailSnapshot::decl(),
            RoutineStartOperationView::decl(),
            RoutineSnapshot::decl(),
            ClientSnapshot::decl(),
            ClientSessionTemplateKind::decl(),
            ClientSessionTemplateSearchTarget::decl(),
            ClientTimelineDirection::decl(),
            ClientCommand::decl(),
        ]
        .join("\n\n")
        .lines()
        .map(|line| {
            let line = line.trim_end();
            line.strip_prefix("type ").map_or_else(
                || line.to_owned(),
                |declaration| format!("export type {declaration}"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
        let bindings =
            format!("// Generated by waltr-client. Do not edit directly.\n\n{bindings}\n");

        assert!(bindings.contains("export type ClientCommand"));
        assert!(bindings.contains("admissionSequence: bigint,"));
        assert!(!bindings.contains("admissionSequence: bigint | null"));

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../waltr-web/src/generated/waltr-client.ts");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("generated types directory should be created");
        }
        fs::write(path, bindings).expect("generated TypeScript bindings should be written");
    }
}
