use crate::support::{
    grpc::{config_client_for_addr, config_with_provider, get_config},
    server::{temp_root, wait_for_health},
};
use waltr::grpc::waltr::config::v1::{PatchConfigRequest, ProviderMapPatch};
use sqlx::Connection as _;
use std::{
    fs,
    io::{self, Read as _},
    net::TcpListener,
    process::{Child, Command, ExitStatus, Stdio},
    time::Duration,
};

const OIDC_ENVIRONMENT: &[&str] = &[
    "WALTR_AUTH_MODE",
    "WALTR_OIDC_ISSUER",
    "WALTR_OIDC_CLIENT_ID",
    "WALTR_OIDC_CLIENT_SECRET",
    "WALTR_OIDC_REDIRECT_URI",
    "WALTR_PUBLIC_URL",
    "WALTR_API_OIDC_ISSUER",
    "WALTR_API_OIDC_AUDIENCE",
    "WALTR_API_REQUIRED_SCOPES",
    "WALTR_OIDC_AUDIENCE",
    "WALTR_OIDC_SCOPES",
    "WALTR_MCP_REQUIRED_SCOPES",
    "WALTR_MCP_OAUTH_ISSUER",
    "WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE",
    "WALTR_MCP_OAUTH_ACCESS_TOKEN_TTL",
    "WALTR_MCP_OAUTH_REFRESH_TOKEN_TTL",
    "WALTR_MCP_OAUTH_REFRESH_FAMILY_TTL",
    "WALTR_MCP_OAUTH_CODE_TTL",
    "WALTR_MCP_OAUTH_ALLOW_DCR",
    "WALTR_MCP_OAUTH_ALLOW_CIMD",
    "WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS",
    "WALTR_AUTH_SESSION_TTL",
    "WALTR_AUTH_ALLOW_INSECURE_COOKIE",
    "WALTR_OIDC_ALLOW_INSECURE_HTTP",
    "WALTR_TRUSTED_PROXIES",
];

#[tokio::test]
async fn server_process_boots_and_serves_health() -> Result<(), Box<dyn std::error::Error>> {
    let (addr, root, database_url) = process_test_inputs()?;
    let waltr_database = root.join("waltr.sqlite3");
    let mcp_database = root.join("mcp.sqlite3");
    let mut server = isolated_server_command(addr, &root, &database_url)
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
    assert_process_database_isolation(&waltr_database, &mcp_database).await?;
    fs::remove_dir_all(root)?;
    result
}

#[cfg(unix)]
#[tokio::test]
async fn server_process_gracefully_shuts_down_on_sigterm() -> Result<(), Box<dyn std::error::Error>>
{
    let (addr, root, database_url) = process_test_inputs()?;
    let mut server = isolated_server_command(addr, &root, &database_url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    wait_for_health(addr).await?;
    let signal = Command::new("kill")
        .args(["-TERM", &server.id().to_string()])
        .status()?;
    assert!(signal.success(), "failed to send SIGTERM to server");
    let status = wait_for_exit(&mut server, Duration::from_secs(5)).await?;

    assert!(
        status.success(),
        "SIGTERM did not produce a clean exit: {status}"
    );
    assert_listener_unused(addr).await?;
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn server_process_fails_before_bind_for_unusable_database_path()
-> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    let root = temp_root()?;
    let database_path = root.join("invalid-%FF.sqlite3");
    let database_url = format!("sqlite://{}", database_path.display());
    let mut server = isolated_server_command(addr, &root, &database_url).spawn()?;

    let status = wait_for_exit(&mut server, Duration::from_secs(5)).await?;
    let output = captured_output(&mut server)?;
    assert!(!status.success(), "server unexpectedly started: {output}");
    assert_listener_unused(addr).await?;
    assert!(!output.contains(&database_url));
    assert!(!output.contains(&database_path.to_string_lossy().into_owned()));

    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn server_process_fails_closed_for_missing_oidc_configuration()
-> Result<(), Box<dyn std::error::Error>> {
    let (addr, root, database_url) = process_test_inputs()?;
    let mut command = isolated_server_command(addr, &root, &database_url);
    command.env("WALTR_AUTH_MODE", "oidc");
    let mut server = command.spawn()?;

    let status = wait_for_exit(&mut server, Duration::from_secs(5)).await?;
    let output = captured_output(&mut server)?;
    assert!(!status.success(), "server unexpectedly started: {output}");
    assert!(output.contains("access authentication configuration is invalid"));
    assert!(!output.contains("WALTR_OIDC_ISSUER"));
    assert!(!output.contains("WALTR_OIDC_CLIENT_SECRET"));
    assert_listener_unused(addr).await?;

    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn server_process_fails_closed_for_unreachable_oidc_discovery()
-> Result<(), Box<dyn std::error::Error>> {
    let (addr, root, database_url) = process_test_inputs()?;
    let issuer_listener = TcpListener::bind("127.0.0.1:0")?;
    let issuer = format!("http://{}", issuer_listener.local_addr()?);
    drop(issuer_listener);
    let secret = "must-not-appear-in-startup-output";
    let mut command = isolated_server_command(addr, &root, &database_url);
    command
        .env("WALTR_AUTH_MODE", "oidc")
        .env("WALTR_OIDC_ISSUER", &issuer)
        .env("WALTR_OIDC_CLIENT_ID", "waltr-process-test")
        .env("WALTR_OIDC_CLIENT_SECRET", secret)
        .env(
            "WALTR_OIDC_REDIRECT_URI",
            format!("http://{addr}/oidc/callback"),
        )
        .env("WALTR_PUBLIC_URL", "https://waltr.example")
        .env("WALTR_MCP_REQUIRED_SCOPES", "mcp:use")
        .env(
            "WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE",
            root.join("unused-wrapping-keys.json"),
        )
        .env("WALTR_OIDC_AUDIENCE", "waltr-api")
        .env("WALTR_OIDC_SCOPES", "openid")
        .env("WALTR_AUTH_ALLOW_INSECURE_COOKIE", "true")
        .env("WALTR_OIDC_ALLOW_INSECURE_HTTP", "true");
    let mut server = command.spawn()?;

    let status = wait_for_exit(&mut server, Duration::from_secs(5)).await?;
    let output = captured_output(&mut server)?;
    assert!(!status.success(), "server unexpectedly started: {output}");
    assert!(output.contains("failed to initialize OIDC provider discovery"));
    assert!(!output.contains(&issuer));
    assert!(!output.contains(secret));
    assert_listener_unused(addr).await?;

    fs::remove_dir_all(root)?;
    Ok(())
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

fn process_test_inputs() -> io::Result<(std::net::SocketAddr, std::path::PathBuf, String)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    let root = temp_root().map_err(|error| io::Error::other(error.to_string()))?;
    let database_url = format!("sqlite://{}", root.join("waltr.sqlite3").display());
    Ok((addr, root, database_url))
}

fn isolated_server_command(
    addr: std::net::SocketAddr,
    root: &std::path::Path,
    database_url: &str,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_waltr-server"));
    command
        .env("WALTR_SERVER_ADDR", addr.to_string())
        .env("WALTR_DATABASE_URL", database_url)
        .env_remove("WALTR_MCP_DATABASE_URL")
        .env("WALTR_SESSION_STORE", "memory")
        .env_remove("WALTR_OTEL_EXPORT")
        .env_remove("WALTR_PYROSCOPE_URL")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in OIDC_ENVIRONMENT {
        command.env_remove(name);
    }
    command
}

async fn wait_for_exit(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if tokio::time::Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "server did not exit after startup initialization failed",
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn captured_output(server: &mut Child) -> io::Result<String> {
    let mut output = String::new();
    server
        .stdout
        .take()
        .expect("captured server stdout")
        .read_to_string(&mut output)?;
    server
        .stderr
        .take()
        .expect("captured server stderr")
        .read_to_string(&mut output)?;
    Ok(output)
}

async fn assert_listener_unused(addr: std::net::SocketAddr) -> io::Result<()> {
    let health = reqwest::Client::builder()
        .timeout(Duration::from_millis(250))
        .build()
        .map_err(io::Error::other)?
        .get(format!("http://{addr}/health"))
        .send()
        .await;
    if health.is_ok() {
        return Err(io::Error::other("failed startup became healthy"));
    }
    drop(TcpListener::bind(addr).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed startup retained its listen address: {error}"),
        )
    })?);
    Ok(())
}

async fn assert_process_database_isolation(
    waltr_database: &std::path::Path,
    mcp_database: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    assert!(waltr_database.is_file());
    assert!(mcp_database.is_file());
    let mut waltr =
        sqlx::SqliteConnection::connect(&format!("sqlite://{}", waltr_database.display())).await?;
    let mut mcp =
        sqlx::SqliteConnection::connect(&format!("sqlite://{}", mcp_database.display())).await?;
    let waltr_tables = sqlx::query_scalar::<_, String>(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(&mut waltr)
    .await?;
    let mcp_tables = sqlx::query_scalar::<_, String>(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(&mut mcp)
    .await?;
    assert_eq!(
        waltr_tables,
        [
            "_sqlx_migrations",
            "waltr_browser_sessions",
            "waltr_mcp_oauth_login_continuations",
            "waltr_oidc_login_attempts"
        ]
    );
    assert_eq!(
        mcp_tables,
        [
            "_sqlx_migrations",
            "mcp_oauth_access_tokens",
            "mcp_oauth_authorization_codes",
            "mcp_oauth_authorization_transactions",
            "mcp_oauth_client_metadata_cache",
            "mcp_oauth_client_redirect_uris",
            "mcp_oauth_clients",
            "mcp_oauth_consents",
            "mcp_oauth_dynamic_clients",
            "mcp_oauth_grants",
            "mcp_oauth_refresh_families",
            "mcp_oauth_refresh_tokens",
            "mcp_oauth_registration_rates",
            "mcp_oauth_revoked_clients",
            "mcp_oauth_signing_keys",
            "mcp_streamable_http_events",
            "mcp_streamable_http_sessions"
        ]
    );
    Ok(())
}
