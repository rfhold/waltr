use crate::support::grpc::{config_client, config_with_provider, get_config};
use waltr::grpc::waltr::config::v1::{
    Configuration, McpServer, McpServerMapPatch, McpServerType, Model, ModelType,
    PatchConfigRequest, Provider, ProviderMapPatch, ProviderType,
};
use waltr_server::test_support::setup_server;
use tonic::Code;

#[tokio::test]
async fn config_service_rejects_invalid_provider_names() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(&server).await?;
    let initial = get_config(&mut client).await?;

    let error = client
        .patch_config(PatchConfigRequest {
            expected_etag: initial.etag,
            providers: Some(ProviderMapPatch {
                providers: config_with_provider("Open AI").providers,
            }),
            mcp_servers: None,
        })
        .await
        .expect_err("invalid provider names should be rejected");

    assert_eq!(error.code(), Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn config_service_rejects_model_with_unspecified_type()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(&server).await?;
    let initial = get_config(&mut client).await?;

    let error = client
        .patch_config(PatchConfigRequest {
            expected_etag: initial.etag,
            providers: Some(ProviderMapPatch {
                providers: config_with_model("anthropic", "claude", ModelType::Unspecified)
                    .providers,
            }),
            mcp_servers: None,
        })
        .await
        .expect_err("model with unspecified type should be rejected");

    assert_eq!(error.code(), Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn config_service_accepts_model_with_valid_type() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(&server).await?;
    let initial = get_config(&mut client).await?;

    client
        .patch_config(PatchConfigRequest {
            expected_etag: initial.etag,
            providers: Some(ProviderMapPatch {
                providers: config_with_model("anthropic", "claude", ModelType::Llm).providers,
            }),
            mcp_servers: None,
        })
        .await
        .expect("model with a valid type should be accepted");

    Ok(())
}

#[tokio::test]
async fn config_service_rejects_mcp_server_with_unspecified_type()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(&server).await?;
    let initial = get_config(&mut client).await?;

    let server_def = McpServer {
        name: "tools".to_owned(),
        r#type: McpServerType::Unspecified as i32,
        command: "tool-server".to_owned(),
        ..McpServer::default()
    };

    let error = client
        .patch_config(PatchConfigRequest {
            expected_etag: initial.etag,
            providers: None,
            mcp_servers: Some(McpServerMapPatch {
                mcp_servers: config_with_mcp_server("tools", server_def).mcp_servers,
            }),
        })
        .await
        .expect_err("mcp server with unspecified type should be rejected");

    assert_eq!(error.code(), Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn config_service_rejects_stdio_server_without_command()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(&server).await?;
    let initial = get_config(&mut client).await?;

    let server_def = McpServer {
        name: "tools".to_owned(),
        r#type: McpServerType::Stdio as i32,
        ..McpServer::default()
    };

    let error = client
        .patch_config(PatchConfigRequest {
            expected_etag: initial.etag,
            providers: None,
            mcp_servers: Some(McpServerMapPatch {
                mcp_servers: config_with_mcp_server("tools", server_def).mcp_servers,
            }),
        })
        .await
        .expect_err("stdio server without command should be rejected");

    assert_eq!(error.code(), Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn config_service_rejects_sse_server_without_url() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(&server).await?;
    let initial = get_config(&mut client).await?;

    let server_def = McpServer {
        name: "tools".to_owned(),
        r#type: McpServerType::Sse as i32,
        ..McpServer::default()
    };

    let error = client
        .patch_config(PatchConfigRequest {
            expected_etag: initial.etag,
            providers: None,
            mcp_servers: Some(McpServerMapPatch {
                mcp_servers: config_with_mcp_server("tools", server_def).mcp_servers,
            }),
        })
        .await
        .expect_err("sse server without url should be rejected");

    assert_eq!(error.code(), Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn config_service_accepts_valid_mcp_servers() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(&server).await?;
    let initial = get_config(&mut client).await?;

    let config = Configuration {
        mcp_servers: std::collections::HashMap::from([
            (
                "stdio".to_owned(),
                McpServer {
                    name: "stdio".to_owned(),
                    r#type: McpServerType::Stdio as i32,
                    command: "tool-server".to_owned(),
                    ..McpServer::default()
                },
            ),
            (
                "sse".to_owned(),
                McpServer {
                    name: "sse".to_owned(),
                    r#type: McpServerType::Sse as i32,
                    url: "https://example.com/sse".to_owned(),
                    ..McpServer::default()
                },
            ),
        ]),
        ..Configuration::default()
    };

    client
        .patch_config(PatchConfigRequest {
            expected_etag: initial.etag,
            providers: None,
            mcp_servers: Some(McpServerMapPatch {
                mcp_servers: config.mcp_servers,
            }),
        })
        .await
        .expect("valid stdio and sse mcp servers should be accepted");

    Ok(())
}

fn config_with_model(provider: &str, model_key: &str, model_type: ModelType) -> Configuration {
    Configuration {
        providers: std::collections::HashMap::from([(
            provider.to_owned(),
            Provider {
                r#type: ProviderType::Anthropic as i32,
                name: provider.to_owned(),
                url: "https://api.anthropic.com".to_owned(),
                models: std::collections::HashMap::from([(
                    model_key.to_owned(),
                    Model {
                        name: model_key.to_owned(),
                        r#type: model_type as i32,
                        ..Model::default()
                    },
                )]),
                ..Provider::default()
            },
        )]),
        ..Configuration::default()
    }
}

fn config_with_mcp_server(key: &str, server: McpServer) -> Configuration {
    Configuration {
        mcp_servers: std::collections::HashMap::from([(key.to_owned(), server)]),
        ..Configuration::default()
    }
}
