//! Server catalog integration tests.

mod support;

use crate::support::grpc::{catalog_client, config_client, get_config};
use waltr::grpc::waltr::{
    catalog::v1::{
        ApplyCatalogModelDefaultsRequest, CatalogDefaultField, CatalogMatchConfidence,
        GetModelCatalogDriftRequest, SearchCatalogModelsRequest, SetModelCatalogMatchRequest,
        SuggestCatalogMatchesRequest,
    },
    config::v1::{
        CatalogMatchSource, Configuration, Model, ModelCapabilities, ModelCatalogMatch,
        ModelLimits, ModelOptions, ModelType, PatchConfigRequest, Provider, ProviderMapPatch,
        ProviderType, WatchConfigRequest,
    },
};
use waltr_server::{catalog::CatalogLoader, test_support};
use std::collections::HashMap;
use tonic::transport::Channel;

const FIXTURE: &str = r#"
{
  "anthropic": {
    "id": "anthropic",
    "name": "Anthropic",
    "api": "https://api.anthropic.com/v1",
    "models": {
      "claude-haiku-4-5": {
        "id": "claude-haiku-4-5",
        "name": "Claude Haiku 4.5",
        "family": "claude-haiku",
        "status": "stable",
        "attachment": false,
        "reasoning": true,
        "tool_call": true,
        "modalities": {"input": ["text"], "output": ["text"]},
        "limit": {"context": 200000, "output": 8192},
        "cost": {"input": 1, "output": 5}
      }
    }
  },
  "openai": {
    "id": "openai",
    "name": "OpenAI",
    "api": "https://api.openai.com/v1",
    "models": {
      "gpt-5-nano": {
        "id": "gpt-5-nano",
        "name": "GPT-5 Nano",
        "family": "gpt-5",
        "status": "stable",
        "attachment": true,
        "reasoning": true,
        "tool_call": true,
        "structured_output": true,
        "reasoning_options": [{"type": "effort", "values": ["low", "medium", "high"]}],
        "modalities": {"input": ["text", "image"], "output": ["text"]},
        "limit": {"context": 400000, "output": 128000},
        "cost": {"input": 0.05, "output": 0.4}
      }
    }
  },
  "gateway": {
    "id": "gateway",
    "name": "Example Gateway",
    "api": "https://gateway.example/v1",
    "models": {
      "openai/gpt-5-nano": {
        "id": "openai/gpt-5-nano",
        "name": "GPT-5 Nano via Gateway",
        "attachment": true,
        "tool_call": false,
        "modalities": {"input": ["text", "image"], "output": ["text"]},
        "limit": {"context": 1000, "output": 2000},
        "provider": {"api": "https://gateway.example/openai/v1", "shape": "responses"}
      }
    }
  }
}
"#;

#[tokio::test]
async fn catalog_search_returns_fixture_model() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_catalog_server()?;
    let mut client = catalog_client(server).await?;

    let response = client
        .search_catalog_models(SearchCatalogModelsRequest {
            query: "gpt-5-nano".to_owned(),
            limit: 5,
            ..SearchCatalogModelsRequest::default()
        })
        .await?
        .into_inner();

    assert!(!response.stale);
    let model = response.models[0].model.as_ref().expect("model ref");
    assert_eq!(model.provider_id, "openai");
    assert_eq!(model.model_id, "gpt-5-nano");
    assert_eq!(response.models[0].model_name, "GPT-5 Nano");
    assert_eq!(
        response.models[0]
            .reasoning
            .as_ref()
            .expect("reasoning summary")
            .efforts,
        vec!["high", "low", "medium"]
    );

    Ok(())
}

#[tokio::test]
async fn catalog_suggests_exact_provider_model_match() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_catalog_server()?;
    let mut client = catalog_client(server).await?;

    let response = client
        .suggest_catalog_matches(SuggestCatalogMatchesRequest {
            provider_key: "openai".to_owned(),
            model_key: "gpt-5-nano".to_owned(),
            limit: 3,
        })
        .await?
        .into_inner();

    assert!(!response.stale);
    let catalog_match = &response.matches[0];
    let model = catalog_match.model.as_ref().expect("match model ref");
    assert_eq!(model.provider_id, "openai");
    assert_eq!(model.model_id, "gpt-5-nano");
    assert_eq!(
        catalog_match.confidence,
        CatalogMatchConfidence::Exact as i32
    );
    assert_eq!(catalog_match.reasons, vec!["provider_id_model_id"]);

    Ok(())
}

#[tokio::test]
async fn catalog_suggest_derives_gateway_upstream_model() -> Result<(), Box<dyn std::error::Error>>
{
    let server = setup_catalog_server()?;
    let mut client = catalog_client(server).await?;

    let response = client
        .suggest_catalog_matches(SuggestCatalogMatchesRequest {
            provider_key: "gateway".to_owned(),
            model_key: "openai/gpt-5-nano".to_owned(),
            limit: 3,
        })
        .await?
        .into_inner();

    let catalog_match = &response.matches[0];
    let serving = catalog_match.model.as_ref().expect("serving model ref");
    assert_eq!(serving.provider_id, "gateway");
    assert_eq!(serving.model_id, "openai/gpt-5-nano");
    let upstream = catalog_match
        .upstream_model
        .as_ref()
        .expect("upstream model ref should be derived from serving id");
    assert_eq!(upstream.provider_id, "openai");
    assert_eq!(upstream.model_id, "gpt-5-nano");

    Ok(())
}

#[tokio::test]
async fn catalog_set_match_persists_model_catalog_match() -> Result<(), Box<dyn std::error::Error>>
{
    let server = setup_catalog_server()?;
    let mut catalog = catalog_client(server).await?;
    let mut config = config_client(server).await?;
    let initial = get_config(&mut config).await?;

    let response = catalog
        .set_model_catalog_match(SetModelCatalogMatchRequest {
            expected_etag: initial.etag,
            provider_key: "openai".to_owned(),
            model_key: "gpt-5-nano".to_owned(),
            catalog_match: Some(ModelCatalogMatch {
                provider_id: "openai".to_owned(),
                model_id: "gpt-5-nano".to_owned(),
                upstream_provider_id: None,
                upstream_model_id: None,
                source: CatalogMatchSource::Unspecified as i32,
            }),
            clear_match: false,
        })
        .await?
        .into_inner();
    let updated = response.config.expect("config should be present");

    let persisted_match = updated.providers["openai"].models["gpt-5-nano"]
        .catalog_match
        .as_ref()
        .expect("catalog match should persist");
    assert_eq!(persisted_match.provider_id, "openai");
    assert_eq!(persisted_match.model_id, "gpt-5-nano");
    assert_eq!(persisted_match.source, CatalogMatchSource::Manual as i32);

    let current = get_config(&mut config).await?;
    assert_eq!(current.etag, updated.etag);
    assert_eq!(
        current.providers["openai"].models["gpt-5-nano"].catalog_match,
        updated.providers["openai"].models["gpt-5-nano"].catalog_match
    );

    Ok(())
}

#[tokio::test]
async fn catalog_set_match_emits_config_watch_update() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_catalog_server()?;
    let mut catalog = catalog_client(server).await?;
    let mut config = config_client(server).await?;
    let initial = get_config(&mut config).await?;
    let mut watch = config
        .watch_config(WatchConfigRequest::default())
        .await?
        .into_inner();

    let initial_watch_event =
        tokio::time::timeout(std::time::Duration::from_secs(2), watch.message())
            .await??
            .expect("watch stream should emit initial config snapshot");
    assert_eq!(initial_watch_event.etag, initial.etag);

    let response = catalog
        .set_model_catalog_match(SetModelCatalogMatchRequest {
            expected_etag: initial.etag.clone(),
            provider_key: "openai".to_owned(),
            model_key: "gpt-5-nano".to_owned(),
            catalog_match: Some(ModelCatalogMatch {
                provider_id: "openai".to_owned(),
                model_id: "gpt-5-nano".to_owned(),
                upstream_provider_id: None,
                upstream_model_id: None,
                source: CatalogMatchSource::Unspecified as i32,
            }),
            clear_match: false,
        })
        .await?
        .into_inner();
    let updated = response.config.expect("config should be present");

    let watch_event = tokio::time::timeout(std::time::Duration::from_secs(2), watch.message())
        .await??
        .expect("watch stream should emit catalog config change");
    assert_eq!(watch_event.previous_etag, initial.etag);
    assert_eq!(watch_event.etag, updated.etag);
    assert_eq!(watch_event.changed_keys, vec!["providers"]);
    assert_eq!(
        watch_event
            .config
            .expect("watch config should be present")
            .providers["openai"]
            .models["gpt-5-nano"]
            .catalog_match,
        updated.providers["openai"].models["gpt-5-nano"].catalog_match
    );

    Ok(())
}

#[tokio::test]
async fn catalog_apply_defaults_fills_blanks_without_touching_options()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_catalog_server()?;
    let mut catalog = catalog_client(server).await?;
    let mut config = config_client(server).await?;
    let patched = patch_openai_provider(&mut config, openai_provider(blank_model())).await?;

    let response = catalog
        .apply_catalog_model_defaults(ApplyCatalogModelDefaultsRequest {
            expected_etag: patched.etag,
            provider_key: "openai".to_owned(),
            model_key: "gpt-5-nano".to_owned(),
            catalog_match: None,
            fields: vec![
                CatalogDefaultField::Name as i32,
                CatalogDefaultField::Limits as i32,
                CatalogDefaultField::Capabilities as i32,
            ],
        })
        .await?
        .into_inner();
    let updated = response.config.expect("config should be present");
    let model = &updated.providers["openai"].models["gpt-5-nano"];

    assert_eq!(model.name, "GPT-5 Nano");
    assert_eq!(model.r#type, ModelType::Llm as i32);
    assert_eq!(
        model.limits.as_ref().and_then(|limits| limits.context),
        Some(1)
    );
    assert_eq!(
        model.limits.as_ref().and_then(|limits| limits.output),
        Some(128000)
    );
    assert_eq!(
        model.capabilities,
        Some(ModelCapabilities {
            tools: true,
            image: true,
        })
    );
    assert_eq!(
        model.options,
        Some(ModelOptions {
            temperature: Some(0.2),
            top_p: Some(0.9),
            top_k: Some(40),
            min_p: None,
            presence_penalty: Some(0.1),
            repetition_penalty: None,
            reasoning_efforts: vec!["high".to_owned(), "low".to_owned(), "medium".to_owned()],
        })
    );

    Ok(())
}

#[tokio::test]
async fn catalog_drift_reports_changed_limits_and_capabilities()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_catalog_server()?;
    let mut catalog = catalog_client(server).await?;
    let mut config = config_client(server).await?;
    patch_openai_provider(&mut config, openai_provider(drifting_model())).await?;

    let response = catalog
        .get_model_catalog_drift(GetModelCatalogDriftRequest {
            provider_key: "openai".to_owned(),
            model_key: "gpt-5-nano".to_owned(),
        })
        .await?
        .into_inner();

    assert!(!response.stale);
    assert_eq!(
        response.catalog_match.expect("match should resolve").source,
        CatalogMatchSource::Auto as i32
    );
    let fields = response
        .fields
        .iter()
        .map(|field| field.field.as_str())
        .collect::<Vec<_>>();
    assert!(fields.contains(&"limit_context"));
    assert!(fields.contains(&"limit_output"));
    assert!(fields.contains(&"capability_tool_call"));

    Ok(())
}

#[tokio::test]
async fn catalog_drift_prefers_valid_upstream_identity() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_catalog_server()?;
    let mut catalog = catalog_client(server).await?;
    let mut config = config_client(server).await?;
    patch_provider(
        &mut config,
        "gateway",
        gateway_provider(gateway_drifting_model(Some("gpt-5-nano"))),
    )
    .await?;

    let response = catalog
        .get_model_catalog_drift(GetModelCatalogDriftRequest {
            provider_key: "gateway".to_owned(),
            model_key: "openai/gpt-5-nano".to_owned(),
        })
        .await?
        .into_inner();

    let context = response
        .fields
        .iter()
        .find(|field| field.field == "limit_context")
        .expect("context drift should use upstream model");
    assert_eq!(context.catalog, "400000");

    Ok(())
}

#[tokio::test]
async fn catalog_drift_falls_back_to_serving_identity_when_upstream_missing()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_catalog_server()?;
    let mut catalog = catalog_client(server).await?;
    let mut config = config_client(server).await?;
    patch_provider(
        &mut config,
        "gateway",
        gateway_provider(gateway_drifting_model(Some("missing-model"))),
    )
    .await?;

    let response = catalog
        .get_model_catalog_drift(GetModelCatalogDriftRequest {
            provider_key: "gateway".to_owned(),
            model_key: "openai/gpt-5-nano".to_owned(),
        })
        .await?
        .into_inner();

    let context = response
        .fields
        .iter()
        .find(|field| field.field == "limit_context")
        .expect("context drift should fall back to serving model");
    assert_eq!(context.catalog, "1000");

    Ok(())
}

fn setup_catalog_server() -> Result<&'static test_support::TestServer, Box<dyn std::error::Error>> {
    let root = test_support::temp_root();
    let fixture_path = root.join("models-dev-api.json");
    std::fs::write(&fixture_path, FIXTURE)?;
    Ok(test_support::setup_server_with_catalog_loader(
        CatalogLoader::from_path(fixture_path),
    ))
}

async fn patch_openai_provider(
    client: &mut waltr::grpc::waltr::config::v1::config_service_client::ConfigServiceClient<Channel>,
    provider: Provider,
) -> Result<Configuration, Box<dyn std::error::Error>> {
    patch_provider(client, "openai", provider).await
}

async fn patch_provider(
    client: &mut waltr::grpc::waltr::config::v1::config_service_client::ConfigServiceClient<Channel>,
    provider_key: &str,
    provider: Provider,
) -> Result<Configuration, Box<dyn std::error::Error>> {
    let current = get_config(client).await?;
    Ok(client
        .patch_config(PatchConfigRequest {
            expected_etag: current.etag,
            providers: Some(ProviderMapPatch {
                providers: HashMap::from([(provider_key.to_owned(), provider)]),
            }),
            mcp_servers: None,
            runtime_defaults: None,
        })
        .await?
        .into_inner()
        .config
        .expect("config should be present"))
}

fn openai_provider(model: Model) -> Provider {
    Provider {
        r#type: ProviderType::Openai as i32,
        name: "openai".to_owned(),
        url: "https://api.openai.com/v1".to_owned(),
        models: HashMap::from([("gpt-5-nano".to_owned(), model)]),
        ..Provider::default()
    }
}

fn gateway_provider(model: Model) -> Provider {
    Provider {
        r#type: ProviderType::Openai as i32,
        name: "gateway".to_owned(),
        url: "https://gateway.example/v1".to_owned(),
        models: HashMap::from([("openai/gpt-5-nano".to_owned(), model)]),
        ..Provider::default()
    }
}

fn blank_model() -> Model {
    Model {
        name: String::new(),
        r#type: ModelType::Llm as i32,
        options: Some(ModelOptions {
            temperature: Some(0.2),
            top_p: Some(0.9),
            top_k: Some(40),
            min_p: None,
            presence_penalty: Some(0.1),
            repetition_penalty: None,
            reasoning_efforts: Vec::new(),
        }),
        limits: Some(ModelLimits {
            context: Some(1),
            output: None,
        }),
        capabilities: None,
        catalog_match: None,
    }
}

fn drifting_model() -> Model {
    Model {
        name: "GPT-5 Nano".to_owned(),
        r#type: ModelType::Llm as i32,
        limits: Some(ModelLimits {
            context: Some(1),
            output: Some(2),
        }),
        capabilities: Some(ModelCapabilities {
            tools: false,
            image: false,
        }),
        ..Model::default()
    }
}

fn gateway_drifting_model(upstream_model_id: Option<&str>) -> Model {
    Model {
        name: "GPT-5 Nano via Gateway".to_owned(),
        r#type: ModelType::Llm as i32,
        limits: Some(ModelLimits {
            context: Some(1),
            output: Some(2),
        }),
        capabilities: Some(ModelCapabilities {
            tools: true,
            image: true,
        }),
        catalog_match: Some(ModelCatalogMatch {
            provider_id: "gateway".to_owned(),
            model_id: "openai/gpt-5-nano".to_owned(),
            upstream_provider_id: Some("openai".to_owned()),
            upstream_model_id: upstream_model_id.map(str::to_owned),
            source: CatalogMatchSource::Manual as i32,
        }),
        ..Model::default()
    }
}
