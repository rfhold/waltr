use serde::Deserialize;
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Debug, Deserialize)]
pub(crate) struct HealthResponse {
    pub(crate) status: String,
}

pub(crate) async fn wait_for_health(
    addr: std::net::SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/health");
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline {
        if let Ok(response) = client.get(&url).send().await
            && response.status().is_success()
        {
            let body = response.json::<HealthResponse>().await?;
            assert_eq!(body.status, "ok");
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err("server did not serve /health before timeout".into())
}

pub(crate) fn temp_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join(format!(
        "waltr-server-boot-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    fs::create_dir_all(&root)?;
    Ok(root)
}
