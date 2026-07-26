use serde::Deserialize;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

static TEMP_ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Deserialize)]
pub struct HealthResponse {
    pub status: String,
}

pub async fn wait_for_health(addr: std::net::SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
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

pub fn temp_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let counter = TEMP_ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("waltr-server-boot-{}-{counter}", std::process::id()));
    fs::create_dir_all(&root)?;
    Ok(root)
}
