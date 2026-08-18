//! Read-only models.dev catalog RPC integration tests.

mod support;

use crate::support::grpc::catalog_client;
use waltr::grpc::waltr::catalog::v1::{
    CatalogMatchConfidence, SearchCatalogModelsRequest, SuggestCatalogMatchesRequest,
};
use waltr_server::{catalog::CatalogLoader, test_support};

const FIXTURE: &str = r#"{
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
        "reasoning": true,
        "tool_call": true,
        "reasoning_options": [{"type": "effort", "values": ["low", "medium", "high"]}],
        "limit": {"context": 400000, "output": 128000}
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
        "provider": {"api": "https://gateway.example/openai/v1", "shape": "responses"}
      }
    }
  }
}"#;

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
    let model = response.models[0].model.as_ref().expect("model ref");
    assert_eq!(
        (model.provider_id.as_str(), model.model_id.as_str()),
        ("openai", "gpt-5-nano")
    );
    assert_eq!(
        response.models[0].reasoning.as_ref().unwrap().efforts,
        ["high", "low", "medium"]
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
    assert_eq!(
        response.matches[0].confidence,
        CatalogMatchConfidence::Exact as i32
    );
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
    let upstream = response.matches[0]
        .upstream_model
        .as_ref()
        .expect("upstream model");
    assert_eq!(
        (upstream.provider_id.as_str(), upstream.model_id.as_str()),
        ("openai", "gpt-5-nano")
    );
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
