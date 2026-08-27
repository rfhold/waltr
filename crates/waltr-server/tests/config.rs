//! Unified resource catalog integration tests.

mod support;

use crate::support::grpc::{get_catalog, resource_client};
use waltr::grpc::waltr::resource::v1::{
    InspectResourceRequest, ReplaceResourceRequest, ResetResourceRequest, ResourceKind,
    WatchResourceCatalogRequest,
};
use waltr_server::test_support::setup_server;

const PROVIDER_OVERRIDE: &str = "apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata:\n  name: openai\nspec:\n  transport: openaiResponses\n  url: https://gateway.example/v1/responses\n";

#[tokio::test]
async fn read_returns_complete_seven_kind_catalog() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = resource_client(setup_server()).await?;
    let catalog = get_catalog(&mut client).await?;
    assert_eq!(catalog.resources.len(), 15);
    assert_eq!(
        catalog
            .resources
            .iter()
            .map(|resource| resource.kind)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        7
    );
    let config = catalog.effective_configuration.expect("effective config");
    assert_eq!(config.providers.len(), 4);
    assert!(config.mcp_servers.contains_key("waltr"));
    assert!(config.inference_defaults.is_some());
    Ok(())
}

#[tokio::test]
async fn replace_watch_and_reset_are_resource_native() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = resource_client(setup_server()).await?;
    let mut watch = client
        .watch_resource_catalog(WatchResourceCatalogRequest {})
        .await?
        .into_inner();
    let initial = watch
        .message()
        .await?
        .expect("initial snapshot")
        .catalog
        .unwrap();
    let inspection = client
        .inspect_resource(InspectResourceRequest {
            path: Some("providers/openai.yaml".to_owned()),
        })
        .await?
        .into_inner()
        .resources
        .remove(0);
    assert!(!inspection.persisted);

    let replaced = client
        .replace_resource(ReplaceResourceRequest {
            path: "providers/openai.yaml".to_owned(),
            source: PROVIDER_OVERRIDE.to_owned(),
            expected_etag: inspection.persisted_etag,
        })
        .await?
        .into_inner();
    let replaced_catalog = replaced.catalog.expect("replacement catalog");
    assert_ne!(replaced_catalog.etag, initial.etag);
    assert_eq!(
        replaced_catalog
            .effective_configuration
            .as_ref()
            .unwrap()
            .providers["openai"]
            .url,
        "https://gateway.example/v1/responses"
    );
    let watched = watch
        .message()
        .await?
        .expect("changed snapshot")
        .catalog
        .unwrap();
    assert_eq!(watched.etag, replaced_catalog.etag);
    assert_eq!(watched.resources.len(), initial.resources.len());

    let reset = client
        .reset_resource(ResetResourceRequest {
            path: "providers/openai.yaml".to_owned(),
            expected_etag: replaced.resource.unwrap().persisted_etag,
        })
        .await?
        .into_inner();
    assert_eq!(
        reset
            .catalog
            .unwrap()
            .effective_configuration
            .unwrap()
            .providers["openai"]
            .url,
        "https://api.openai.com/v1/chat/completions"
    );
    assert!(!reset.resource.unwrap().persisted);
    Ok(())
}

#[tokio::test]
async fn routine_inspect_replace_watch_and_reset_restore_bundle()
-> Result<(), Box<dyn std::error::Error>> {
    let mut client = resource_client(setup_server()).await?;
    let mut watch = client
        .watch_resource_catalog(WatchResourceCatalogRequest {})
        .await?
        .into_inner();
    let initial = watch
        .message()
        .await?
        .expect("initial snapshot")
        .catalog
        .unwrap();
    let bundled_source = include_str!("../resources/routines/fork.yaml");
    let inspection = client
        .inspect_resource(InspectResourceRequest {
            path: Some("routines/fork.yaml".to_owned()),
        })
        .await?
        .into_inner()
        .resources
        .remove(0);
    assert_eq!(inspection.kind, ResourceKind::Routine as i32);
    assert_eq!(inspection.active_source.as_deref(), Some(bundled_source));

    let replacement = bundled_source.replace(
        "Compact a durable session snapshot",
        "Compact a validated durable session snapshot",
    );
    let replaced = client
        .replace_resource(ReplaceResourceRequest {
            path: "routines/fork.yaml".to_owned(),
            source: replacement.clone(),
            expected_etag: inspection.persisted_etag,
        })
        .await?
        .into_inner();
    assert_eq!(
        replaced.resource.as_ref().unwrap().active_source.as_deref(),
        Some(replacement.as_str())
    );
    let watched = watch
        .message()
        .await?
        .expect("changed snapshot")
        .catalog
        .unwrap();
    assert_ne!(watched.etag, initial.etag);
    assert_eq!(watched.etag, replaced.catalog.as_ref().unwrap().etag);

    let reset = client
        .reset_resource(ResetResourceRequest {
            path: "routines/fork.yaml".to_owned(),
            expected_etag: replaced.resource.unwrap().persisted_etag,
        })
        .await?
        .into_inner();
    let resource = reset.resource.unwrap();
    assert!(!resource.persisted);
    assert_eq!(resource.active_source.as_deref(), Some(bundled_source));
    Ok(())
}

#[tokio::test]
async fn stale_etag_and_invalid_yaml_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = resource_client(setup_server()).await?;
    let stale = client
        .replace_resource(ReplaceResourceRequest {
            path: "providers/openai.yaml".to_owned(),
            source: PROVIDER_OVERRIDE.to_owned(),
            expected_etag: "sha256:stale".to_owned(),
        })
        .await
        .unwrap_err();
    assert_eq!(stale.code(), tonic::Code::FailedPrecondition);

    let inspection = client
        .inspect_resource(InspectResourceRequest {
            path: Some("providers/openai.yaml".to_owned()),
        })
        .await?
        .into_inner()
        .resources
        .remove(0);
    let invalid = client
        .replace_resource(ReplaceResourceRequest {
            path: "providers/openai.yaml".to_owned(),
            source: "kind: Provider\n".to_owned(),
            expected_etag: inspection.persisted_etag,
        })
        .await
        .unwrap_err();
    assert_eq!(invalid.code(), tonic::Code::InvalidArgument);
    Ok(())
}
