//! Models.dev catalog loading and gRPC service behavior.

mod models_dev;

use crate::config::{ConfigError, ConfigPatch, ConfigStore, FilesystemConfigStore};
use waltr::grpc::waltr::{catalog::v1 as catalog_proto, config::v1 as config_proto};
use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

const DEFAULT_MODELS_DEV_URL: &str = "https://models.dev/api.json";
const ENV_MODELS_DEV_URL: &str = "WALTR_MODELS_DEV_URL";
const ENV_MODELS_DEV_PATH: &str = "WALTR_MODELS_DEV_PATH";
const ENV_MODELS_DEV_CACHE_PATH: &str = "WALTR_MODELS_DEV_CACHE_PATH";
const CACHE_RELATIVE_PATH: &[&str] = &[".local", "share", "waltr", "cache", "models-dev-api.json"];
const CATALOG_FRESHNESS: Duration = Duration::from_hours(24);
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

/// Loaded catalog with freshness metadata.
#[derive(Clone, Debug)]
pub struct LoadedCatalog {
    catalog: Arc<models_dev::ModelCatalog>,
    stale: bool,
}

impl LoadedCatalog {
    #[must_use]
    fn catalog(&self) -> &models_dev::ModelCatalog {
        &self.catalog
    }

    #[must_use]
    const fn stale(&self) -> bool {
        self.stale
    }
}

#[derive(Clone, Debug)]
struct CachedCatalog {
    loaded: LoadedCatalog,
    loaded_at: Instant,
}

/// On-demand models.dev catalog loader with cache fallback.
#[derive(Clone, Debug)]
pub struct CatalogLoader {
    source: CatalogSource,
    cache_path: PathBuf,
    client: reqwest::Client,
    cached: Arc<Mutex<Option<CachedCatalog>>>,
    refresh: Arc<CatalogRefreshCoordinator>,
    #[cfg(test)]
    refresh_barrier: Option<Arc<tokio::sync::Barrier>>,
}

impl CatalogLoader {
    /// Create a loader from `WALTR_MODELS_DEV_PATH`, `WALTR_MODELS_DEV_URL`, or defaults.
    #[must_use]
    pub fn from_env() -> Self {
        let source = std::env::var_os(ENV_MODELS_DEV_PATH).map_or_else(
            || {
                let url = std::env::var(ENV_MODELS_DEV_URL)
                    .unwrap_or_else(|_| DEFAULT_MODELS_DEV_URL.to_owned());
                CatalogSource::Url(url)
            },
            |path| CatalogSource::Path(PathBuf::from(path)),
        );

        Self::new(source, default_cache_path())
    }

    /// Create a loader backed by a local fixture path.
    #[must_use]
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self::new(CatalogSource::Path(path.into()), default_cache_path())
    }

    #[must_use]
    fn new(source: CatalogSource, cache_path: PathBuf) -> Self {
        Self {
            source,
            cache_path,
            client: reqwest::Client::new(),
            cached: Arc::new(Mutex::new(None)),
            refresh: Arc::new(CatalogRefreshCoordinator::default()),
            #[cfg(test)]
            refresh_barrier: None,
        }
    }

    /// Load the catalog, refreshing stale in-memory entries on demand.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured source and any cache fallback cannot be loaded.
    pub async fn load(&self) -> Result<LoadedCatalog, CatalogLoadError> {
        if let Some(loaded) = self.fresh_cached().await {
            return Ok(loaded);
        }

        #[cfg(test)]
        if let Some(barrier) = &self.refresh_barrier {
            barrier.wait().await;
        }

        match self.refresh.join_or_start() {
            CatalogRefreshRole::Follower(generation) => generation.wait().await.into_result(),
            CatalogRefreshRole::Leader(generation) => self.run_refresh_generation(generation).await,
        }
    }

    async fn fresh_cached(&self) -> Option<LoadedCatalog> {
        self.cached
            .lock()
            .await
            .as_ref()
            .filter(|cached| cached.loaded_at.elapsed() < CATALOG_FRESHNESS)
            .map(|cached| cached.loaded.clone())
    }

    async fn run_refresh_generation(
        &self,
        generation: Arc<CatalogRefreshGeneration>,
    ) -> Result<LoadedCatalog, CatalogLoadError> {
        let leader = CatalogRefreshLeader::new(Arc::clone(&self.refresh), generation);
        if let Some(loaded) = self.fresh_cached().await {
            return leader.complete(SharedCatalogRefreshOutcome::Success(loaded));
        }

        let outcome = match &self.source {
            CatalogSource::Path(path) => self.load_path(path).await,
            CatalogSource::Url(url) => self.load_url(url).await,
        };
        let shared = match outcome {
            Ok(loaded) => {
                *self.cached.lock().await = Some(CachedCatalog {
                    loaded: loaded.clone(),
                    loaded_at: Instant::now(),
                });
                SharedCatalogRefreshOutcome::Success(loaded)
            }
            Err(error) => SharedCatalogRefreshOutcome::Failure(error.into_failure()),
        };
        leader.complete(shared)
    }

    async fn load_path(&self, path: &PathBuf) -> Result<LoadedCatalog, CatalogLoadError> {
        let data = tokio::fs::read_to_string(path)
            .await
            .map_err(|_| CatalogLoadError::new("read catalog fixture", CatalogFailureKind::Io))?;
        parse_catalog(&data, false)
    }

    async fn load_url(&self, url: &str) -> Result<LoadedCatalog, CatalogLoadError> {
        match self.fetch_and_cache(url).await {
            Ok(loaded) => Ok(loaded),
            Err(fetch_error) => match self.load_cached_file().await {
                Ok(Some(loaded)) => Ok(loaded),
                Ok(None) => Err(fetch_error),
                Err(cache_error) => Err(fetch_error.with_fallback(cache_error)),
            },
        }
    }

    async fn fetch_and_cache(&self, url: &str) -> Result<LoadedCatalog, CatalogLoadError> {
        let endpoint = sanitized_endpoint(url);
        let response = self.client.get(url).send().await.map_err(|error| {
            CatalogLoadError::request("fetch models.dev catalog", endpoint.clone(), &error)
        })?;
        if !response.status().is_success() {
            return Err(CatalogLoadError::http(
                "fetch models.dev catalog",
                endpoint,
                response.status().as_u16(),
            ));
        }
        let data = response.text().await.map_err(|_| {
            CatalogLoadError::endpoint(
                "read models.dev catalog response",
                CatalogFailureKind::Body,
                endpoint,
            )
        })?;
        let loaded = parse_catalog(&data, false)?;
        if let Err(error) = self.write_cache(&data).await {
            tracing::warn!(%error, "failed to write catalog cache");
        }
        Ok(loaded)
    }

    async fn load_cached_file(&self) -> Result<Option<LoadedCatalog>, CatalogLoadError> {
        match tokio::fs::read_to_string(&self.cache_path).await {
            Ok(data) => parse_catalog(&data, true).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(CatalogLoadError::new(
                "read catalog cache",
                CatalogFailureKind::Io,
            )),
        }
    }

    async fn write_cache(&self, data: &str) -> Result<(), CatalogLoadError> {
        if let Some(parent) = self.cache_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|_| {
                CatalogLoadError::new("create catalog cache directory", CatalogFailureKind::Io)
            })?;
        }

        let tmp = self.cache_path.with_extension("json.tmp");
        tokio::fs::write(&tmp, data)
            .await
            .map_err(|_| CatalogLoadError::new("write catalog cache", CatalogFailureKind::Io))?;
        tokio::fs::rename(&tmp, &self.cache_path)
            .await
            .map_err(|_| CatalogLoadError::new("replace catalog cache", CatalogFailureKind::Io))
    }
}

#[derive(Debug, Default)]
struct CatalogRefreshCoordinator {
    state: StdMutex<CatalogRefreshState>,
}

#[derive(Debug, Default)]
struct CatalogRefreshState {
    next_generation: u64,
    active: Option<Arc<CatalogRefreshGeneration>>,
}

impl CatalogRefreshCoordinator {
    fn join_or_start(&self) -> CatalogRefreshRole {
        let mut state = self.state.lock().expect("catalog refresh state lock");
        if let Some(generation) = state.active.clone() {
            drop(state);
            return CatalogRefreshRole::Follower(generation);
        }
        let id = state.next_generation;
        state.next_generation = state.next_generation.wrapping_add(1);
        let generation = Arc::new(CatalogRefreshGeneration::new(id));
        state.active = Some(Arc::clone(&generation));
        drop(state);
        CatalogRefreshRole::Leader(generation)
    }

    fn complete(
        &self,
        generation: &CatalogRefreshGeneration,
        outcome: SharedCatalogRefreshOutcome,
    ) {
        generation.publish(outcome);
        let mut state = self.state.lock().expect("catalog refresh state lock");
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.id == generation.id)
        {
            state.active = None;
        }
    }

    #[cfg(test)]
    fn active_generation(&self) -> Option<Arc<CatalogRefreshGeneration>> {
        self.state
            .lock()
            .expect("catalog refresh state lock")
            .active
            .clone()
    }
}

enum CatalogRefreshRole {
    Leader(Arc<CatalogRefreshGeneration>),
    Follower(Arc<CatalogRefreshGeneration>),
}

#[derive(Debug)]
struct CatalogRefreshGeneration {
    id: u64,
    outcome: StdMutex<Option<SharedCatalogRefreshOutcome>>,
    completed: tokio::sync::Notify,
    #[cfg(test)]
    follower_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    follower_joined: tokio::sync::Notify,
}

impl CatalogRefreshGeneration {
    fn new(id: u64) -> Self {
        Self {
            id,
            outcome: StdMutex::new(None),
            completed: tokio::sync::Notify::new(),
            #[cfg(test)]
            follower_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            follower_joined: tokio::sync::Notify::new(),
        }
    }

    async fn wait(&self) -> SharedCatalogRefreshOutcome {
        #[cfg(test)]
        {
            self.follower_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.follower_joined.notify_waiters();
        }
        loop {
            let completed = self.completed.notified();
            let outcome = self
                .outcome
                .lock()
                .expect("catalog refresh outcome lock")
                .clone();
            if let Some(outcome) = outcome {
                return outcome;
            }
            completed.await;
        }
    }

    fn publish(&self, outcome: SharedCatalogRefreshOutcome) {
        let mut published = self.outcome.lock().expect("catalog refresh outcome lock");
        if published.is_none() {
            *published = Some(outcome);
            drop(published);
            self.completed.notify_waiters();
        }
    }

    #[cfg(test)]
    async fn wait_for_followers(&self, count: usize) {
        loop {
            let joined = self.follower_joined.notified();
            if self
                .follower_count
                .load(std::sync::atomic::Ordering::SeqCst)
                >= count
            {
                return;
            }
            joined.await;
        }
    }
}

#[derive(Clone, Debug)]
enum SharedCatalogRefreshOutcome {
    Success(LoadedCatalog),
    Failure(Arc<CatalogFailure>),
}

impl SharedCatalogRefreshOutcome {
    fn into_result(self) -> Result<LoadedCatalog, CatalogLoadError> {
        match self {
            Self::Success(loaded) => Ok(loaded),
            Self::Failure(failure) => Err(CatalogLoadError::from_failure(failure)),
        }
    }
}

struct CatalogRefreshLeader {
    coordinator: Arc<CatalogRefreshCoordinator>,
    generation: Arc<CatalogRefreshGeneration>,
    completed: bool,
}

impl CatalogRefreshLeader {
    const fn new(
        coordinator: Arc<CatalogRefreshCoordinator>,
        generation: Arc<CatalogRefreshGeneration>,
    ) -> Self {
        Self {
            coordinator,
            generation,
            completed: false,
        }
    }

    fn complete(
        mut self,
        outcome: SharedCatalogRefreshOutcome,
    ) -> Result<LoadedCatalog, CatalogLoadError> {
        self.coordinator.complete(&self.generation, outcome.clone());
        self.completed = true;
        outcome.into_result()
    }
}

impl Drop for CatalogRefreshLeader {
    fn drop(&mut self) {
        if !self.completed {
            self.coordinator.complete(
                &self.generation,
                SharedCatalogRefreshOutcome::Failure(Arc::new(CatalogFailure::new(
                    "refresh catalog",
                    CatalogFailureKind::Canceled,
                ))),
            );
        }
    }
}

#[cfg(test)]
mod loader_tests {
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, extract::State, http::StatusCode, routing::get};
    use tokio::{
        sync::{Barrier, Semaphore, mpsc},
        task::JoinHandle,
    };

    use super::*;

    const CATALOG: &str = r#"{
        "test": {
            "id": "test",
            "name": "Test Provider",
            "models": {
                "model": {
                    "id": "model",
                    "name": "Test Model",
                    "limit": {"context": 1024, "output": 256},
                    "cost": {"input": 1, "output": 2},
                    "modalities": {"input": ["text"], "output": ["text"]}
                }
            }
        }
    }"#;

    #[derive(Clone)]
    struct CatalogHttpState {
        count: Arc<AtomicUsize>,
        entered: mpsc::UnboundedSender<usize>,
        responses: Arc<Mutex<VecDeque<CatalogHttpResponse>>>,
    }

    struct CatalogHttpResponse {
        status: StatusCode,
        body: &'static str,
        release: Option<Arc<Semaphore>>,
    }

    struct CatalogHttpServer {
        url: String,
        count: Arc<AtomicUsize>,
        entered: mpsc::UnboundedReceiver<usize>,
        task: JoinHandle<()>,
    }

    impl Drop for CatalogHttpServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn catalog_response(State(state): State<CatalogHttpState>) -> (StatusCode, &'static str) {
        let request = state.count.fetch_add(1, Ordering::SeqCst) + 1;
        state
            .entered
            .send(request)
            .expect("test receiver remains open");
        let response = state
            .responses
            .lock()
            .await
            .pop_front()
            .expect("test response is configured");
        if let Some(release) = response.release {
            release
                .acquire()
                .await
                .expect("test release semaphore remains open")
                .forget();
        }
        (response.status, response.body)
    }

    async fn start_catalog_server(responses: Vec<CatalogHttpResponse>) -> CatalogHttpServer {
        let count = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered) = mpsc::unbounded_channel();
        let state = CatalogHttpState {
            count: Arc::clone(&count),
            entered: entered_tx,
            responses: Arc::new(Mutex::new(responses.into())),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind catalog fixture");
        let address = listener.local_addr().expect("read fixture address");
        let app = Router::new()
            .route("/catalog", get(catalog_response))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve catalog fixture");
        });
        CatalogHttpServer {
            url: format!("http://{address}/catalog"),
            count,
            entered,
            task,
        }
    }

    async fn concurrent_results(
        loader: &CatalogLoader,
        callers: usize,
    ) -> Vec<Result<LoadedCatalog, CatalogLoadError>> {
        let mut tasks = Vec::new();
        for _ in 0..callers {
            let loader = loader.clone();
            tasks.push(tokio::spawn(async move { loader.load().await }));
        }
        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await.expect("join catalog load"));
        }
        results
    }

    fn successful_snapshots(
        results: Vec<Result<LoadedCatalog, CatalogLoadError>>,
    ) -> Vec<LoadedCatalog> {
        results
            .into_iter()
            .map(|result| result.expect("load catalog"))
            .collect()
    }

    fn assert_same_snapshot(loaded: &[LoadedCatalog], stale: bool) {
        let first = loaded.first().expect("at least one catalog result");
        assert!(loaded.iter().all(|loaded| {
            Arc::ptr_eq(&first.catalog, &loaded.catalog) && first.stale == loaded.stale
        }));
        assert_eq!(first.stale, stale);
    }

    fn assert_equivalent_errors(results: Vec<Result<LoadedCatalog, CatalogLoadError>>) -> String {
        let errors = results
            .into_iter()
            .map(|result| result.expect_err("catalog refresh should fail").to_string())
            .collect::<Vec<_>>();
        let first = errors.first().expect("at least one catalog error");
        assert!(errors.iter().all(|error| error == first));
        first.clone()
    }

    async fn wait_for_followers(loader: &CatalogLoader, count: usize) {
        loader
            .refresh
            .active_generation()
            .expect("active refresh generation")
            .wait_for_followers(count)
            .await;
    }

    #[tokio::test]
    async fn concurrent_cold_loads_share_one_refresh() {
        const CALLERS: usize = 8;
        let release = Arc::new(Semaphore::new(0));
        let responses = (0..CALLERS)
            .map(|index| CatalogHttpResponse {
                status: StatusCode::OK,
                body: CATALOG,
                release: (index == 0).then(|| Arc::clone(&release)),
            })
            .collect();
        let mut server = start_catalog_server(responses).await;
        let arrivals = Arc::new(Barrier::new(CALLERS + 1));
        let mut loader = CatalogLoader::new(
            CatalogSource::Url(server.url.clone()),
            crate::test_support::temp_root().join("catalog.json"),
        );
        loader.refresh_barrier = Some(Arc::clone(&arrivals));

        let loads = tokio::spawn({
            let loader = loader.clone();
            async move { concurrent_results(&loader, CALLERS).await }
        });
        arrivals.wait().await;
        assert_eq!(server.entered.recv().await, Some(1));
        wait_for_followers(&loader, CALLERS - 1).await;
        release.add_permits(1);

        let snapshots = successful_snapshots(loads.await.expect("join concurrent loads"));
        assert_same_snapshot(&snapshots, false);
        assert_eq!(server.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_failures_share_one_error_and_a_later_burst_retries_once() {
        const CALLERS: usize = 8;
        let failed_release = Arc::new(Semaphore::new(0));
        let success_release = Arc::new(Semaphore::new(0));
        let mut server = start_catalog_server(vec![
            CatalogHttpResponse {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: "failed",
                release: Some(Arc::clone(&failed_release)),
            },
            CatalogHttpResponse {
                status: StatusCode::OK,
                body: CATALOG,
                release: Some(Arc::clone(&success_release)),
            },
        ])
        .await;
        let sensitive_url = format!(
            "{}?token=SENTINEL_QUERY#SENTINEL_FRAGMENT",
            server
                .url
                .replacen("http://", "http://catalog-user:SENTINEL_PASSWORD@", 1)
        );
        let arrivals = Arc::new(Barrier::new(CALLERS + 1));
        let mut loader = CatalogLoader::new(
            CatalogSource::Url(sensitive_url),
            crate::test_support::temp_root().join("catalog.json"),
        );
        loader.refresh_barrier = Some(Arc::clone(&arrivals));

        let failures = tokio::spawn({
            let loader = loader.clone();
            async move { concurrent_results(&loader, CALLERS).await }
        });
        arrivals.wait().await;
        assert_eq!(server.entered.recv().await, Some(1));
        wait_for_followers(&loader, CALLERS - 1).await;
        failed_release.add_permits(1);
        let message = assert_equivalent_errors(failures.await.expect("join failed loads"));
        assert!(message.contains("failed to fetch models.dev catalog"));
        assert!(message.contains("(http), HTTP 500"));
        assert!(message.contains(&server.url));
        assert!(!message.contains("catalog-user"));
        assert!(!message.contains("SENTINEL_PASSWORD"));
        assert!(!message.contains("SENTINEL_QUERY"));
        assert!(!message.contains("SENTINEL_FRAGMENT"));
        assert_eq!(server.count.load(Ordering::SeqCst), 1);

        let successes = tokio::spawn({
            let loader = loader.clone();
            async move { concurrent_results(&loader, CALLERS).await }
        });
        arrivals.wait().await;
        assert_eq!(server.entered.recv().await, Some(2));
        wait_for_followers(&loader, CALLERS - 1).await;
        success_release.add_permits(1);
        let snapshots = successful_snapshots(successes.await.expect("join successful loads"));
        assert_same_snapshot(&snapshots, false);
        assert_eq!(server.count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn leader_cancellation_releases_waiters_without_retry_fanout() {
        const WAITERS: usize = 8;
        let canceled_release = Arc::new(Semaphore::new(0));
        let mut server = start_catalog_server(vec![
            CatalogHttpResponse {
                status: StatusCode::OK,
                body: CATALOG,
                release: Some(Arc::clone(&canceled_release)),
            },
            CatalogHttpResponse {
                status: StatusCode::OK,
                body: CATALOG,
                release: None,
            },
        ])
        .await;
        let loader = CatalogLoader::new(
            CatalogSource::Url(server.url.clone()),
            crate::test_support::temp_root().join("catalog.json"),
        );

        let leader_task = tokio::spawn({
            let loader = loader.clone();
            async move { loader.load().await }
        });
        assert_eq!(server.entered.recv().await, Some(1));
        let waiters = tokio::spawn({
            let loader = loader.clone();
            async move { concurrent_results(&loader, WAITERS).await }
        });
        wait_for_followers(&loader, WAITERS).await;
        leader_task.abort();
        assert!(
            leader_task
                .await
                .expect_err("leader task is canceled")
                .is_cancelled()
        );

        let message = assert_equivalent_errors(waiters.await.expect("join waiting loads"));
        assert_eq!(message, "failed to refresh catalog (canceled)");
        assert_eq!(server.count.load(Ordering::SeqCst), 1);

        let retried = loader.load().await.expect("retry canceled refresh");
        assert_eq!(server.entered.recv().await, Some(2));
        assert!(!retried.stale);
        canceled_release.add_permits(1);
        assert_eq!(server.count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stale_cache_file_fallback_is_shared_by_one_failed_upstream_refresh() {
        const CALLERS: usize = 8;
        let release = Arc::new(Semaphore::new(0));
        let mut server = start_catalog_server(vec![CatalogHttpResponse {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: "failed",
            release: Some(Arc::clone(&release)),
        }])
        .await;
        let root = crate::test_support::temp_root();
        let cache_path = root.join("catalog.json");
        tokio::fs::write(&cache_path, CATALOG)
            .await
            .expect("write stale catalog cache");
        let arrivals = Arc::new(Barrier::new(CALLERS + 1));
        let mut loader = CatalogLoader::new(CatalogSource::Url(server.url.clone()), cache_path);
        loader.refresh_barrier = Some(Arc::clone(&arrivals));

        let loads = tokio::spawn({
            let loader = loader.clone();
            async move { concurrent_results(&loader, CALLERS).await }
        });
        arrivals.wait().await;
        assert_eq!(server.entered.recv().await, Some(1));
        wait_for_followers(&loader, CALLERS - 1).await;
        release.add_permits(1);

        let snapshots = successful_snapshots(loads.await.expect("join fallback loads"));
        assert_same_snapshot(&snapshots, true);
        assert_eq!(server.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fresh_cache_hits_bypass_refresh_coordination_and_source() {
        const CALLERS: usize = 8;
        let mut server = start_catalog_server(vec![CatalogHttpResponse {
            status: StatusCode::OK,
            body: CATALOG,
            release: None,
        }])
        .await;
        let loader = CatalogLoader::new(
            CatalogSource::Url(server.url.clone()),
            crate::test_support::temp_root().join("catalog.json"),
        );
        let primed = loader.load().await.expect("prime catalog cache");
        assert_eq!(server.entered.recv().await, Some(1));

        let snapshots = successful_snapshots(concurrent_results(&loader, CALLERS).await);
        assert_same_snapshot(&snapshots, false);
        assert!(
            snapshots
                .iter()
                .all(|loaded| Arc::ptr_eq(&primed.catalog, &loaded.catalog))
        );
        assert_eq!(server.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cache_file_read_failure_is_shared_without_source_fanout() {
        const CALLERS: usize = 8;
        let release = Arc::new(Semaphore::new(0));
        let mut server = start_catalog_server(vec![CatalogHttpResponse {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: "failed",
            release: Some(Arc::clone(&release)),
        }])
        .await;
        let root = crate::test_support::temp_root().join("SENTINEL_CACHE_PATH");
        tokio::fs::create_dir_all(&root)
            .await
            .expect("create sensitive cache root");
        let cache_path = root.join("cache-directory");
        tokio::fs::create_dir(&cache_path)
            .await
            .expect("create cache directory");
        let arrivals = Arc::new(Barrier::new(CALLERS + 1));
        let mut loader = CatalogLoader::new(CatalogSource::Url(server.url.clone()), cache_path);
        loader.refresh_barrier = Some(Arc::clone(&arrivals));

        let loads = tokio::spawn({
            let loader = loader.clone();
            async move { concurrent_results(&loader, CALLERS).await }
        });
        arrivals.wait().await;
        assert_eq!(server.entered.recv().await, Some(1));
        wait_for_followers(&loader, CALLERS - 1).await;
        release.add_permits(1);

        let message = assert_equivalent_errors(loads.await.expect("join failed cache loads"));
        assert!(message.contains("cache fallback failed"));
        assert!(!message.contains("SENTINEL_CACHE_PATH"));
        assert_eq!(server.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fixture_read_and_decode_failures_do_not_expose_paths_or_input() {
        let missing = crate::test_support::temp_root().join("SENTINEL_FIXTURE_PATH/catalog.json");
        let error = CatalogLoader::from_path(missing)
            .load()
            .await
            .expect_err("missing fixture should fail")
            .to_string();
        assert_eq!(error, "failed to read catalog fixture (io)");
        assert!(!error.contains("SENTINEL_FIXTURE_PATH"));

        let root = crate::test_support::temp_root();
        let fixture = root.join("catalog.json");
        tokio::fs::write(&fixture, r#"{"SENTINEL_CATALOG_BODY": }"#)
            .await
            .expect("write malformed fixture");
        let error = CatalogLoader::from_path(fixture)
            .load()
            .await
            .expect_err("malformed fixture should fail")
            .to_string();
        assert_eq!(error, "failed to decode catalog (decode)");
        assert!(!error.contains("SENTINEL_CATALOG_BODY"));
    }
}

fn parse_catalog(data: &str, stale: bool) -> Result<LoadedCatalog, CatalogLoadError> {
    let catalog = models_dev::ModelCatalog::from_json_str(data)
        .map_err(|_| CatalogLoadError::new("decode catalog", CatalogFailureKind::Decode))?;
    Ok(LoadedCatalog {
        catalog: Arc::new(catalog),
        stale,
    })
}

fn default_cache_path() -> PathBuf {
    if let Some(path) = std::env::var_os(ENV_MODELS_DEV_CACHE_PATH) {
        return PathBuf::from(path);
    }

    let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
    CACHE_RELATIVE_PATH
        .iter()
        .fold(PathBuf::from(home), |path, segment| path.join(segment))
}

#[derive(Clone, Debug)]
enum CatalogSource {
    Url(String),
    Path(PathBuf),
}

fn sanitized_endpoint(value: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return None;
    }
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

/// Catalog load failure.
#[derive(Debug)]
pub struct CatalogLoadError {
    failure: Arc<CatalogFailure>,
}

impl CatalogLoadError {
    fn new(operation: &'static str, kind: CatalogFailureKind) -> Self {
        Self {
            failure: Arc::new(CatalogFailure::new(operation, kind)),
        }
    }

    fn endpoint(
        operation: &'static str,
        kind: CatalogFailureKind,
        endpoint: Option<String>,
    ) -> Self {
        Self {
            failure: Arc::new(CatalogFailure::Detail(CatalogFailureDetail {
                operation,
                kind,
                status: None,
                endpoint,
            })),
        }
    }

    fn request(operation: &'static str, endpoint: Option<String>, error: &reqwest::Error) -> Self {
        let kind = if error.is_timeout() {
            CatalogFailureKind::Timeout
        } else if error.is_connect() {
            CatalogFailureKind::Connect
        } else if error.is_body() {
            CatalogFailureKind::Body
        } else if error.is_decode() {
            CatalogFailureKind::Decode
        } else {
            CatalogFailureKind::Request
        };
        Self::endpoint(operation, kind, endpoint)
    }

    fn http(operation: &'static str, endpoint: Option<String>, status: u16) -> Self {
        Self {
            failure: Arc::new(CatalogFailure::Detail(CatalogFailureDetail {
                operation,
                kind: CatalogFailureKind::Http,
                status: Some(status),
                endpoint,
            })),
        }
    }

    fn with_fallback(self, fallback: Self) -> Self {
        Self {
            failure: Arc::new(CatalogFailure::Fallback {
                primary: self.failure,
                fallback: fallback.failure,
            }),
        }
    }

    fn into_failure(self) -> Arc<CatalogFailure> {
        self.failure
    }

    const fn from_failure(failure: Arc<CatalogFailure>) -> Self {
        Self { failure }
    }
}

impl fmt::Display for CatalogLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.failure.fmt(formatter)
    }
}

impl std::error::Error for CatalogLoadError {}

#[derive(Debug)]
enum CatalogFailure {
    Detail(CatalogFailureDetail),
    Fallback {
        primary: Arc<Self>,
        fallback: Arc<Self>,
    },
}

impl CatalogFailure {
    const fn new(operation: &'static str, kind: CatalogFailureKind) -> Self {
        Self::Detail(CatalogFailureDetail {
            operation,
            kind,
            status: None,
            endpoint: None,
        })
    }
}

impl fmt::Display for CatalogFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Detail(detail) => detail.fmt(formatter),
            Self::Fallback { primary, fallback } => {
                write!(formatter, "{primary}; cache fallback failed: {fallback}")
            }
        }
    }
}

#[derive(Debug)]
struct CatalogFailureDetail {
    operation: &'static str,
    kind: CatalogFailureKind,
    status: Option<u16>,
    endpoint: Option<String>,
}

impl fmt::Display for CatalogFailureDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "failed to {} ({})", self.operation, self.kind)?;
        if let Some(status) = self.status {
            write!(formatter, ", HTTP {status}")?;
        }
        if let Some(endpoint) = &self.endpoint {
            write!(formatter, " at {endpoint}")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
enum CatalogFailureKind {
    Timeout,
    Connect,
    Request,
    Http,
    Body,
    Decode,
    Io,
    Canceled,
}

impl fmt::Display for CatalogFailureKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::Request => "request",
            Self::Http => "http",
            Self::Body => "body",
            Self::Decode => "decode",
            Self::Io => "io",
            Self::Canceled => "canceled",
        })
    }
}

/// gRPC catalog service backed by the shared configuration store.
#[derive(Debug)]
pub struct CatalogService<S = FilesystemConfigStore> {
    store: S,
    loader: CatalogLoader,
}

impl<S> CatalogService<S>
where
    S: ConfigStore,
{
    /// Create a catalog service from a config store and catalog loader.
    #[must_use]
    pub fn new(store: S, loader: CatalogLoader) -> Arc<Self> {
        Arc::new(Self { store, loader })
    }
}

#[tonic::async_trait]
impl<S> catalog_proto::catalog_service_server::CatalogService for CatalogService<S>
where
    S: ConfigStore,
{
    async fn search_catalog_models(
        &self,
        request: Request<catalog_proto::SearchCatalogModelsRequest>,
    ) -> Result<Response<catalog_proto::SearchCatalogModelsResponse>, Status> {
        let request = request.into_inner();
        let loaded = self.load_catalog().await?;
        let limit = request_limit(request.limit);
        let search_limit = if request.provider_id.trim().is_empty() {
            limit
        } else {
            usize::MAX
        };
        let models = loaded
            .catalog()
            .search(&request.query, search_limit)
            .into_iter()
            .filter(|catalog_match| {
                request.provider_id.trim().is_empty()
                    || catalog_match.model.provider_id == request.provider_id
            })
            .take(limit)
            .filter_map(|catalog_match| catalog_summary(loaded.catalog(), &catalog_match.model))
            .collect();

        Ok(Response::new(catalog_proto::SearchCatalogModelsResponse {
            models,
            stale: loaded.stale(),
        }))
    }

    async fn suggest_catalog_matches(
        &self,
        request: Request<catalog_proto::SuggestCatalogMatchesRequest>,
    ) -> Result<Response<catalog_proto::SuggestCatalogMatchesResponse>, Status> {
        let request = request.into_inner();
        let config = self.load_config().await?;
        let provider_url = config
            .providers
            .get(&request.provider_key)
            .map(|provider| provider.url.as_str());
        let loaded = self.load_catalog().await?;
        let matches = loaded
            .catalog()
            .suggest_matches(
                non_empty(request.provider_key.as_str()),
                provider_url,
                &request.model_key,
                request_limit(request.limit),
            )
            .into_iter()
            .filter_map(|catalog_match| proto_catalog_match(loaded.catalog(), catalog_match))
            .collect();

        Ok(Response::new(
            catalog_proto::SuggestCatalogMatchesResponse {
                matches,
                stale: loaded.stale(),
            },
        ))
    }

    async fn get_model_catalog_drift(
        &self,
        request: Request<catalog_proto::GetModelCatalogDriftRequest>,
    ) -> Result<Response<catalog_proto::GetModelCatalogDriftResponse>, Status> {
        let request = request.into_inner();
        let config = self.load_config().await?;
        let (provider, model) =
            configured_model(&config, &request.provider_key, &request.model_key)?;
        let loaded = self.load_catalog().await?;
        let catalog_match = resolve_catalog_match(
            loaded.catalog(),
            &request.provider_key,
            provider,
            &request.model_key,
            model,
            None,
            config_proto::CatalogMatchSource::Auto,
        )?;
        let fields = catalog_match
            .as_ref()
            .and_then(|catalog_match| {
                lookup_catalog_model(loaded.catalog(), catalog_match).map(|catalog_model| {
                    let facts = configured_facts(model);
                    catalog_model
                        .compare_configured_facts(&facts)
                        .into_iter()
                        .map(proto_drift_field)
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();

        Ok(Response::new(catalog_proto::GetModelCatalogDriftResponse {
            fields,
            stale: loaded.stale(),
            catalog_match,
        }))
    }

    async fn set_model_catalog_match(
        &self,
        request: Request<catalog_proto::SetModelCatalogMatchRequest>,
    ) -> Result<Response<catalog_proto::SetModelCatalogMatchResponse>, Status> {
        let request = request.into_inner();
        let mut config = self.load_config().await?;
        let provider = config
            .providers
            .get_mut(&request.provider_key)
            .ok_or_else(|| provider_not_found(&request.provider_key))?;
        let model = provider
            .models
            .get_mut(&request.model_key)
            .ok_or_else(|| model_not_found(&request.provider_key, &request.model_key))?;

        if request.clear_match {
            model.catalog_match = None;
        } else {
            let catalog_match = request.catalog_match.ok_or_else(|| {
                Status::invalid_argument("catalog_match is required unless clear_match is true")
            })?;
            model.catalog_match = Some(normalize_catalog_match(
                catalog_match,
                config_proto::CatalogMatchSource::Manual,
            )?);
        }

        let config = self.patch_providers(config, &request.expected_etag).await?;
        Ok(Response::new(catalog_proto::SetModelCatalogMatchResponse {
            config: Some(config),
        }))
    }

    async fn apply_catalog_model_defaults(
        &self,
        request: Request<catalog_proto::ApplyCatalogModelDefaultsRequest>,
    ) -> Result<Response<catalog_proto::ApplyCatalogModelDefaultsResponse>, Status> {
        let request = request.into_inner();
        let mut config = self.load_config().await?;
        let loaded = self.load_catalog().await?;
        let (provider, model) =
            configured_model(&config, &request.provider_key, &request.model_key)?;
        let catalog_match = resolve_catalog_match(
            loaded.catalog(),
            &request.provider_key,
            provider,
            &request.model_key,
            model,
            request.catalog_match,
            config_proto::CatalogMatchSource::Applied,
        )?
        .ok_or_else(|| Status::not_found("no high-confidence catalog match found"))?;
        let catalog_model = lookup_catalog_model(loaded.catalog(), &catalog_match)
            .ok_or_else(|| Status::not_found("catalog model was not found"))?;
        let fields = RequestedDefaultFields::from_proto(&request.fields);

        let provider = config
            .providers
            .get_mut(&request.provider_key)
            .ok_or_else(|| provider_not_found(&request.provider_key))?;
        let model = provider
            .models
            .get_mut(&request.model_key)
            .ok_or_else(|| model_not_found(&request.provider_key, &request.model_key))?;
        apply_defaults(model, catalog_model, &catalog_match, fields);

        let config = self.patch_providers(config, &request.expected_etag).await?;
        Ok(Response::new(
            catalog_proto::ApplyCatalogModelDefaultsResponse {
                config: Some(config),
            },
        ))
    }
}

impl<S> CatalogService<S>
where
    S: ConfigStore,
{
    async fn load_catalog(&self) -> Result<LoadedCatalog, Status> {
        self.loader.load().await.map_err(|error| {
            tracing::warn!(%error, "catalog load failed");
            Status::unavailable(error.to_string())
        })
    }

    async fn load_config(&self) -> Result<config_proto::Configuration, Status> {
        self.store.load().await.map_err(|error| {
            tracing::warn!(%error, "catalog config load failed");
            config_error_to_status(&error)
        })
    }

    async fn patch_providers(
        &self,
        config: config_proto::Configuration,
        expected_etag: &str,
    ) -> Result<config_proto::Configuration, Status> {
        self.store
            .patch(
                ConfigPatch {
                    providers: Some(config.providers),
                    mcp_servers: None,
                    runtime_defaults: None,
                },
                expected_etag,
            )
            .await
            .map_err(|error| {
                tracing::warn!(%error, "catalog config patch failed");
                config_error_to_status(&error)
            })
    }
}

#[derive(Clone, Copy, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "these flags directly represent independent requested proto fields"
)]
struct RequestedDefaultFields {
    name: bool,
    model_type: bool,
    limits: bool,
    capabilities: bool,
    cost: bool,
}

impl RequestedDefaultFields {
    fn from_proto(fields: &[i32]) -> Self {
        if fields.is_empty() {
            return Self {
                name: true,
                model_type: true,
                limits: true,
                capabilities: true,
                cost: true,
            };
        }

        let mut requested = Self {
            name: false,
            model_type: false,
            limits: false,
            capabilities: false,
            cost: false,
        };
        for field in fields {
            match catalog_proto::CatalogDefaultField::try_from(*field).ok() {
                Some(catalog_proto::CatalogDefaultField::Name) => requested.name = true,
                Some(catalog_proto::CatalogDefaultField::Type) => requested.model_type = true,
                Some(catalog_proto::CatalogDefaultField::Limits) => requested.limits = true,
                Some(catalog_proto::CatalogDefaultField::Capabilities) => {
                    requested.capabilities = true;
                }
                Some(catalog_proto::CatalogDefaultField::Cost) => requested.cost = true,
                Some(catalog_proto::CatalogDefaultField::Unspecified) | None => {}
            }
        }
        requested
    }
}

fn apply_defaults(
    model: &mut config_proto::Model,
    catalog_model: &models_dev::CatalogModel,
    catalog_match: &config_proto::ModelCatalogMatch,
    fields: RequestedDefaultFields,
) {
    model.catalog_match = Some(catalog_match.clone());

    if fields.name && model.name.trim().is_empty() {
        model.name.clone_from(&catalog_model.name);
    }

    if fields.model_type && model_type_is_blank(model.r#type) {
        model.r#type = config_proto::ModelType::Llm as i32;
    }

    if fields.limits {
        let limits = model
            .limits
            .get_or_insert_with(config_proto::ModelLimits::default);
        if limits.context.is_none() {
            limits.context = u64_to_u32(catalog_model.limits.context);
        }
        if limits.output.is_none() {
            limits.output = u64_to_u32(catalog_model.limits.output);
        }
    }

    if fields.capabilities && model.capabilities.is_none() {
        model.capabilities = Some(config_proto::ModelCapabilities {
            tools: catalog_model.capabilities.tool_call.unwrap_or(false),
            image: catalog_supports_image(catalog_model),
        });
    }

    if fields.cost {
        model.cost = Some(model_cost_from_catalog(catalog_model));
    }

    let reasoning_efforts = catalog_reasoning_efforts(catalog_model);
    if !reasoning_efforts.is_empty() {
        let options = model
            .options
            .get_or_insert_with(config_proto::ModelOptions::default);
        if options.reasoning_efforts.is_empty() {
            options.reasoning_efforts = reasoning_efforts;
        }
    }
}

fn catalog_reasoning_efforts(catalog_model: &models_dev::CatalogModel) -> Vec<String> {
    let mut efforts = catalog_model
        .reasoning_options
        .iter()
        .flat_map(|option| match option {
            models_dev::CatalogReasoningOption::Effort { values } => values.as_slice(),
            _ => &[],
        })
        .filter_map(|value| match value {
            models_dev::CatalogReasoningEffortValue::String(value) if !value.is_empty() => {
                Some(value.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    efforts.sort();
    efforts.dedup();
    efforts
}

fn model_type_is_blank(value: i32) -> bool {
    matches!(
        config_proto::ModelType::try_from(value),
        Ok(config_proto::ModelType::Unspecified) | Err(_)
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the arguments preserve explicit serving and catalog match inputs"
)]
fn resolve_catalog_match(
    catalog: &models_dev::ModelCatalog,
    provider_key: &str,
    provider: &config_proto::Provider,
    model_key: &str,
    model: &config_proto::Model,
    selected: Option<config_proto::ModelCatalogMatch>,
    default_source: config_proto::CatalogMatchSource,
) -> Result<Option<config_proto::ModelCatalogMatch>, Status> {
    if let Some(catalog_match) = selected.or_else(|| model.catalog_match.clone()) {
        return normalize_catalog_match(catalog_match, default_source).map(Some);
    }

    let Some(suggestion) = catalog
        .suggest_matches(
            Some(provider_key),
            Some(provider.url.as_str()),
            model_key,
            1,
        )
        .into_iter()
        .find(|suggestion| high_confidence(suggestion.confidence))
    else {
        return Ok(None);
    };

    let upstream = derive_upstream_model_ref(catalog, &suggestion.model);
    Ok(Some(config_proto::ModelCatalogMatch {
        provider_id: suggestion.model.provider_id,
        model_id: suggestion.model.model_id,
        upstream_provider_id: upstream
            .as_ref()
            .map(|upstream| upstream.provider_id.clone()),
        upstream_model_id: upstream.map(|upstream| upstream.model_id),
        source: default_source as i32,
    }))
}

fn normalize_catalog_match(
    mut catalog_match: config_proto::ModelCatalogMatch,
    default_source: config_proto::CatalogMatchSource,
) -> Result<config_proto::ModelCatalogMatch, Status> {
    catalog_match.provider_id = catalog_match.provider_id.trim().to_owned();
    catalog_match.model_id = catalog_match.model_id.trim().to_owned();
    catalog_match.upstream_provider_id = cleaned_optional(catalog_match.upstream_provider_id);
    catalog_match.upstream_model_id = cleaned_optional(catalog_match.upstream_model_id);

    if catalog_match.provider_id.is_empty() || catalog_match.model_id.is_empty() {
        return Err(Status::invalid_argument(
            "catalog_match provider_id and model_id are required",
        ));
    }

    if matches!(
        config_proto::CatalogMatchSource::try_from(catalog_match.source),
        Ok(config_proto::CatalogMatchSource::Unspecified) | Err(_)
    ) {
        catalog_match.source = default_source as i32;
    }

    Ok(catalog_match)
}

fn cleaned_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn lookup_catalog_model<'a>(
    catalog: &'a models_dev::ModelCatalog,
    catalog_match: &config_proto::ModelCatalogMatch,
) -> Option<&'a models_dev::CatalogModel> {
    let upstream_provider_id = catalog_match
        .upstream_provider_id
        .as_deref()
        .filter(|value| !value.is_empty());
    let upstream_model_id = catalog_match
        .upstream_model_id
        .as_deref()
        .filter(|value| !value.is_empty());

    if let (Some(provider_id), Some(model_id)) = (upstream_provider_id, upstream_model_id)
        && let Some(model) = catalog.model(provider_id, model_id)
    {
        return Some(model);
    }

    catalog.model(&catalog_match.provider_id, &catalog_match.model_id)
}

fn configured_model<'a>(
    config: &'a config_proto::Configuration,
    provider_key: &str,
    model_key: &str,
) -> Result<(&'a config_proto::Provider, &'a config_proto::Model), Status> {
    let provider = config
        .providers
        .get(provider_key)
        .ok_or_else(|| provider_not_found(provider_key))?;
    let model = provider
        .models
        .get(model_key)
        .ok_or_else(|| model_not_found(provider_key, model_key))?;
    Ok((provider, model))
}

fn configured_facts(model: &config_proto::Model) -> models_dev::ConfiguredModelFacts {
    let limits = model.limits.unwrap_or_default();
    let capabilities = model.capabilities;

    models_dev::ConfiguredModelFacts {
        limits: models_dev::CatalogLimits {
            context: limits.context.map(u64::from),
            input: None,
            output: limits.output.map(u64::from),
        },
        capabilities: models_dev::CatalogCapabilities {
            attachment: capabilities.map(|capabilities| capabilities.image),
            reasoning: None,
            tool_call: capabilities.map(|capabilities| capabilities.tools),
            structured_output: None,
            temperature: None,
        },
        cost: model.cost.as_ref().map(|cost| models_dev::CatalogCost {
            input: cost.input_per_million_tokens,
            output: cost.output_per_million_tokens,
            cache_read: cost.cache_read_per_million_tokens,
            cache_write: cost.cache_write_per_million_tokens,
            ..models_dev::CatalogCost::default()
        }),
        ..models_dev::ConfiguredModelFacts::default()
    }
}

fn model_cost_from_catalog(catalog_model: &models_dev::CatalogModel) -> config_proto::ModelCost {
    config_proto::ModelCost {
        input_per_million_tokens: catalog_model.cost.input,
        output_per_million_tokens: catalog_model.cost.output,
        cache_read_per_million_tokens: catalog_model.cost.cache_read,
        cache_write_per_million_tokens: catalog_model.cost.cache_write,
        image_input_per_million_tokens: None,
        image_input_per_image: None,
        currency: "USD".to_owned(),
    }
}

fn catalog_summary(
    catalog: &models_dev::ModelCatalog,
    model_ref: &models_dev::CatalogModelRef,
) -> Option<catalog_proto::CatalogModelSummary> {
    let provider = catalog.provider(&model_ref.provider_id)?;
    let model = provider.models.get(&model_ref.model_id)?;
    Some(catalog_proto::CatalogModelSummary {
        model: Some(catalog_proto::CatalogModelRef {
            provider_id: provider.id.clone(),
            model_id: model.id.clone(),
        }),
        provider_name: provider.name.clone(),
        model_name: model.name.clone(),
        family: model.family.clone(),
        status: model.status.clone(),
        limits: Some(catalog_proto::CatalogModelLimitsSummary {
            context: u64_to_u32(model.limits.context),
            output: u64_to_u32(model.limits.output),
        }),
        cost: Some(catalog_proto::CatalogModelCostSummary {
            input_per_million_tokens: model.cost.input,
            output_per_million_tokens: model.cost.output,
            currency: "USD".to_owned(),
            cache_read_per_million_tokens: model.cost.cache_read,
            cache_write_per_million_tokens: model.cost.cache_write,
            image_input_per_million_tokens: None,
            image_input_per_image: None,
        }),
        capabilities: Some(catalog_proto::CatalogModelCapabilitySummary {
            tools: model.capabilities.tool_call.unwrap_or(false),
            image: catalog_supports_image(model),
            audio: catalog_supports_audio(model),
        }),
        reasoning: Some(catalog_proto::CatalogModelReasoningSummary {
            supported: model.capabilities.reasoning.unwrap_or(false)
                || !model.reasoning_options.is_empty(),
            effort: catalog_reasoning_effort(model),
            efforts: catalog_reasoning_efforts(model),
        }),
    })
}

fn proto_catalog_match(
    catalog: &models_dev::ModelCatalog,
    catalog_match: models_dev::CatalogMatch,
) -> Option<catalog_proto::CatalogMatch> {
    let provider = catalog.provider(&catalog_match.model.provider_id)?;
    let model = provider.models.get(&catalog_match.model.model_id)?;
    let upstream_model = derive_upstream_model_ref(catalog, &catalog_match.model).map(|model| {
        catalog_proto::CatalogModelRef {
            provider_id: model.provider_id,
            model_id: model.model_id,
        }
    });

    Some(catalog_proto::CatalogMatch {
        model: Some(catalog_proto::CatalogModelRef {
            provider_id: provider.id.clone(),
            model_id: model.id.clone(),
        }),
        provider_name: provider.name.clone(),
        model_name: model.name.clone(),
        confidence: proto_confidence(catalog_match.confidence) as i32,
        reasons: vec![catalog_match.reason],
        upstream_model,
    })
}

fn derive_upstream_model_ref(
    catalog: &models_dev::ModelCatalog,
    model_ref: &models_dev::CatalogModelRef,
) -> Option<models_dev::CatalogModelRef> {
    let (provider_id, model_id) = model_ref.model_id.split_once('/')?;
    if provider_id.is_empty()
        || model_id.is_empty()
        || catalog.model(provider_id, model_id).is_none()
    {
        return None;
    }

    Some(models_dev::CatalogModelRef::new(provider_id, model_id))
}

const fn proto_confidence(
    confidence: models_dev::CatalogMatchConfidence,
) -> catalog_proto::CatalogMatchConfidence {
    match confidence {
        models_dev::CatalogMatchConfidence::Exact => catalog_proto::CatalogMatchConfidence::Exact,
        models_dev::CatalogMatchConfidence::High => catalog_proto::CatalogMatchConfidence::High,
        models_dev::CatalogMatchConfidence::Suggested => {
            catalog_proto::CatalogMatchConfidence::Suggested
        }
    }
}

const fn high_confidence(confidence: models_dev::CatalogMatchConfidence) -> bool {
    matches!(
        confidence,
        models_dev::CatalogMatchConfidence::Exact | models_dev::CatalogMatchConfidence::High
    )
}

fn proto_drift_field(drift: models_dev::CatalogDrift) -> catalog_proto::CatalogDriftField {
    catalog_proto::CatalogDriftField {
        field: drift_field_name(drift.field).to_owned(),
        configured: drift_value(drift.configured),
        catalog: drift_value(drift.catalog),
        severity: drift_severity(drift.field) as i32,
    }
}

const fn drift_field_name(field: models_dev::CatalogDriftField) -> &'static str {
    match field {
        models_dev::CatalogDriftField::Status => "status",
        models_dev::CatalogDriftField::LimitContext => "limit_context",
        models_dev::CatalogDriftField::LimitInput => "limit_input",
        models_dev::CatalogDriftField::LimitOutput => "limit_output",
        models_dev::CatalogDriftField::CapabilityAttachment => "capability_attachment",
        models_dev::CatalogDriftField::CapabilityReasoning => "capability_reasoning",
        models_dev::CatalogDriftField::CapabilityToolCall => "capability_tool_call",
        models_dev::CatalogDriftField::CapabilityStructuredOutput => "capability_structured_output",
        models_dev::CatalogDriftField::CapabilityTemperature => "capability_temperature",
        models_dev::CatalogDriftField::ModalityInput => "modality_input",
        models_dev::CatalogDriftField::ModalityOutput => "modality_output",
        models_dev::CatalogDriftField::ReasoningOptions => "reasoning_options",
        models_dev::CatalogDriftField::ProviderShape => "provider_shape",
        models_dev::CatalogDriftField::CostInput => "cost_input",
        models_dev::CatalogDriftField::CostOutput => "cost_output",
        models_dev::CatalogDriftField::CostCacheRead => "cost_cache_read",
        models_dev::CatalogDriftField::CostCacheWrite => "cost_cache_write",
    }
}

const fn drift_severity(
    field: models_dev::CatalogDriftField,
) -> catalog_proto::CatalogDriftSeverity {
    match field {
        models_dev::CatalogDriftField::LimitContext
        | models_dev::CatalogDriftField::LimitInput
        | models_dev::CatalogDriftField::LimitOutput
        | models_dev::CatalogDriftField::CapabilityAttachment
        | models_dev::CatalogDriftField::CapabilityReasoning
        | models_dev::CatalogDriftField::CapabilityToolCall
        | models_dev::CatalogDriftField::CapabilityStructuredOutput
        | models_dev::CatalogDriftField::CapabilityTemperature => {
            catalog_proto::CatalogDriftSeverity::Actionable
        }
        models_dev::CatalogDriftField::Status
        | models_dev::CatalogDriftField::ReasoningOptions
        | models_dev::CatalogDriftField::ProviderShape
        | models_dev::CatalogDriftField::CostInput
        | models_dev::CatalogDriftField::CostOutput
        | models_dev::CatalogDriftField::CostCacheRead
        | models_dev::CatalogDriftField::CostCacheWrite => {
            catalog_proto::CatalogDriftSeverity::Warning
        }
        models_dev::CatalogDriftField::ModalityInput
        | models_dev::CatalogDriftField::ModalityOutput => {
            catalog_proto::CatalogDriftSeverity::Info
        }
    }
}

fn drift_value(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value,
        value => value.to_string(),
    }
}

fn catalog_supports_image(model: &models_dev::CatalogModel) -> bool {
    model.capabilities.attachment.unwrap_or(false)
        || model
            .modalities
            .input
            .iter()
            .any(|modality| modality.eq_ignore_ascii_case("image"))
}

fn catalog_supports_audio(model: &models_dev::CatalogModel) -> bool {
    model
        .modalities
        .input
        .iter()
        .chain(model.modalities.output.iter())
        .any(|modality| modality.eq_ignore_ascii_case("audio"))
}

fn catalog_reasoning_effort(model: &models_dev::CatalogModel) -> Option<String> {
    model
        .reasoning_options
        .iter()
        .find_map(|option| match option {
            models_dev::CatalogReasoningOption::Effort { values } => {
                let values = values
                    .iter()
                    .filter_map(|value| match value {
                        models_dev::CatalogReasoningEffortValue::String(value) => {
                            Some(value.clone())
                        }
                        models_dev::CatalogReasoningEffortValue::Other(_) => None,
                    })
                    .collect::<Vec<_>>();
                (!values.is_empty()).then(|| values.join(","))
            }
            _ => None,
        })
}

fn u64_to_u32(value: Option<u64>) -> Option<u32> {
    value.and_then(|value| u32::try_from(value).ok())
}

fn request_limit(limit: u32) -> usize {
    if limit == 0 {
        DEFAULT_LIMIT
    } else {
        usize::try_from(limit).unwrap_or(MAX_LIMIT).min(MAX_LIMIT)
    }
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn provider_not_found(provider_key: &str) -> Status {
    Status::not_found(format!("provider '{provider_key}' was not found"))
}

fn model_not_found(provider_key: &str, model_key: &str) -> Status {
    Status::not_found(format!(
        "model '{model_key}' was not found in provider '{provider_key}'"
    ))
}

fn config_error_to_status(error: &ConfigError) -> Status {
    if error.is_etag_mismatch() {
        Status::failed_precondition(error.to_string())
    } else if error.is_invalid() {
        Status::invalid_argument(error.to_string())
    } else {
        Status::internal(error.to_string())
    }
}
