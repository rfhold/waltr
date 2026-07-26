//! Server-owned database configuration and initialization.

mod migrations;
mod outbound_oauth;

pub(crate) use outbound_oauth::{DurableAttemptRecord, OutboundOAuthStore};

use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    fs::{File, OpenOptions},
    path::{Component, Path, PathBuf},
    str::FromStr as _,
    sync::Arc,
    time::Duration,
};

use mcp::server::{
    PostgresStreamableHttpSessionStore, SqliteStreamableHttpSessionStore,
    StreamableHttpSessionStore, StreamableHttpSessionStoreConfig,
};
use sqlx::{
    PgPool, SqlitePool,
    postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

const ENV_DATABASE_URL: &str = "WALTR_DATABASE_URL";
const ENV_MCP_DATABASE_URL: &str = "WALTR_MCP_DATABASE_URL";
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// Database backend selected for server-owned persistence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatabaseBackend {
    /// A local or in-memory `SQLite` database.
    Sqlite,
    /// A `PostgreSQL` database.
    PostgreSql,
}

impl fmt::Display for DatabaseBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite => formatter.write_str("SQLite"),
            Self::PostgreSql => formatter.write_str("PostgreSQL"),
        }
    }
}

/// Validated Waltr and MCP database configuration.
#[derive(Clone)]
pub struct DatabaseConfig {
    waltr: DatabaseTarget,
    mcp: DatabaseTarget,
}

#[derive(Clone)]
enum DatabaseTarget {
    Sqlite {
        options: SqliteConnectOptions,
        path: Option<PathBuf>,
        in_memory: bool,
        identity: SqliteIdentity,
    },
    PostgreSql {
        options: PgConnectOptions,
    },
}

#[derive(Clone)]
enum SqliteIdentity {
    File(PathBuf),
    PrivateMemory,
    SharedMemory(PathBuf),
}

impl DatabaseTarget {
    const fn backend(&self) -> DatabaseBackend {
        match self {
            Self::Sqlite { .. } => DatabaseBackend::Sqlite,
            Self::PostgreSql { .. } => DatabaseBackend::PostgreSql,
        }
    }

    fn sqlite_path(&self) -> Option<&Path> {
        match self {
            Self::Sqlite { path, .. } => path.as_deref(),
            Self::PostgreSql { .. } => None,
        }
    }
}

impl fmt::Debug for DatabaseConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseConfig")
            .field("backend", &self.backend())
            .field("mcp_backend", &self.mcp_backend())
            .finish_non_exhaustive()
    }
}

impl DatabaseConfig {
    /// Read `WALTR_DATABASE_URL` and optional `WALTR_MCP_DATABASE_URL`, defaulting to durable
    /// storage under `$XDG_DATA_HOME/waltr` or `$HOME/.local/share/waltr`.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid Unicode, unsupported schemes, malformed URLs, colliding
    /// `SQLite` files, or an unavailable default application data path.
    pub fn from_env() -> Result<Self, PersistenceError> {
        let waltr_url = read_optional_env(ENV_DATABASE_URL)?;
        let mcp_url = read_optional_env(ENV_MCP_DATABASE_URL)?;
        if let Some(waltr_url) = waltr_url {
            Self::from_optional_urls(&waltr_url, mcp_url.as_deref())
        } else {
            let data_directory = default_data_directory(
                env::var_os("XDG_DATA_HOME").as_deref(),
                env::var_os("HOME").as_deref(),
            )?;
            match mcp_url {
                Some(mcp_url) => Self::new(
                    sqlite_file_target(data_directory.join("waltr.sqlite3")),
                    parse_target(&mcp_url, ENV_MCP_DATABASE_URL)?,
                ),
                None => Self::default_in(&data_directory),
            }
        }
    }

    /// Parse a Waltr database URL and derive a separate MCP target.
    ///
    /// File-backed `SQLite` derives `mcp.sqlite3` in the same directory. `PostgreSQL` reuses the
    /// same database target through a separate pool. In-memory `SQLite` creates separate pools.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported schemes, malformed URLs, or a derived `SQLite` collision.
    pub fn from_url(database_url: &str) -> Result<Self, PersistenceError> {
        Self::from_optional_urls(database_url, None)
    }

    /// Parse explicit, independently selectable Waltr and MCP database URLs.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported schemes, malformed URLs, or colliding `SQLite` files.
    pub fn from_urls(
        waltr_database_url: &str,
        mcp_database_url: &str,
    ) -> Result<Self, PersistenceError> {
        Self::from_optional_urls(waltr_database_url, Some(mcp_database_url))
    }

    /// Create two independent retained one-connection in-memory `SQLite` targets for tests.
    ///
    /// # Errors
    ///
    /// Returns an error if the built-in `SQLite` URL cannot be parsed.
    pub fn for_tests() -> Result<Self, PersistenceError> {
        Self::from_urls("sqlite::memory:", "sqlite::memory:")
    }

    /// Return the selected Waltr backend without exposing connection credentials.
    #[must_use]
    pub const fn backend(&self) -> DatabaseBackend {
        self.waltr.backend()
    }

    /// Return the selected MCP backend without exposing connection credentials.
    #[must_use]
    pub const fn mcp_backend(&self) -> DatabaseBackend {
        self.mcp.backend()
    }

    /// Return the Waltr `SQLite` file path, or `None` for `PostgreSQL` and in-memory `SQLite`.
    #[must_use]
    pub fn sqlite_path(&self) -> Option<&Path> {
        self.waltr.sqlite_path()
    }

    /// Return the MCP `SQLite` file path, or `None` for `PostgreSQL` and in-memory `SQLite`.
    #[must_use]
    pub fn mcp_sqlite_path(&self) -> Option<&Path> {
        self.mcp.sqlite_path()
    }

    fn from_optional_urls(waltr_url: &str, mcp_url: Option<&str>) -> Result<Self, PersistenceError> {
        let waltr = parse_target(waltr_url, ENV_DATABASE_URL)?;
        let mcp = match mcp_url {
            Some(mcp_url) => parse_target(mcp_url, ENV_MCP_DATABASE_URL)?,
            None => derive_mcp_target(&waltr)?,
        };
        Self::new(waltr, mcp)
    }

    fn new(waltr: DatabaseTarget, mcp: DatabaseTarget) -> Result<Self, PersistenceError> {
        if let (
            DatabaseTarget::Sqlite { identity: waltr, .. },
            DatabaseTarget::Sqlite { identity: mcp, .. },
        ) = (&waltr, &mcp)
        {
            match (waltr, mcp) {
                (SqliteIdentity::File(waltr), SqliteIdentity::File(mcp))
                    if normalized_absolute_path(waltr)? == normalized_absolute_path(mcp)? =>
                {
                    return Err(PersistenceError::Configuration(
                        "WALTR_DATABASE_URL and WALTR_MCP_DATABASE_URL must not use the same SQLite file",
                    ));
                }
                (SqliteIdentity::SharedMemory(waltr), SqliteIdentity::SharedMemory(mcp))
                    if waltr == mcp =>
                {
                    return Err(PersistenceError::Configuration(
                        "WALTR_DATABASE_URL and WALTR_MCP_DATABASE_URL must not use the same named shared in-memory SQLite database",
                    ));
                }
                _ => {}
            }
        }
        Ok(Self { waltr, mcp })
    }

    fn default_in(data_directory: &Path) -> Result<Self, PersistenceError> {
        Self::new(
            sqlite_file_target(data_directory.join("waltr.sqlite3")),
            sqlite_file_target(data_directory.join("mcp.sqlite3")),
        )
    }
}

/// An initialized, migrated server persistence backend.
#[derive(Clone)]
pub struct Persistence {
    pool: PersistencePool,
    mcp_pool: PersistencePool,
    mcp_session_store: Arc<dyn StreamableHttpSessionStore>,
}

#[derive(Clone)]
enum PersistencePool {
    Sqlite(SqlitePool),
    PostgreSql(PgPool),
}

impl PersistencePool {
    const fn backend(&self) -> DatabaseBackend {
        match self {
            Self::Sqlite(_) => DatabaseBackend::Sqlite,
            Self::PostgreSql(_) => DatabaseBackend::PostgreSql,
        }
    }

    async fn close(&self) {
        match self {
            Self::Sqlite(pool) => pool.close().await,
            Self::PostgreSql(pool) => pool.close().await,
        }
    }
}

impl fmt::Debug for Persistence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Persistence")
            .field("backend", &self.backend())
            .field("mcp_backend", &self.mcp_backend())
            .finish_non_exhaustive()
    }
}

impl Persistence {
    /// Initialize independent Waltr and MCP pools and run their owned migrations.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error if directories cannot be created, a database is unavailable, or
    /// migration history is invalid.
    pub async fn initialize(config: DatabaseConfig) -> Result<Self, PersistenceError> {
        tracing::info!(
            backend = %config.backend(),
            mcp_backend = %config.mcp_backend(),
            "initializing server persistence"
        );
        create_sqlite_parent(&config.waltr).await?;
        create_sqlite_parent(&config.mcp).await?;
        // Startup validates static targets. Database directory entries must be protected from
        // concurrent same-UID replacement by deployment filesystem ownership and permissions.
        validate_sqlite_file_identities(&config)?;

        let pool = initialize_waltr_pool(config.waltr).await?;
        match initialize_mcp_pool(config.mcp).await {
            Ok((mcp_pool, mcp_session_store)) => Ok(Self {
                pool,
                mcp_pool,
                mcp_session_store,
            }),
            Err(error) => {
                pool.close().await;
                Err(error)
            }
        }
    }

    /// Initialize persistence directly from environment configuration or durable defaults.
    ///
    /// # Errors
    ///
    /// Returns configuration, connection, directory, or migration errors from initialization.
    pub async fn from_env() -> Result<Self, PersistenceError> {
        Self::initialize(DatabaseConfig::from_env()?).await
    }

    /// Return the initialized Waltr backend.
    #[must_use]
    pub const fn backend(&self) -> DatabaseBackend {
        self.pool.backend()
    }

    /// Return the initialized MCP backend.
    #[must_use]
    pub const fn mcp_backend(&self) -> DatabaseBackend {
        self.mcp_pool.backend()
    }

    /// Borrow the Waltr `SQLite` pool when `SQLite` is selected.
    #[must_use]
    pub const fn sqlite_pool(&self) -> Option<&SqlitePool> {
        match &self.pool {
            PersistencePool::Sqlite(pool) => Some(pool),
            PersistencePool::PostgreSql(_) => None,
        }
    }

    /// Borrow the Waltr `PostgreSQL` pool when `PostgreSQL` is selected.
    #[must_use]
    pub const fn postgres_pool(&self) -> Option<&PgPool> {
        match &self.pool {
            PersistencePool::PostgreSql(pool) => Some(pool),
            PersistencePool::Sqlite(_) => None,
        }
    }

    /// Borrow the MCP `SQLite` pool when `SQLite` is selected.
    #[must_use]
    pub const fn mcp_sqlite_pool(&self) -> Option<&SqlitePool> {
        match &self.mcp_pool {
            PersistencePool::Sqlite(pool) => Some(pool),
            PersistencePool::PostgreSql(_) => None,
        }
    }

    /// Borrow the MCP `PostgreSQL` pool when `PostgreSQL` is selected.
    #[must_use]
    pub const fn mcp_postgres_pool(&self) -> Option<&PgPool> {
        match &self.mcp_pool {
            PersistencePool::PostgreSql(pool) => Some(pool),
            PersistencePool::Sqlite(_) => None,
        }
    }

    /// Clone the initialized MCP Streamable HTTP session store.
    #[must_use]
    pub fn mcp_session_store(&self) -> Arc<dyn StreamableHttpSessionStore> {
        Arc::clone(&self.mcp_session_store)
    }

    /// Check that both initialized database pools can serve queries.
    ///
    /// # Errors
    ///
    /// Returns a sanitized backend error when either pool is unavailable.
    pub async fn ready(&self) -> Result<(), PersistenceError> {
        check_pool(&self.pool).await?;
        check_pool(&self.mcp_pool).await
    }

    pub(crate) fn outbound_oauth_store(&self) -> Arc<OutboundOAuthStore> {
        Arc::new(OutboundOAuthStore::new(self.pool.clone()))
    }
}

async fn check_pool(pool: &PersistencePool) -> Result<(), PersistenceError> {
    match pool {
        PersistencePool::Sqlite(pool) => sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(pool)
            .await
            .map(|_| ())
            .map_err(|_| PersistenceError::Connection(DatabaseBackend::Sqlite)),
        PersistencePool::PostgreSql(pool) => sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(pool)
            .await
            .map(|_| ())
            .map_err(|_| PersistenceError::Connection(DatabaseBackend::PostgreSql)),
    }
}

/// Failure to configure or initialize server persistence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistenceError {
    /// Database configuration is invalid.
    Configuration(&'static str),
    /// A required local data directory could not be created.
    Directory(&'static str),
    /// The selected database could not be reached.
    Connection(DatabaseBackend),
    /// A Waltr migration could not be validated or applied.
    Migration(DatabaseBackend),
    /// The MCP session store could not initialize or migrate.
    McpSessionStore(DatabaseBackend),
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message) | Self::Directory(message) => formatter.write_str(message),
            Self::Connection(backend) => write!(
                formatter,
                "failed to connect to the configured {backend} database; check database configuration and availability"
            ),
            Self::Migration(backend) => write!(
                formatter,
                "failed to validate or apply Waltr migrations to the {backend} database; check database permissions and migration compatibility"
            ),
            Self::McpSessionStore(backend) => write!(
                formatter,
                "failed to initialize MCP session persistence in the {backend} database; check database permissions and migration compatibility"
            ),
        }
    }
}

impl Error for PersistenceError {}

async fn initialize_waltr_pool(target: DatabaseTarget) -> Result<PersistencePool, PersistenceError> {
    match target {
        DatabaseTarget::Sqlite {
            options, in_memory, ..
        } => {
            let pool = sqlite_pool(options, in_memory).await?;
            if let Err(error) = migrations::sqlite(&pool).await {
                pool.close().await;
                return Err(error);
            }
            Ok(PersistencePool::Sqlite(pool))
        }
        DatabaseTarget::PostgreSql { options } => {
            let pool = PgPoolOptions::new()
                .connect_with(options)
                .await
                .map_err(|_| PersistenceError::Connection(DatabaseBackend::PostgreSql))?;
            if let Err(error) = migrations::postgresql(&pool).await {
                pool.close().await;
                return Err(error);
            }
            Ok(PersistencePool::PostgreSql(pool))
        }
    }
}

async fn initialize_mcp_pool(
    target: DatabaseTarget,
) -> Result<(PersistencePool, Arc<dyn StreamableHttpSessionStore>), PersistenceError> {
    let backend = target.backend();
    match target {
        DatabaseTarget::Sqlite {
            options, in_memory, ..
        } => {
            let pool = sqlite_pool(options, in_memory).await?;
            if let Ok(store) = SqliteStreamableHttpSessionStore::from_pool_with_config(
                pool.clone(),
                StreamableHttpSessionStoreConfig::default(),
            )
            .await
            {
                Ok((PersistencePool::Sqlite(pool), Arc::new(store)))
            } else {
                pool.close().await;
                Err(PersistenceError::McpSessionStore(backend))
            }
        }
        DatabaseTarget::PostgreSql { options } => {
            let pool = PgPoolOptions::new()
                .connect_with(options)
                .await
                .map_err(|_| PersistenceError::Connection(DatabaseBackend::PostgreSql))?;
            if let Ok(store) = PostgresStreamableHttpSessionStore::from_pool_with_config(
                pool.clone(),
                StreamableHttpSessionStoreConfig::default(),
            )
            .await
            {
                Ok((PersistencePool::PostgreSql(pool), Arc::new(store)))
            } else {
                pool.close().await;
                Err(PersistenceError::McpSessionStore(backend))
            }
        }
    }
}

async fn sqlite_pool(
    options: SqliteConnectOptions,
    in_memory: bool,
) -> Result<SqlitePool, PersistenceError> {
    let options = options
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(SQLITE_BUSY_TIMEOUT);
    let mut pool_options = SqlitePoolOptions::new();
    if in_memory {
        pool_options = pool_options
            .max_connections(1)
            .min_connections(1)
            .max_lifetime(None)
            .idle_timeout(None);
    }
    pool_options
        .connect_with(options)
        .await
        .map_err(|_| PersistenceError::Connection(DatabaseBackend::Sqlite))
}

async fn create_sqlite_parent(target: &DatabaseTarget) -> Result<(), PersistenceError> {
    let Some(parent) = target
        .sqlite_path()
        .and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    tokio::fs::create_dir_all(parent).await.map_err(|_| {
        PersistenceError::Directory(
            "could not create a SQLite database parent directory; check configured paths and permissions",
        )
    })
}

fn validate_sqlite_file_identities(config: &DatabaseConfig) -> Result<(), PersistenceError> {
    let files = [&config.waltr, &config.mcp]
        .into_iter()
        .filter_map(DatabaseTarget::sqlite_path)
        .map(open_sqlite_file)
        .collect::<Result<Vec<_>, _>>()?;
    if files.len() == 2 && file_identity(&files[0])? == file_identity(&files[1])? {
        return Err(PersistenceError::Configuration(
            "WALTR_DATABASE_URL and WALTR_MCP_DATABASE_URL must not use the same SQLite file",
        ));
    }
    Ok(())
}

fn open_sqlite_file(path: &Path) -> Result<File, PersistenceError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| PersistenceError::Connection(DatabaseBackend::Sqlite))
}

#[cfg(unix)]
fn file_identity(file: &File) -> Result<(u64, u64), PersistenceError> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file
        .metadata()
        .map_err(|_| PersistenceError::Connection(DatabaseBackend::Sqlite))?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn file_identity(file: &File) -> Result<(u32, u64), PersistenceError> {
    use std::os::windows::fs::MetadataExt as _;

    let metadata = file
        .metadata()
        .map_err(|_| PersistenceError::Connection(DatabaseBackend::Sqlite))?;
    let volume = metadata
        .volume_serial_number()
        .ok_or(PersistenceError::Configuration(
            "could not establish SQLite database file identity",
        ))?;
    let index = metadata
        .file_index()
        .ok_or(PersistenceError::Configuration(
            "could not establish SQLite database file identity",
        ))?;
    Ok((volume, index))
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_: &File) -> Result<(), PersistenceError> {
    Err(PersistenceError::Configuration(
        "could not establish SQLite database file identity",
    ))
}

fn parse_target(
    database_url: &str,
    environment_name: &'static str,
) -> Result<DatabaseTarget, PersistenceError> {
    if database_url.starts_with("sqlite:") {
        let mut options = SqliteConnectOptions::from_str(database_url)
            .map_err(|_| invalid_url(environment_name, "is not a valid SQLite database URL"))?;
        let memory = sqlite_memory_identity(database_url, &options);
        let in_memory = memory.is_some();
        let path = if in_memory {
            None
        } else {
            let path = expand_home(options.get_filename(), environment_name)?;
            if path
                .to_string_lossy()
                .get(..5)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
            {
                return Err(nested_sqlite_uri(environment_name));
            }
            if path.as_os_str().is_empty() {
                return Err(invalid_url(
                    environment_name,
                    "must name a SQLite file or explicit in-memory database",
                ));
            }
            options = options.filename(&path);
            Some(path)
        };
        let identity = memory
            .unwrap_or_else(|| SqliteIdentity::File(path.clone().expect("file path established")));
        Ok(DatabaseTarget::Sqlite {
            options,
            path,
            in_memory,
            identity,
        })
    } else if database_url.starts_with("postgres://") || database_url.starts_with("postgresql://") {
        let options = PgConnectOptions::from_str(database_url)
            .map_err(|_| invalid_url(environment_name, "is not a valid PostgreSQL database URL"))?;
        Ok(DatabaseTarget::PostgreSql {
            options: apply_postgres_ssl_policy(database_url, options),
        })
    } else {
        Err(invalid_url(
            environment_name,
            "must use sqlite:, postgres://, or postgresql://",
        ))
    }
}

fn nested_sqlite_uri(environment_name: &'static str) -> PersistenceError {
    PersistenceError::Configuration(match environment_name {
        ENV_DATABASE_URL => {
            "WALTR_DATABASE_URL must use a direct SQLite path rather than a nested file: URI"
        }
        ENV_MCP_DATABASE_URL => {
            "WALTR_MCP_DATABASE_URL must use a direct SQLite path rather than a nested file: URI"
        }
        _ => "database URL must use a direct SQLite path rather than a nested file: URI",
    })
}

fn invalid_url(environment_name: &'static str, reason: &'static str) -> PersistenceError {
    let message = match (environment_name, reason) {
        (ENV_DATABASE_URL, "is not a valid SQLite database URL") => {
            "WALTR_DATABASE_URL is not a valid SQLite database URL"
        }
        (ENV_MCP_DATABASE_URL, "is not a valid SQLite database URL") => {
            "WALTR_MCP_DATABASE_URL is not a valid SQLite database URL"
        }
        (ENV_DATABASE_URL, "must name a SQLite file or explicit in-memory database") => {
            "WALTR_DATABASE_URL must name a SQLite file or explicit in-memory database"
        }
        (ENV_MCP_DATABASE_URL, "must name a SQLite file or explicit in-memory database") => {
            "WALTR_MCP_DATABASE_URL must name a SQLite file or explicit in-memory database"
        }
        (ENV_DATABASE_URL, "is not a valid PostgreSQL database URL") => {
            "WALTR_DATABASE_URL is not a valid PostgreSQL database URL"
        }
        (ENV_MCP_DATABASE_URL, "is not a valid PostgreSQL database URL") => {
            "WALTR_MCP_DATABASE_URL is not a valid PostgreSQL database URL"
        }
        (ENV_DATABASE_URL, _) => {
            "WALTR_DATABASE_URL must use sqlite:, postgres://, or postgresql://"
        }
        (ENV_MCP_DATABASE_URL, _) => {
            "WALTR_MCP_DATABASE_URL must use sqlite:, postgres://, or postgresql://"
        }
        _ => "database URL configuration is invalid",
    };
    PersistenceError::Configuration(message)
}

fn derive_mcp_target(waltr: &DatabaseTarget) -> Result<DatabaseTarget, PersistenceError> {
    match waltr {
        DatabaseTarget::Sqlite {
            options,
            path: Some(path),
            ..
        } => {
            let parent = path.parent().unwrap_or_else(|| Path::new(""));
            let path = parent.join("mcp.sqlite3");
            Ok(DatabaseTarget::Sqlite {
                options: options.clone().filename(&path),
                path: Some(path.clone()),
                in_memory: false,
                identity: SqliteIdentity::File(path),
            })
        }
        DatabaseTarget::Sqlite { .. } => parse_target("sqlite::memory:", ENV_MCP_DATABASE_URL),
        DatabaseTarget::PostgreSql { options } => Ok(DatabaseTarget::PostgreSql {
            options: options.clone(),
        }),
    }
}

fn sqlite_file_target(path: PathBuf) -> DatabaseTarget {
    DatabaseTarget::Sqlite {
        options: SqliteConnectOptions::new().filename(&path),
        path: Some(path.clone()),
        in_memory: false,
        identity: SqliteIdentity::File(path),
    }
}

fn read_optional_env(name: &'static str) -> Result<Option<String>, PersistenceError> {
    optional_env_value(name, env::var(name))
}

fn optional_env_value(
    name: &'static str,
    value: Result<String, env::VarError>,
) -> Result<Option<String>, PersistenceError> {
    match value {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(PersistenceError::Configuration(match name {
            ENV_DATABASE_URL => "WALTR_DATABASE_URL must contain valid Unicode",
            ENV_MCP_DATABASE_URL => "WALTR_MCP_DATABASE_URL must contain valid Unicode",
            _ => "database URL must contain valid Unicode",
        })),
    }
}

fn expand_home(path: &Path, environment_name: &'static str) -> Result<PathBuf, PersistenceError> {
    if path == Path::new("~") || path.starts_with("~/") {
        let home =
            env::var_os("HOME").ok_or(PersistenceError::Configuration(match environment_name {
                ENV_DATABASE_URL => "WALTR_DATABASE_URL uses ~ but HOME is not set",
                ENV_MCP_DATABASE_URL => "WALTR_MCP_DATABASE_URL uses ~ but HOME is not set",
                _ => "database URL uses ~ but HOME is not set",
            }))?;
        let remainder = path.strip_prefix("~").expect("prefix checked");
        return Ok(PathBuf::from(home).join(remainder));
    }
    Ok(path.to_owned())
}

fn normalized_absolute_path(path: &Path) -> Result<PathBuf, PersistenceError> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()
            .map_err(|_| {
                PersistenceError::Configuration("could not resolve SQLite database paths")
            })?
            .join(path)
    };
    let mut ancestor = absolute;
    let mut suffix = Vec::<OsString>::new();
    let canonical =
        loop {
            match std::fs::canonicalize(&ancestor) {
                Ok(canonical) => break canonical,
                Err(_) if ancestor.try_exists().is_ok_and(|exists| !exists) => {
                    let component = ancestor.components().next_back().ok_or(
                        PersistenceError::Configuration("could not resolve SQLite database paths"),
                    )?;
                    let component = match component {
                        Component::CurDir => OsString::from("."),
                        Component::ParentDir => OsString::from(".."),
                        Component::Normal(component) => component.to_owned(),
                        Component::Prefix(_) | Component::RootDir => {
                            return Err(PersistenceError::Configuration(
                                "could not resolve SQLite database paths",
                            ));
                        }
                    };
                    suffix.push(component);
                    if !ancestor.pop() {
                        return Err(PersistenceError::Configuration(
                            "could not resolve SQLite database paths",
                        ));
                    }
                }
                Err(_) => {
                    return Err(PersistenceError::Configuration(
                        "could not resolve SQLite database paths",
                    ));
                }
            }
        };
    let mut normalized = canonical;
    for component in suffix.iter().rev() {
        normalize_component(&mut normalized, Path::new(component));
    }
    Ok(normalized)
}

fn normalize_component(path: &mut PathBuf, component: &Path) {
    for component in component.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                path.pop();
            }
            component => path.push(component.as_os_str()),
        }
    }
}

fn default_data_directory(
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, PersistenceError> {
    if let Some(xdg_data_home) = xdg_data_home
        .filter(|value| !value.is_empty())
        .map(Path::new)
        .filter(|path| path.is_absolute())
    {
        return Ok(xdg_data_home.join("waltr"));
    }
    if let Some(home) = home
        .filter(|value| !value.is_empty())
        .map(Path::new)
        .filter(|path| path.is_absolute())
    {
        return Ok(home.join(".local/share/waltr"));
    }
    Err(PersistenceError::Configuration(
        "WALTR_DATABASE_URL is unset and neither XDG_DATA_HOME nor HOME provides an absolute data path",
    ))
}

fn apply_postgres_ssl_policy(database_url: &str, options: PgConnectOptions) -> PgConnectOptions {
    if postgres_url_has_ssl_mode(database_url) {
        options
    } else {
        options.ssl_mode(PgSslMode::Require)
    }
}

fn postgres_url_has_ssl_mode(database_url: &str) -> bool {
    url::Url::parse(database_url).is_ok_and(|url| {
        url.query_pairs()
            .any(|(name, _)| matches!(name.as_ref(), "sslmode" | "ssl-mode"))
    })
}

fn sqlite_memory_identity(
    database_url: &str,
    options: &SqliteConnectOptions,
) -> Option<SqliteIdentity> {
    let value = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .unwrap_or(database_url);
    let (database, parameters) = value
        .split_once('?')
        .map_or((value, ""), |(database, parameters)| (database, parameters));
    let private_memory = database == ":memory:";
    let mut in_memory = private_memory;
    let mut shared_cache = private_memory;
    for (name, value) in url::form_urlencoded::parse(parameters.as_bytes()) {
        match (name.as_ref(), value.as_ref()) {
            ("mode", "memory") => {
                in_memory = true;
                shared_cache = true;
            }
            ("cache", "shared") => shared_cache = true,
            ("cache", "private") => shared_cache = false,
            _ => {}
        }
    }
    if !in_memory || private_memory || !shared_cache {
        in_memory.then_some(SqliteIdentity::PrivateMemory)
    } else {
        Some(SqliteIdentity::SharedMemory(
            options.get_filename().to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests;
