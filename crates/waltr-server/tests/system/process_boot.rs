use crate::support::{
    grpc::{config_client_for_addr, config_with_provider, get_config},
    server::{temp_root, wait_for_health},
};
use waltr::grpc::waltr::config::v1::{PatchConfigRequest, ProviderMapPatch};
use std::{
    fs,
    net::TcpListener,
    process::{Command, Stdio},
    time::Duration,
};

#[tokio::test]
async fn server_process_boots_and_serves_health() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    let root = temp_root()?;

    let mut server = Command::new(env!("CARGO_BIN_EXE_waltr-server"))
        .env("WALTR_SERVER_ADDR", addr.to_string())
        .env_remove("WALTR_OTEL_EXPORT")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .current_dir(&root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let result = async {
        wait_for_health(addr).await?;
        patch_config(addr).await
    }
    .await;

    server.kill()?;
    server.wait()?;
    drop(fs::remove_dir_all(root));

    result
}

async fn patch_config(addr: std::net::SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = config_client_for_addr(addr).await?;
    let current = get_config(&mut client).await?;

    let patched = tokio::time::timeout(
        Duration::from_secs(2),
        client.patch_config(PatchConfigRequest {
            expected_etag: current.etag,
            providers: Some(ProviderMapPatch {
                providers: config_with_provider("anthropic").providers,
            }),
            mcp_servers: None,
            runtime_defaults: None,
        }),
    )
    .await??
    .into_inner()
    .config
    .expect("patched config should be present");

    assert_eq!(patched.providers["anthropic"].name, "anthropic");

    Ok(())
}
