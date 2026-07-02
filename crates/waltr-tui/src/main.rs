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
    let runtime_options = runtime_options(&options).await?;

    let mut terminal = ratatui::init();
    let mut terminal_modes = TerminalModeGuard::new();
    terminal_modes.enable_mouse_capture()?;
    terminal_modes.enable_enhanced_keyboard();
    let result = runtime::run_with_options(&mut terminal, runtime_options).await;
    let restore_result = terminal_modes.restore();

    result.and(restore_result)
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
            if let Err(restore_error) = execute!(io::stdout(), PopKeyboardEnhancementFlags) {
                if error.is_none() {
                    error = Some(restore_error);
                }
            }
            self.enhanced_keyboard = false;
        }

        if self.mouse_capture {
            if let Err(restore_error) = execute!(io::stdout(), DisableMouseCapture) {
                if error.is_none() {
                    error = Some(restore_error);
                }
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

async fn runtime_options(options: &CliOptions) -> io::Result<runtime::RuntimeOptions> {
    if !options.server {
        return Ok(runtime::RuntimeOptions::default());
    }

    #[cfg(feature = "server")]
    {
        let addr = start_embedded_server(options.server_addr).await?;
        return Ok(runtime::RuntimeOptions {
            client_endpoint: Some(format!("http://{}", client_addr_for_server(addr))),
        });
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
async fn start_embedded_server(addr: SocketAddr) -> io::Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;

    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, waltr_server::build_app()).await {
            tracing::error!(%error, "embedded server stopped");
        }
    });

    Ok(addr)
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
fn client_addr_for_server(addr: SocketAddr) -> SocketAddr {
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
