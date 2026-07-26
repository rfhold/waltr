//! Stable shell-facing view models.

mod config;
mod session;

pub use config::{
    CatalogMatchSourceView, ConfigView, McpServerTypeView, McpServerView, ModelCapabilitiesView,
    ModelCatalogMatchView, ModelCostView, ModelLimitsView, ModelOptionsView, ModelTypeView,
    ModelView, OAuthJwtMetadataView, OAuthProviderView, OAuthTokenFieldView,
    OAuthTokenRequestStyleView, OAuthUrlParamView, ProviderKindView, ProviderView,
    RuntimeDefaultsView,
};
pub use session::{
    ExternalRequestView, RuntimeConfigMatchView, SessionMcpServerRuntimeView, SessionMetricsView,
    SessionPhaseView, SessionProviderRuntimeView, SessionRuntimeConfigView, SessionStatusView,
    SessionTimelineView, SessionView, TimelineContentView, TimelineEventKindView,
    TimelineEventView, TimelineStatusView, TimestampView,
};

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs, path::PathBuf};

    use ts_rs::TS as _;

    use crate::{
        AccessSnapshot, AccessStatus, AuthAttemptSnapshot, AuthRequirementSnapshot, AuthSnapshot,
        CatalogMatchSource, ClientAuthTarget, ClientCommand, ClientOperationView,
        ClientSessionTemplateKind, ClientSessionTemplateSearchTarget, ClientSnapshot,
        ClientTimelineDirection, ConfigSnapshot, Configuration, Model, ModelCapabilities,
        ModelCatalogMatch, ModelLimits, ModelOptions, ModelType, OperationCollectionSnapshot,
        OperationSnapshot, Provider, ProviderType, RollbackTargetView, SessionSnapshot,
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
            runtime_defaults: None,
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
            RuntimeDefaultsView::decl(),
            ProviderView::decl(),
            ConfigView::decl(),
            TimestampView::decl(),
            SessionPhaseView::decl(),
            SessionStatusView::decl(),
            RuntimeConfigMatchView::decl(),
            SessionProviderRuntimeView::decl(),
            SessionMcpServerRuntimeView::decl(),
            SessionRuntimeConfigView::decl(),
            SessionMetricsView::decl(),
            SessionView::decl(),
            TimelineEventKindView::decl(),
            TimelineStatusView::decl(),
            ExternalRequestView::decl(),
            TimelineContentView::decl(),
            TimelineEventView::decl(),
            SessionTimelineView::decl(),
            SessionTemplateOptionView::decl(),
            SessionTemplateSearchSnapshot::decl(),
            RollbackTargetView::decl(),
            AccessStatus::decl(),
            AccessSnapshot::decl(),
            ClientAuthTarget::decl(),
            AuthAttemptSnapshot::decl(),
            AuthRequirementSnapshot::decl(),
            AuthSnapshot::decl(),
            ClientOperationView::decl(),
            OperationCollectionSnapshot::decl(),
            OperationSnapshot::decl(),
            ConfigSnapshot::decl(),
            StatusSnapshot::decl(),
            SessionSnapshot::decl(),
            ClientSnapshot::decl(),
            ClientSessionTemplateKind::decl(),
            ClientSessionTemplateSearchTarget::decl(),
            ClientTimelineDirection::decl(),
            ClientCommand::decl(),
        ]
        .join("\n\n")
        .lines()
        .map(|line| {
            line.strip_prefix("type ").map_or_else(
                || line.to_owned(),
                |declaration| format!("export type {declaration}"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
        let bindings =
            format!("// Generated by waltr-client. Do not edit directly.\n\n{bindings}\n");

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../waltr-web/src/generated/waltr-client.ts");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("generated types directory should be created");
        }
        fs::write(path, bindings).expect("generated TypeScript bindings should be written");
    }
}
