//! Runtime profiling setup.

use pyroscope::{
    backend::{BackendConfig, PprofConfig, pprof_backend},
    pyroscope::{PyroscopeAgent, PyroscopeAgentBuilder, PyroscopeAgentRunning},
};
use std::env;

const SERVICE_NAME: &str = "waltr-server";
const SAMPLE_RATE: u32 = 100;

/// Holds the Pyroscope agent until process shutdown.
pub struct ProfilingGuard {
    agent: Option<PyroscopeAgent<PyroscopeAgentRunning>>,
}

impl std::fmt::Debug for ProfilingGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProfilingGuard")
            .field("enabled", &self.agent.is_some())
            .finish()
    }
}

impl Drop for ProfilingGuard {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.take() {
            match agent.stop() {
                Ok(agent) => agent.shutdown(),
                Err(error) => tracing::warn!(%error, "failed to stop pyroscope profiler"),
            }
        }
    }
}

/// Initialize Pyroscope profiling when `WALTR_PYROSCOPE_URL` is set.
///
/// # Errors
///
/// Returns an error when the Pyroscope agent cannot be built or started.
pub fn init() -> Result<ProfilingGuard, Box<dyn std::error::Error + Send + Sync>> {
    let Some(url) = env::var_os("WALTR_PYROSCOPE_URL") else {
        return Ok(ProfilingGuard { agent: None });
    };
    let url = url.into_string().map_err(
        |_| "WALTR_PYROSCOPE_URL must contain valid Unicode for Pyroscope initialization",
    )?;

    let agent = PyroscopeAgentBuilder::new(
        &url,
        SERVICE_NAME,
        SAMPLE_RATE,
        "pyroscope-rs",
        env!("CARGO_PKG_VERSION"),
        pprof_backend(PprofConfig::default(), BackendConfig::default()),
    )
    .build()?
    .start()?;

    tracing::info!(%url, sample_rate = SAMPLE_RATE, "pyroscope profiler initialized");

    Ok(ProfilingGuard { agent: Some(agent) })
}
