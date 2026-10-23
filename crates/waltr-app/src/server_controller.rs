//! Process-owned embedded server lifecycle.

use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use waltr_server::{
    LocalBearer,
    embedded::{
        EmbeddedBuildError, EmbeddedCatalogFallback, EmbeddedCatalogSource, EmbeddedOutboundPolicy,
        EmbeddedServerConfig, build_embedded_server,
    },
};
use rand::TryRngCore as _;
use tokio::sync::oneshot;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const STARTUP_WAIT_TIMEOUT: Duration = Duration::from_secs(65);

#[derive(Clone)]
pub(crate) struct ServerConnection {
    pub(crate) generation: u64,
    pub(crate) endpoint: String,
    pub(crate) bearer: String,
    pub(crate) first_catalog_download: bool,
}

impl fmt::Debug for ServerConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerConnection")
            .field("generation", &self.generation)
            .field("endpoint", &"<loopback>")
            .field("bearer", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ServerPhase {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

#[derive(Debug)]
enum ControllerState {
    Stopped,
    Starting {
        generation: u64,
        first_catalog_download: bool,
        cancellation: Arc<AtomicBool>,
    },
    Running {
        connection: ServerConnection,
        shutdown: Option<oneshot::Sender<()>>,
        thread: Option<thread::JoinHandle<()>>,
    },
    Stopping {
        generation: u64,
        failed: bool,
    },
    Failed,
}

enum StartReservation {
    Existing(ServerConnection),
    New {
        generation: u64,
        cancellation: Arc<AtomicBool>,
    },
}

#[derive(Debug)]
struct ControllerInner {
    state: Mutex<ControllerState>,
    changed: Condvar,
    next_generation: AtomicU64,
    reaper: mpsc::Sender<ReaperCommand>,
}

enum ReaperCommand {
    Reap {
        generation: u64,
        thread: thread::JoinHandle<()>,
    },
    Exited {
        generation: u64,
        succeeded: bool,
    },
}

/// Serialized owner of one embedded server generation.
#[derive(Clone, Debug)]
pub(crate) struct EmbeddedServerController {
    inner: Arc<ControllerInner>,
}

impl Default for EmbeddedServerController {
    fn default() -> Self {
        let (reaper, commands) = mpsc::channel();
        let inner = Arc::new(ControllerInner {
            state: Mutex::new(ControllerState::Stopped),
            changed: Condvar::new(),
            next_generation: AtomicU64::new(0),
            reaper,
        });
        let weak = Arc::downgrade(&inner);
        thread::Builder::new()
            .name("waltr-server-reaper".to_owned())
            .spawn(move || run_reaper_worker(weak, commands))
            .expect("embedded server reaper thread must start before the controller is published");
        Self { inner }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AppPrivateRoots {
    data: PathBuf,
    config: PathBuf,
}

impl AppPrivateRoots {
    pub(crate) fn new(data: PathBuf, config: PathBuf) -> Result<Self, ServerStartError> {
        if !data.is_absolute() || !config.is_absolute() {
            return Err(ServerStartError::Paths);
        }
        Ok(Self { data, config })
    }

    fn server_config(&self, bearer: LocalBearer) -> EmbeddedServerConfig {
        let server_data = self.data.join("server");
        EmbeddedServerConfig {
            resources_root: self.config.join("server"),
            waltr_database_path: server_data.join("waltr.sqlite3"),
            mcp_database_path: server_data.join("mcp.sqlite3"),
            session_root: server_data.join("sessions"),
            workspace_root: self.data.join("workspace"),
            catalog_source: EmbeddedCatalogSource::Url(MODELS_DEV_URL.to_owned()),
            catalog_cache_path: server_data.join("models/api.json"),
            catalog_fallback: Some(EmbeddedCatalogFallback::BuiltIn),
            default_agent: "coding".to_owned(),
            local_bearer: bearer,
            outbound_policy: EmbeddedOutboundPolicy::default(),
            interaction_limits: Default::default(),
        }
    }

    fn prepare(&self) -> Result<(), ServerStartError> {
        for path in [
            self.data.join("server/sessions"),
            self.data.join("server/models"),
            self.data.join("workspace"),
            self.config.join("server"),
        ] {
            std::fs::create_dir_all(path).map_err(|_| ServerStartError::Paths)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn all_server_paths(&self) -> [PathBuf; 6] {
        let config = self.server_config(LocalBearer::new("test-bearer").expect("test bearer"));
        [
            config.resources_root,
            config.waltr_database_path,
            config.mcp_database_path,
            config.session_root,
            config.workspace_root,
            config.catalog_cache_path,
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ServerStartError {
    Paths,
    Entropy,
    Thread,
    Build(EmbeddedBuildError),
    Readiness,
}

impl fmt::Display for ServerStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Paths => "embedded server paths are unavailable",
            Self::Entropy => "embedded server credential initialization failed",
            Self::Thread => "embedded server thread could not start",
            Self::Build(error) => return error.fmt(formatter),
            Self::Readiness => "embedded server readiness timed out",
        })
    }
}

impl std::error::Error for ServerStartError {}

impl ServerStartError {
    #[cfg(any(target_os = "android", test))]
    pub(crate) const fn telemetry_category(self) -> &'static str {
        match self {
            Self::Paths => "paths",
            Self::Entropy => "entropy",
            Self::Thread => "thread",
            Self::Build(EmbeddedBuildError::Paths) => "build_paths",
            Self::Build(EmbeddedBuildError::Listener) => "build_listener",
            Self::Build(EmbeddedBuildError::Persistence) => "build_persistence",
            Self::Build(EmbeddedBuildError::Catalog) => "build_catalog",
            Self::Build(EmbeddedBuildError::Resources) => "build_resources",
            Self::Build(EmbeddedBuildError::Application) => "build_application",
            Self::Readiness => "readiness",
        }
    }
}

impl EmbeddedServerController {
    pub(crate) fn start(
        &self,
        roots: AppPrivateRoots,
    ) -> Result<ServerConnection, ServerStartError> {
        if let Some(connection) = self.connection() {
            return Ok(connection);
        }
        let first_catalog_download = !roots.data.join("server/models/api.json").exists();
        roots.prepare()?;
        let token = random_bearer()?;
        let bearer = LocalBearer::new(token.clone()).map_err(|_| ServerStartError::Entropy)?;
        let (generation, cancellation) = match self.reserve_start(first_catalog_download) {
            StartReservation::Existing(connection) => return Ok(connection),
            StartReservation::New {
                generation,
                cancellation,
            } => (generation, cancellation),
        };
        let config = roots.server_config(bearer);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (serve_tx, serve_rx) = std::sync::mpsc::sync_channel(0);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let reaper = self.inner.reaper.clone();
        let thread_cancellation = Arc::clone(&cancellation);
        let thread = thread::Builder::new()
            .name(format!("waltr-server-{generation}"))
            .spawn(move || {
                run_server_thread(
                    reaper,
                    generation,
                    thread_cancellation,
                    config,
                    shutdown_rx,
                    ready_tx,
                    serve_rx,
                );
            })
            .map_err(|_| {
                self.finish_failed_start(generation);
                ServerStartError::Thread
            })?;

        let endpoint = match receive_startup(&ready_rx, STARTUP_WAIT_TIMEOUT) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                cancellation.store(true, Ordering::Release);
                drop(shutdown_tx);
                drop(serve_tx);
                self.finish_threaded_start(generation, true);
                self.reap_generation(generation, thread);
                return Err(error);
            }
        };
        let connection = ServerConnection {
            generation,
            endpoint,
            bearer: token,
            first_catalog_download,
        };
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cancellation.load(Ordering::Acquire)
            || !matches!(&*state, ControllerState::Starting { generation: current, .. } if *current == generation)
        {
            if matches!(&*state, ControllerState::Starting { generation: current, .. } if *current == generation)
            {
                *state = ControllerState::Stopping {
                    generation,
                    failed: true,
                };
                self.inner.changed.notify_all();
            }
            drop(state);
            drop(shutdown_tx);
            drop(serve_tx);
            self.reap_generation(generation, thread);
            return Err(ServerStartError::Readiness);
        }
        *state = ControllerState::Running {
            connection: connection.clone(),
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        };
        self.inner.changed.notify_all();
        if serve_tx.send(()).is_err() {
            let (shutdown, thread) = match &mut *state {
                ControllerState::Running {
                    connection,
                    shutdown,
                    thread,
                } if connection.generation == generation => {
                    let values = (shutdown.take(), thread.take());
                    *state = ControllerState::Stopping {
                        generation,
                        failed: true,
                    };
                    values
                }
                _ => (None, None),
            };
            self.inner.changed.notify_all();
            drop(state);
            drop(shutdown);
            if let Some(thread) = thread {
                self.reap_generation(generation, thread);
            }
            return Err(ServerStartError::Thread);
        }
        if !matches!(&*state, ControllerState::Running { connection: current, .. } if current.generation == generation)
        {
            let (shutdown, thread) = match &mut *state {
                ControllerState::Running {
                    connection,
                    shutdown,
                    thread,
                } if connection.generation == generation => {
                    let values = (shutdown.take(), thread.take());
                    *state = ControllerState::Stopping {
                        generation,
                        failed: true,
                    };
                    values
                }
                _ => (None, None),
            };
            self.inner.changed.notify_all();
            drop(state);
            drop(shutdown);
            if let Some(thread) = thread {
                self.reap_generation(generation, thread);
            }
            return Err(ServerStartError::Readiness);
        }
        drop(state);
        Ok(connection)
    }

    pub(crate) fn stop(&self) {
        let (generation, shutdown, thread) = loop {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &mut *state {
                ControllerState::Running {
                    connection,
                    shutdown,
                    thread,
                } => {
                    let generation = connection.generation;
                    let values = (generation, shutdown.take(), thread.take());
                    *state = ControllerState::Stopping {
                        generation,
                        failed: false,
                    };
                    break values;
                }
                ControllerState::Starting {
                    generation,
                    cancellation,
                    ..
                } => {
                    let generation = *generation;
                    cancellation.store(true, Ordering::Release);
                    *state = ControllerState::Stopping {
                        generation,
                        failed: false,
                    };
                    state = self
                        .inner
                        .changed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    drop(state);
                }
                ControllerState::Stopping { .. } => {
                    state = self
                        .inner
                        .changed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    drop(state);
                }
                ControllerState::Stopped => return,
                ControllerState::Failed => {
                    *state = ControllerState::Stopped;
                    self.inner.changed.notify_all();
                    return;
                }
            }
        };
        drop(shutdown);
        if let Some(thread) = thread {
            drop(thread.join());
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(&*state, ControllerState::Stopping { generation: current, .. } if *current == generation)
        {
            *state = ControllerState::Stopped;
            self.inner.changed.notify_all();
        }
    }

    #[cfg(any(target_os = "android", test))]
    pub(crate) fn stop_nonblocking(&self) {
        let detached = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let detached = match &mut *state {
                ControllerState::Running {
                    connection,
                    shutdown,
                    thread,
                } => {
                    let generation = connection.generation;
                    let detached = Some((generation, shutdown.take(), thread.take()));
                    *state = ControllerState::Stopping {
                        generation,
                        failed: false,
                    };
                    detached
                }
                ControllerState::Starting {
                    generation,
                    cancellation,
                    ..
                } => {
                    let generation = *generation;
                    cancellation.store(true, Ordering::Release);
                    *state = ControllerState::Stopping {
                        generation,
                        failed: false,
                    };
                    None
                }
                ControllerState::Failed => {
                    *state = ControllerState::Stopped;
                    None
                }
                ControllerState::Stopped | ControllerState::Stopping { .. } => None,
            };
            self.inner.changed.notify_all();
            detached
        };
        if let Some((generation, shutdown, thread)) = detached {
            drop(shutdown);
            if let Some(thread) = thread {
                self.reap_generation(generation, thread);
            } else {
                finish_generation_exit(&self.inner, generation);
            }
        }
    }

    pub(crate) fn phase(&self) -> ServerPhase {
        match &*self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            ControllerState::Stopped => ServerPhase::Stopped,
            ControllerState::Starting { .. } => ServerPhase::Starting,
            ControllerState::Running { .. } => ServerPhase::Running,
            ControllerState::Stopping { .. } => ServerPhase::Stopping,
            ControllerState::Failed => ServerPhase::Failed,
        }
    }

    pub(crate) fn connection(&self) -> Option<ServerConnection> {
        match &*self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            ControllerState::Running { connection, .. } => Some(connection.clone()),
            _ => None,
        }
    }

    pub(crate) fn first_catalog_download(&self) -> bool {
        matches!(
            &*self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            ControllerState::Starting {
                first_catalog_download: true,
                ..
            }
        )
    }

    fn reserve_start(&self, first_catalog_download: bool) -> StartReservation {
        loop {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &*state {
                ControllerState::Running { connection, .. } => {
                    return StartReservation::Existing(connection.clone());
                }
                ControllerState::Starting { .. } | ControllerState::Stopping { .. } => {
                    state = self
                        .inner
                        .changed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    drop(state);
                }
                ControllerState::Stopped | ControllerState::Failed => {
                    let generation = self
                        .inner
                        .next_generation
                        .fetch_add(1, Ordering::AcqRel)
                        .saturating_add(1);
                    let cancellation = Arc::new(AtomicBool::new(false));
                    *state = ControllerState::Starting {
                        generation,
                        first_catalog_download,
                        cancellation: Arc::clone(&cancellation),
                    };
                    return StartReservation::New {
                        generation,
                        cancellation,
                    };
                }
            }
        }
    }

    fn finish_failed_start(&self, generation: u64) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            ControllerState::Starting {
                generation: current,
                ..
            } if *current == generation => *state = ControllerState::Failed,
            ControllerState::Stopping {
                generation: current,
                ..
            } if *current == generation => *state = ControllerState::Stopped,
            _ => return,
        }
        self.inner.changed.notify_all();
    }

    fn finish_threaded_start(&self, generation: u64, failed: bool) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &mut *state {
            ControllerState::Starting {
                generation: current,
                ..
            } if *current == generation => {
                *state = ControllerState::Stopping { generation, failed };
            }
            ControllerState::Stopping {
                generation: current,
                ..
            } if *current == generation => {}
            _ => return,
        }
        self.inner.changed.notify_all();
    }

    fn reap_generation(&self, generation: u64, thread: thread::JoinHandle<()>) {
        if let Err(mpsc::SendError(ReaperCommand::Reap { thread, .. })) = self
            .inner
            .reaper
            .send(ReaperCommand::Reap { generation, thread })
        {
            drop(thread.join());
            finish_generation_exit(&self.inner, generation);
        }
    }
}

impl Drop for EmbeddedServerController {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.stop();
        }
    }
}

fn run_server_thread(
    reaper: mpsc::Sender<ReaperCommand>,
    generation: u64,
    cancellation: Arc<AtomicBool>,
    config: EmbeddedServerConfig,
    shutdown: oneshot::Receiver<()>,
    ready: std::sync::mpsc::SyncSender<Result<String, ServerStartError>>,
    serve: std::sync::mpsc::Receiver<()>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            drop(ready.send(Err(ServerStartError::Thread)));
            return;
        }
    };
    let succeeded = runtime.block_on(async move {
        let server = match build_embedded_server(config).await {
            Ok(server) => server,
            Err(error) => {
                drop(ready.send(Err(ServerStartError::Build(error))));
                return false;
            }
        };
        if cancellation.load(Ordering::Acquire) {
            server.shutdown().await;
            drop(ready.send(Err(ServerStartError::Readiness)));
            return true;
        }
        if tokio::time::timeout(READY_TIMEOUT, server.wait_until_ready())
            .await
            .is_err()
        {
            server.shutdown().await;
            drop(ready.send(Err(ServerStartError::Readiness)));
            return false;
        }
        if cancellation.load(Ordering::Acquire) {
            server.shutdown().await;
            drop(ready.send(Err(ServerStartError::Readiness)));
            return true;
        }
        let endpoint = format!("http://{}", server.local_addr());
        if ready.send(Ok(endpoint)).is_err() {
            server.shutdown().await;
            return true;
        }
        if serve.recv().is_err() {
            server.shutdown().await;
            return true;
        }
        let result = server
            .serve_with_shutdown(async move { drop(shutdown.await) })
            .await;
        result.is_ok()
    });
    drop(runtime);
    drop(reaper.send(ReaperCommand::Exited {
        generation,
        succeeded,
    }));
}

fn run_reaper_worker(inner: Weak<ControllerInner>, commands: mpsc::Receiver<ReaperCommand>) {
    while let Ok(command) = commands.recv() {
        match command {
            ReaperCommand::Reap { generation, thread } => {
                drop(thread.join());
                if let Some(inner) = inner.upgrade() {
                    finish_generation_exit(&inner, generation);
                }
            }
            ReaperCommand::Exited {
                generation,
                succeeded,
            } => {
                let Some(inner) = inner.upgrade() else {
                    continue;
                };
                let detached = {
                    let mut state = inner
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let ControllerState::Running {
                        connection,
                        shutdown,
                        thread,
                    } = &mut *state
                    else {
                        continue;
                    };
                    if connection.generation != generation {
                        continue;
                    }
                    let detached = (shutdown.take(), thread.take());
                    *state = ControllerState::Stopping {
                        generation,
                        failed: !succeeded,
                    };
                    inner.changed.notify_all();
                    detached
                };
                drop(detached.0);
                if let Some(thread) = detached.1 {
                    drop(thread.join());
                }
                finish_generation_exit(&inner, generation);
            }
        }
    }
}

fn finish_generation_exit(inner: &ControllerInner, generation: u64) {
    let mut state = inner
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let failed = match &*state {
        ControllerState::Stopping {
            generation: current,
            failed,
        } if *current == generation => *failed,
        _ => return,
    };
    *state = if failed {
        ControllerState::Failed
    } else {
        ControllerState::Stopped
    };
    inner.changed.notify_all();
}

fn receive_startup(
    receiver: &std::sync::mpsc::Receiver<Result<String, ServerStartError>>,
    timeout: Duration,
) -> Result<String, ServerStartError> {
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(ServerStartError::Readiness),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(ServerStartError::Thread),
    }
}

fn random_bearer() -> Result<String, ServerStartError> {
    let mut bytes = [0_u8; 32];
    rand::rng()
        .try_fill_bytes(&mut bytes)
        .map_err(|_| ServerStartError::Entropy)?;
    let mut token = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").map_err(|_| ServerStartError::Entropy)?;
    }
    Ok(token)
}

#[cfg(target_os = "android")]
pub(crate) fn android_roots(
    data: &std::path::Path,
    config: &std::path::Path,
) -> Result<AppPrivateRoots, ServerStartError> {
    AppPrivateRoots::new(data.to_path_buf(), config.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn app_paths_are_absolute_separate_and_ambient_free() {
        let roots = AppPrivateRoots::new(
            PathBuf::from("/private/data"),
            PathBuf::from("/private/config"),
        )
        .unwrap();
        let paths = roots.all_server_paths();
        assert!(paths.iter().all(|path| path.is_absolute()));
        assert_eq!(
            paths[0].join(".waltr/resources"),
            PathBuf::from("/private/config/server/.waltr/resources")
        );
        assert_eq!(paths[1], PathBuf::from("/private/data/server/waltr.sqlite3"));
        assert_eq!(paths[2], PathBuf::from("/private/data/server/mcp.sqlite3"));
        assert_eq!(paths[3], PathBuf::from("/private/data/server/sessions"));
        assert_eq!(paths[4], PathBuf::from("/private/data/workspace"));
        assert_eq!(
            paths[5],
            PathBuf::from("/private/data/server/models/api.json")
        );
    }

    #[test]
    fn generated_bearer_has_at_least_256_bits_and_debug_is_redacted() {
        let bearer = random_bearer().unwrap();
        assert_eq!(bearer.len(), 64);
        assert!(bearer.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let connection = ServerConnection {
            generation: 1,
            endpoint: "http://127.0.0.1:1234".to_owned(),
            bearer: bearer.clone(),
            first_catalog_download: true,
        };
        let debug = format!("{connection:?}");
        assert!(!debug.contains(&bearer));
        assert!(!debug.contains("127.0.0.1:1234"));
    }

    #[test]
    fn relative_roots_are_rejected_without_detail() {
        assert_eq!(
            AppPrivateRoots::new(PathBuf::from("data"), PathBuf::from("config")).unwrap_err(),
            ServerStartError::Paths
        );
    }

    #[test]
    fn embedded_build_categories_survive_controller_mapping() {
        for (embedded, category) in [
            (EmbeddedBuildError::Paths, "build_paths"),
            (EmbeddedBuildError::Listener, "build_listener"),
            (EmbeddedBuildError::Persistence, "build_persistence"),
            (EmbeddedBuildError::Catalog, "build_catalog"),
            (EmbeddedBuildError::Resources, "build_resources"),
            (EmbeddedBuildError::Application, "build_application"),
        ] {
            let error = ServerStartError::Build(embedded);
            assert_eq!(error.telemetry_category(), category);
            assert_eq!(error.to_string(), embedded.to_string());
        }
    }

    #[test]
    fn pending_startup_reception_is_bounded() {
        let (_sender, receiver) = std::sync::mpsc::channel();
        let started = Instant::now();
        assert_eq!(
            receive_startup(&receiver, Duration::from_millis(10)),
            Err(ServerStartError::Readiness)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    fn install_fake_running(
        controller: &EmbeddedServerController,
        generation: u64,
        thread: thread::JoinHandle<()>,
    ) {
        controller
            .inner
            .next_generation
            .store(generation, Ordering::Release);
        let (shutdown, _receiver) = oneshot::channel();
        *controller.inner.state.lock().unwrap() = ControllerState::Running {
            connection: ServerConnection {
                generation,
                endpoint: format!("http://127.0.0.1:{generation}"),
                bearer: format!("bearer-{generation}"),
                first_catalog_download: false,
            },
            shutdown: Some(shutdown),
            thread: Some(thread),
        };
    }

    #[test]
    fn nonblocking_stop_stays_stopping_and_serializes_replacement_until_exit() {
        let controller = EmbeddedServerController::default();
        let (release, wait) = std::sync::mpsc::channel();
        let (replacement, replacement_rx) = std::sync::mpsc::channel();
        install_fake_running(
            &controller,
            1,
            thread::spawn(move || assert!(wait.recv().is_ok())),
        );

        let started = Instant::now();
        controller.stop_nonblocking();
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(controller.phase(), ServerPhase::Stopping);

        let replacement_controller = controller.clone();
        let replacement_thread = thread::spawn(move || {
            let reservation = replacement_controller.reserve_start(false);
            replacement.send(reservation).unwrap();
        });
        assert!(
            replacement_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "replacement must wait for the detached generation to exit"
        );

        assert!(release.send(()).is_ok());
        let StartReservation::New {
            generation,
            cancellation,
        } = replacement_rx.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("replacement reservation");
        };
        assert_eq!(generation, 2);
        cancellation.store(true, Ordering::Release);
        *controller.inner.state.lock().unwrap() = ControllerState::Stopped;
        controller.inner.changed.notify_all();
        replacement_thread.join().unwrap();
    }

    #[test]
    fn delayed_old_reaper_cannot_mutate_running_replacement() {
        let controller = EmbeddedServerController::default();
        let old_inner = Arc::clone(&controller.inner);
        let (release, wait) = std::sync::mpsc::channel();
        let (finished, finished_rx) = std::sync::mpsc::channel();
        let old_thread = thread::spawn(move || {
            assert!(wait.recv().is_ok());
            assert!(finished.send(()).is_ok());
        });
        install_fake_running(&controller, 1, old_thread);
        controller.stop_nonblocking();
        assert_eq!(controller.phase(), ServerPhase::Stopping);

        assert!(release.send(()).is_ok());
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        for _ in 0..100 {
            if controller.phase() == ServerPhase::Stopped {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        install_fake_running(&controller, 2, thread::spawn(|| {}));
        finish_generation_exit(&old_inner, 1);
        assert_eq!(controller.connection().unwrap().generation, 2);
        controller.stop();
    }

    #[test]
    fn stop_during_startup_cancels_only_detached_generation() {
        let controller = EmbeddedServerController::default();
        let StartReservation::New {
            generation: old,
            cancellation,
        } = controller.reserve_start(true)
        else {
            panic!("initial reservation");
        };
        controller.stop_nonblocking();
        assert!(cancellation.load(Ordering::Acquire));
        assert_eq!(controller.phase(), ServerPhase::Stopping);
        controller.finish_failed_start(old);
        assert_eq!(controller.phase(), ServerPhase::Stopped);
    }

    #[test]
    fn unexpected_exit_is_published_only_after_the_exact_thread_joins() {
        let controller = EmbeddedServerController::default();
        let reaper = controller.inner.reaper.clone();
        let (exited, exited_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            reaper
                .send(ReaperCommand::Exited {
                    generation: 1,
                    succeeded: false,
                })
                .unwrap();
            exited.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        install_fake_running(&controller, 1, thread);

        exited_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        for _ in 0..100 {
            if controller.phase() == ServerPhase::Stopping {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(controller.phase(), ServerPhase::Stopping);
        release.send(()).unwrap();
        for _ in 0..100 {
            if controller.phase() == ServerPhase::Failed {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(controller.phase(), ServerPhase::Failed);
    }

    #[test]
    fn dropping_final_controller_stops_and_joins_running_generation() {
        let controller = EmbeddedServerController::default();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let (finished, finished_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                drop(shutdown_rx.await);
            });
            finished.send(()).unwrap();
        });
        *controller.inner.state.lock().unwrap() = ControllerState::Running {
            connection: ServerConnection {
                generation: 1,
                endpoint: "http://127.0.0.1:1".to_owned(),
                bearer: "bearer-1".to_owned(),
                first_catalog_download: false,
            },
            shutdown: Some(shutdown),
            thread: Some(thread),
        };

        drop(controller);

        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }
}
