//! Waltr TUI entry point.

use std::{env, io, net::SocketAddr};

use waltr_tui::runtime;
use ratatui::crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
};
#[cfg(feature = "server")]
use tokio::sync::oneshot;

#[cfg(feature = "web")]
const DEFAULT_EMBEDDED_SERVER_ADDR: &str = "127.0.0.1:14333";
#[cfg(not(feature = "web"))]
const DEFAULT_EMBEDDED_SERVER_ADDR: &str = "127.0.0.1:14334";
const SERVER_ADDR_ENV: &str = "WALTR_SERVER_ADDR";

#[derive(Debug)]
struct CliOptions {
    server: bool,
    server_addr: SocketAddr,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let options = CliOptions::parse(env::args().skip(1))?;
    let startup = runtime_startup(&options).await?;

    let mut terminal = ratatui::init();
    let mut terminal_modes = TerminalModeGuard::new();
    let result = match terminal_modes.enable_mouse_capture() {
        Ok(()) => {
            terminal_modes.enable_enhanced_keyboard();
            runtime::run_with_options(&mut terminal, startup.runtime_options).await
        }
        Err(error) => Err(error),
    };
    let restore_result = terminal_modes.restore();

    finish_runtime(result.and(restore_result), startup.embedded_server).await
}

fn enable_enhanced_keyboard() -> io::Result<()> {
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        )
    )
}

#[derive(Debug, Default)]
struct TerminalModeGuard {
    mouse_capture: bool,
    enhanced_keyboard: bool,
    restored: bool,
}

impl TerminalModeGuard {
    const fn new() -> Self {
        Self {
            mouse_capture: false,
            enhanced_keyboard: false,
            restored: false,
        }
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnableMouseCapture)?;
        self.mouse_capture = true;
        Ok(())
    }

    fn enable_enhanced_keyboard(&mut self) {
        self.enhanced_keyboard = enable_enhanced_keyboard().is_ok();
    }

    fn restore(&mut self) -> io::Result<()> {
        let mut error = None;

        if self.enhanced_keyboard {
            if let Err(restore_error) = execute!(io::stdout(), PopKeyboardEnhancementFlags)
                && error.is_none()
            {
                error = Some(restore_error);
            }
            self.enhanced_keyboard = false;
        }

        if self.mouse_capture {
            if let Err(restore_error) = execute!(io::stdout(), DisableMouseCapture)
                && error.is_none()
            {
                error = Some(restore_error);
            }
            self.mouse_capture = false;
        }

        ratatui::restore();
        self.restored = true;

        error.map_or(Ok(()), Err)
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        if !self.restored {
            drop(self.restore());
        }
    }
}

impl CliOptions {
    fn parse(args: impl IntoIterator<Item = String>) -> io::Result<Self> {
        let mut server = cfg!(feature = "default-server");
        let mut server_addr = embedded_server_addr()?;
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--server" => server = true,
                "--no-server" => server = false,
                "--server-addr" => {
                    let value = args.next().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--server-addr requires an address",
                        )
                    })?;
                    server_addr = parse_addr(&value)?;
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown argument: {arg}"),
                    ));
                }
            }
        }

        Ok(Self {
            server,
            server_addr,
        })
    }
}

struct RuntimeStartup {
    runtime_options: runtime::RuntimeOptions,
    #[cfg(feature = "server")]
    embedded_server: Option<EmbeddedServer>,
    #[cfg(not(feature = "server"))]
    embedded_server: Option<()>,
}

#[allow(clippy::unused_async)]
async fn runtime_startup(options: &CliOptions) -> io::Result<RuntimeStartup> {
    if !options.server {
        return Ok(RuntimeStartup {
            runtime_options: runtime::RuntimeOptions::default(),
            embedded_server: None,
        });
    }

    #[cfg(feature = "server")]
    {
        let embedded_server = start_embedded_server(options.server_addr).await?;
        Ok(RuntimeStartup {
            runtime_options: runtime::RuntimeOptions {
                client_endpoint: Some(format!(
                    "http://{}",
                    client_addr_for_server(embedded_server.addr)
                )),
            },
            embedded_server: Some(embedded_server),
        })
    }

    #[cfg(not(feature = "server"))]
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "--server requires building waltr with the server feature (requested {})",
                options.server_addr
            ),
        ))
    }
}

#[cfg(feature = "server")]
struct EmbeddedServer {
    addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<io::Result<()>>,
}

#[cfg(feature = "server")]
impl EmbeddedServer {
    async fn shutdown(self) -> io::Result<()> {
        let _ = self.shutdown.send(());
        match self.task.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(io::Error::other("embedded server shutdown failed")),
            Err(_) => Err(io::Error::other("embedded server task failed")),
        }
    }
}

#[cfg(feature = "server")]
async fn start_embedded_server(addr: SocketAddr) -> io::Result<EmbeddedServer> {
    let app = waltr_server::build_app(Default::default())
        .await
        .map_err(|error| {
            io::Error::other(format!("embedded server initialization failed: {error}"))
        })?;
    start_embedded_server_with_app(app, addr).await
}

#[cfg(feature = "server")]
async fn start_embedded_server_with_app(
    app: waltr_server::ServerApplication,
    addr: SocketAddr,
) -> io::Result<EmbeddedServer> {
    #[cfg(feature = "web")]
    let (app, listener, addr) = app.bind_with_apps(addr).await?;
    #[cfg(not(feature = "web"))]
    let (app, listener, addr) = app.bind(addr).await?;
    let (shutdown, shutdown_receiver) = oneshot::channel();

    let task = tokio::spawn(async move {
        app.serve_with_shutdown(listener, async move {
            let _ = shutdown_receiver.await;
        })
        .await
    });

    Ok(EmbeddedServer {
        addr,
        shutdown,
        task,
    })
}

#[cfg(feature = "server")]
async fn finish_runtime(
    result: io::Result<()>,
    embedded_server: Option<EmbeddedServer>,
) -> io::Result<()> {
    let shutdown_result = match embedded_server {
        Some(server) => server.shutdown().await,
        None => Ok(()),
    };
    if shutdown_result.is_err() {
        tracing::error!("embedded server shutdown failed");
    }
    result.and(shutdown_result)
}

#[cfg(not(feature = "server"))]
#[allow(clippy::unused_async)]
async fn finish_runtime(result: io::Result<()>, _embedded_server: Option<()>) -> io::Result<()> {
    result
}

fn parse_addr(value: &str) -> io::Result<SocketAddr> {
    value.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid server address {value:?}: {error}"),
        )
    })
}

fn embedded_server_addr() -> io::Result<SocketAddr> {
    match env::var(SERVER_ADDR_ENV) {
        Ok(value) => parse_addr(&value),
        Err(env::VarError::NotPresent) => parse_addr(DEFAULT_EMBEDDED_SERVER_ADDR),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {SERVER_ADDR_ENV}: {error}"),
        )),
    }
}

#[cfg(feature = "server")]
const fn client_addr_for_server(addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() {
        SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            addr.port(),
        )
    } else {
        addr
    }
}

fn print_help() {
    println!(
        "Usage: waltr [--server] [--no-server] [--server-addr <addr>]\n\nOptions:\n  --server              Start the bundled server before the TUI\n  --no-server           Do not start the bundled server\n  --server-addr <addr>  Address for the bundled server [default: {DEFAULT_EMBEDDED_SERVER_ADDR}]\n  -h, --help            Show this help"
    );
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use tokio::sync::Notify;

    use waltr_server::test_support::blocked_background_application;

    use super::*;

    struct CleanupDropGuard(Arc<AtomicBool>);

    impl Drop for CleanupDropGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[allow(
        clippy::type_complexity,
        reason = "fixture returns each independent shutdown synchronization point"
    )]
    fn blocked_server() -> (
        EmbeddedServer,
        Arc<Notify>,
        Arc<AtomicBool>,
        Arc<Notify>,
        Arc<Notify>,
    ) {
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let cleanup_started = Arc::new(Notify::new());
        let cleanup_dropped = Arc::new(AtomicBool::new(false));
        let shutdown_observed = Arc::new(Notify::new());
        let release_server = Arc::new(Notify::new());
        let task = {
            let cleanup_started = Arc::clone(&cleanup_started);
            let cleanup_dropped = Arc::clone(&cleanup_dropped);
            let shutdown_observed = Arc::clone(&shutdown_observed);
            let release_server = Arc::clone(&release_server);
            tokio::spawn(async move {
                let cleanup = tokio::spawn(async move {
                    let _guard = CleanupDropGuard(cleanup_dropped);
                    cleanup_started.notify_one();
                    std::future::pending::<()>().await;
                });
                let _ = shutdown_receiver.await;
                cleanup.abort();
                drop(cleanup.await);
                shutdown_observed.notify_one();
                release_server.notified().await;
                Ok(())
            })
        };
        (
            EmbeddedServer {
                addr: "127.0.0.1:0".parse().expect("test address should parse"),
                shutdown,
                task,
            },
            cleanup_started,
            cleanup_dropped,
            shutdown_observed,
            release_server,
        )
    }

    #[tokio::test]
    async fn runtime_error_cancels_blocked_cleanup_and_joins_server() {
        let (server, cleanup_started, cleanup_dropped, shutdown_observed, release_server) =
            blocked_server();
        cleanup_started.notified().await;
        let completion = tokio::spawn(finish_runtime(
            Err(io::Error::other("TUI failed")),
            Some(server),
        ));

        shutdown_observed.notified().await;
        assert!(cleanup_dropped.load(Ordering::SeqCst));
        assert!(!completion.is_finished());
        release_server.notify_one();

        let error = completion
            .await
            .expect("runtime completion should join")
            .expect_err("the TUI error should be preserved");
        assert_eq!(error.to_string(), "TUI failed");
    }

    #[tokio::test]
    async fn runtime_success_signals_and_joins_server() {
        let (server, cleanup_started, cleanup_dropped, shutdown_observed, release_server) =
            blocked_server();
        cleanup_started.notified().await;
        let completion = tokio::spawn(finish_runtime(Ok(()), Some(server)));

        shutdown_observed.notified().await;
        assert!(cleanup_dropped.load(Ordering::SeqCst));
        assert!(!completion.is_finished());
        release_server.notify_one();

        completion
            .await
            .expect("runtime completion should join")
            .expect("successful TUI and server shutdown should succeed");
    }

    #[tokio::test]
    async fn runtime_error_is_preserved_when_server_shutdown_fails() {
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let server = EmbeddedServer {
            addr: "127.0.0.1:0".parse().expect("test address should parse"),
            shutdown,
            task: tokio::spawn(async move {
                let _ = shutdown_receiver.await;
                Err(io::Error::other("sensitive server detail"))
            }),
        };

        let error = finish_runtime(Err(io::Error::other("TUI failed")), Some(server))
            .await
            .expect_err("the TUI error should be preserved");

        assert_eq!(error.to_string(), "TUI failed");
    }

    #[tokio::test]
    async fn server_shutdown_error_is_redacted_when_runtime_succeeds() {
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let server = EmbeddedServer {
            addr: "127.0.0.1:0".parse().expect("test address should parse"),
            shutdown,
            task: tokio::spawn(async move {
                let _ = shutdown_receiver.await;
                Err(io::Error::other("sensitive server detail"))
            }),
        };

        let error = finish_runtime(Ok(()), Some(server))
            .await
            .expect_err("server shutdown should fail");

        assert_eq!(error.to_string(), "embedded server shutdown failed");
    }

    #[tokio::test]
    async fn occupied_port_cancels_and_joins_cleanup_before_startup_error() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = occupied.local_addr().unwrap();
        let (app, probe) = blocked_background_application();
        probe.cleanup_started.notified().await;

        let startup = tokio::spawn(start_embedded_server_with_app(app, addr));
        probe.shutdown_observed.notified().await;
        assert!(probe.cleanup_dropped.load(Ordering::SeqCst));
        assert!(!startup.is_finished());
        probe.release_cleanup.notify_one();

        let Err(error) = startup.await.expect("startup should join") else {
            panic!("occupied port should fail startup");
        };
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }
}
