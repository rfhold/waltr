use crate::support::server::HealthResponse;
use waltr_server::test_support::setup_server;

#[tokio::test]
async fn health_returns_ok() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();

    let response = reqwest::get(server.url("/health")).await?;
    assert!(response.status().is_success());

    let body = response.json::<HealthResponse>().await?;
    assert_eq!(body.status, "ok");

    Ok(())
}

#[tokio::test]
async fn ready_checks_initialized_dependencies() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();

    let response = reqwest::get(server.url("/ready")).await?;
    assert!(response.status().is_success());

    let body = response.json::<HealthResponse>().await?;
    assert_eq!(body.status, "ready");
    Ok(())
}
