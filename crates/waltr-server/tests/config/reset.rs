use std::{collections::HashMap, time::Duration};

use crate::support::grpc::{config_client, config_with_provider, get_config};
use waltr::grpc::waltr::config::v1::{
    InferenceDefaults, McpServer, McpServerMapPatch, McpServerType, PatchConfigRequest,
    ProviderMapPatch, ResetConfigRequest, WatchConfigRequest,
};
use waltr_server::test_support::setup_server;
use tonic::Code;

#[tokio::test]
async fn reset_replaces_all_sections_and_publishes_changed_keys()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(server).await?;
    let initial = get_config(&mut client).await?;
    let mut watch = client
        .watch_config(WatchConfigRequest::default())
        .await?
        .into_inner();
    watch.message().await?.expect("initial watch snapshot");
    let custom_mcp = McpServer {
        name: "custom".to_owned(),
        r#type: McpServerType::Sse as i32,
        url: "https://mcp.example".to_owned(),
        ..McpServer::default()
    };
    let custom = client
        .patch_config(PatchConfigRequest {
            expected_etag: initial.etag.clone(),
            providers: Some(ProviderMapPatch {
                providers: config_with_provider("custom").providers,
            }),
            mcp_servers: Some(McpServerMapPatch {
                mcp_servers: HashMap::from([("custom".to_owned(), custom_mcp)]),
            }),
            inference_defaults: Some(InferenceDefaults::default()),
        })
        .await?
        .into_inner()
        .config
        .expect("patched config should be present");
    tokio::time::timeout(Duration::from_secs(2), watch.message())
        .await??
        .expect("patch watch event");

    let reset = client
        .reset_config(ResetConfigRequest {
            expected_etag: custom.etag.clone(),
        })
        .await?
        .into_inner()
        .config
        .expect("reset config should be present");

    assert_eq!(reset.providers, initial.providers);
    assert_eq!(reset.mcp_servers, initial.mcp_servers);
    assert!(reset.inference_defaults.is_none());
    assert_ne!(reset.etag, custom.etag);
    assert_eq!(get_config(&mut client).await?, reset);

    let event = tokio::time::timeout(Duration::from_secs(2), watch.message())
        .await??
        .expect("reset watch event");
    assert_eq!(event.previous_etag, custom.etag);
    assert_eq!(event.etag, reset.etag);
    assert_eq!(
        event.changed_keys,
        vec!["inference_defaults", "mcp_servers", "providers"]
    );

    Ok(())
}

#[tokio::test]
async fn reset_rejects_stale_etag_without_mutation() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(server).await?;
    let initial = get_config(&mut client).await?;

    let error = client
        .reset_config(ResetConfigRequest {
            expected_etag: "stale".to_owned(),
        })
        .await
        .expect_err("stale reset should fail");

    assert_eq!(error.code(), Code::FailedPrecondition);
    assert_eq!(get_config(&mut client).await?, initial);
    Ok(())
}
