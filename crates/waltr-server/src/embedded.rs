//! Ambient-free composition for an embedding application.

use crate::{
    LocalBearer, ServerApplication,
    access_auth::AccessAuthenticator,
    app::{AppServices, LocalAdmission, assemble_app},
    auth::OutboundOAuthConfig,
    catalog::{CatalogLoader, CatalogMaterializer},
    config_service::ConfigService,
    persistence::{DatabaseConfig, Persistence},
    resource_catalog::routine::InteractionLimits,
    resource_catalog::{AgentDefinitionState, provision_persisted_resource},
    session,
};
use std::{fmt, future::Future, io, net::SocketAddr, path::PathBuf};

const BUILT_IN_CATALOG: &str = include_str!("../resources/models-dev-defaults.json");

/// Explicit models.dev source used by an embedded generation.
#[derive(Clone)]
pub enum EmbeddedCatalogSource {
    /// Fetch the catalog first and fall back to the configured cache when the fetch fails.
    Url(String),
    /// Load a local catalog fixture or bundled extraction.
    Path(PathBuf),
}

/// Optional final catalog fallback selected by an embedding host.
#[derive(Clone, Copy, Debug)]
pub enum EmbeddedCatalogFallback {
    /// Use Waltr's curated catalog after a network failure when no cache file exists.
    BuiltIn,
}

impl fmt::Debug for EmbeddedCatalogSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Url(_) => "EmbeddedCatalogSource::Url(<redacted>)",
            Self::Path(_) => "EmbeddedCatalogSource::Path(<redacted>)",
        })
    }
}

/// Explicit outbound HTTP exceptions used by provider/upstream-MCP OAuth.
#[derive(Clone, Default)]
pub struct EmbeddedOutboundPolicy {
    /// Origins allowed to use private-network addresses during outbound OAuth.
    pub trusted_private_origins: Vec<reqwest::Url>,
}

impl fmt::Debug for EmbeddedOutboundPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EmbeddedOutboundPolicy")
            .field(
                "trusted_private_origin_count",
                &self.trusted_private_origins.len(),
            )
            .finish()
    }
}

/// Complete host-owned configuration for one embedded server generation.
#[derive(Clone)]
pub struct EmbeddedServerConfig {
    /// Root containing the editable `.waltr/resources` tree.
    pub resources_root: PathBuf,
    /// Independent Waltr-owned SQLite file.
    pub waltr_database_path: PathBuf,
    /// Independent generic-MCP-owned SQLite file.
    pub mcp_database_path: PathBuf,
    /// Filesystem session-store root.
    pub session_root: PathBuf,
    /// App-private workspace used by agent filesystem, context, skills, templates, and shell.
    pub workspace_root: PathBuf,
    /// Models catalog source.
    pub catalog_source: EmbeddedCatalogSource,
    /// Models catalog cache file.
    pub catalog_cache_path: PathBuf,
    /// Optional host-selected fallback used only when the cache file does not exist.
    pub catalog_fallback: Option<EmbeddedCatalogFallback>,
    /// Exact default agent name.
    pub default_agent: String,
    /// Process-generation bearer supplied by the embedding host.
    pub local_bearer: LocalBearer,
    /// Explicit provider/upstream-MCP outbound OAuth policy.
    pub outbound_policy: EmbeddedOutboundPolicy,
    /// Finite interaction admission and transport bounds; use `InteractionLimits::default()` for defaults.
    pub interaction_limits: InteractionLimits,
}

impl fmt::Debug for EmbeddedServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EmbeddedServerConfig")
            .field("catalog_source", &self.catalog_source)
            .field("outbound_policy", &self.outbound_policy)
            .finish_non_exhaustive()
    }
}

/// Prebound loopback listener and all owned server lifecycle state.
pub struct EmbeddedServer {
    application: ServerApplication,
    listener: tokio::net::TcpListener,
    address: SocketAddr,
}

impl fmt::Debug for EmbeddedServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EmbeddedServer { <redacted> }")
    }
}

impl EmbeddedServer {
    /// Return the exact prebound loopback address.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// Wait for all readiness dependencies.
    pub async fn wait_until_ready(&self) {
        self.application.wait_until_ready().await;
    }

    /// Drop the listener and synchronously stop ordinary sessions and background work.
    pub async fn shutdown(self) {
        drop(self.listener);
        self.application.shutdown().await;
    }

    /// Serve the prebound listener until shutdown resolves, then join all owned work.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP server fails.
    pub async fn serve_with_shutdown<F>(self, shutdown: F) -> io::Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.application
            .serve_with_shutdown(self.listener, shutdown)
            .await
    }
}

/// Prebind loopback and construct one ambient-free embedded server generation.
///
/// # Errors
///
/// Returns a sanitized category when explicit paths, persistence, catalog materialization, or
/// application assembly fails.
pub async fn build_embedded_server(
    config: EmbeddedServerConfig,
) -> Result<EmbeddedServer, EmbeddedBuildError> {
    crate::routine_interaction_service::RoutineInteractionService::validate_limits(
        &config.interaction_limits,
    )
    .map_err(|_| EmbeddedBuildError::Application)?;
    validate_paths(&config)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| EmbeddedBuildError::Listener)?;
    let address = listener
        .local_addr()
        .map_err(|_| EmbeddedBuildError::Listener)?;
    let self_mcp_url = format!("http://{address}/mcp");
    let outbound_callback_url = format!("http://{address}/outbound-oauth/callback");

    tokio::fs::create_dir_all(&config.resources_root)
        .await
        .map_err(|_| EmbeddedBuildError::Paths)?;
    tokio::fs::create_dir_all(&config.workspace_root)
        .await
        .map_err(|_| EmbeddedBuildError::Paths)?;
    let self_mcp_source = self_mcp_resource(&self_mcp_url);
    provision_persisted_resource(
        &config.resources_root,
        "mcp-servers/waltr.yaml",
        &self_mcp_source,
    )
    .map_err(|_| EmbeddedBuildError::Resources)?;
    let database =
        DatabaseConfig::from_sqlite_paths(config.waltr_database_path, config.mcp_database_path)
            .map_err(|_| EmbeddedBuildError::Persistence)?;
    let persistence = Persistence::initialize(database)
        .await
        .map_err(|_| EmbeddedBuildError::Persistence)?;
    let mut catalog_loader = match config.catalog_source {
        EmbeddedCatalogSource::Url(url) => {
            CatalogLoader::from_url_with_cache(url, config.catalog_cache_path)
        }
        EmbeddedCatalogSource::Path(path) => {
            CatalogLoader::from_path_with_cache(path, config.catalog_cache_path)
        }
    };
    if let Some(EmbeddedCatalogFallback::BuiltIn) = config.catalog_fallback {
        catalog_loader = catalog_loader.with_fallback_catalog(BUILT_IN_CATALOG);
    }
    let materializer = CatalogMaterializer::load(&catalog_loader)
        .await
        .map_err(|_| EmbeddedBuildError::Catalog)?;
    let state = AgentDefinitionState::load_unified_explicit(
        config.resources_root,
        materializer,
        config.default_agent,
    )
    .map_err(|_| EmbeddedBuildError::Resources)?;
    let config_service = ConfigService::from_state(state.clone(), catalog_loader.clone());
    let self_resource =
        reqwest::Url::parse(&self_mcp_url).map_err(|_| EmbeddedBuildError::Application)?;
    let trusted_origins = config.outbound_policy.trusted_private_origins;
    let session_origins =
        super::app::agent_trusted_origins(trusted_origins.clone(), Some(&self_resource));
    let session_root = config.session_root;
    let workspace_root = config.workspace_root;
    let bearer = config.local_bearer;
    let application = assemble_app(
        AppServices {
            config_service,
            persistence,
            catalog_loader,
            access_authenticator: AccessAuthenticator::disabled(),
            mcp_oauth: None,
            outbound_private_origins: trusted_origins,
            outbound_oauth: OutboundOAuthConfig::for_server_callback(outbound_callback_url),
            automatic_self_mcp_resource: Some(self_resource),
            agent_state: Some(state),
            local_admission: Some(LocalAdmission {
                bearer,
                self_mcp_url,
            }),
            workspace_root,
            sandbox_socket: None,
            interaction_limits: config.interaction_limits,
        },
        move |routes, mut input| {
            input.trusted_mcp_oauth_origins = session_origins;
            session::add_storage_service(
                routes,
                session::SessionStorageConfig::Filesystem { root: session_root },
                input,
            )
        },
    )
    .await
    .map_err(|_| EmbeddedBuildError::Application)?;

    Ok(EmbeddedServer {
        application,
        listener,
        address,
    })
}

fn self_mcp_resource(url: &str) -> String {
    let url = serde_json::to_string(url).expect("a string always serializes");
    format!(
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: streamableHttp\n  url: {url}\n"
    )
}

fn validate_paths(config: &EmbeddedServerConfig) -> Result<(), EmbeddedBuildError> {
    let paths = [
        &config.resources_root,
        &config.waltr_database_path,
        &config.mcp_database_path,
        &config.session_root,
        &config.workspace_root,
        &config.catalog_cache_path,
    ];
    let catalog_path_absolute = match &config.catalog_source {
        EmbeddedCatalogSource::Path(path) => path.is_absolute(),
        EmbeddedCatalogSource::Url(_) => true,
    };
    if paths.iter().any(|path| !path.is_absolute())
        || !catalog_path_absolute
        || !crate::workspace_paths::overlaps(&config.workspace_root, &config.session_root)
            .is_ok_and(|overlap| !overlap)
        || !crate::workspace_paths::inside_config(&config.workspace_root, &config.resources_root)
            .is_ok_and(|inside| !inside)
        || [
            &config.waltr_database_path,
            &config.mcp_database_path,
            &config.catalog_cache_path,
        ]
        .into_iter()
        .any(|path| {
            !crate::workspace_paths::contains_file(&config.workspace_root, path)
                .is_ok_and(|inside| !inside)
        })
    {
        return Err(EmbeddedBuildError::Paths);
    }
    Ok(())
}

/// Sanitized embedded construction failure category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddedBuildError {
    /// An explicit path was invalid or unavailable.
    Paths,
    /// The loopback listener could not be prebound.
    Listener,
    /// Independent persistence could not initialize.
    Persistence,
    /// The models catalog and cache fallback were unavailable.
    Catalog,
    /// Resources could not be materialized.
    Resources,
    /// Server services could not be assembled.
    Application,
}

impl fmt::Display for EmbeddedBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Paths => "embedded server paths are invalid or unavailable",
            Self::Listener => "embedded loopback listener could not be initialized",
            Self::Persistence => "embedded server persistence could not be initialized",
            Self::Catalog => "embedded models catalog could not be initialized",
            Self::Resources => "embedded resource catalog could not be initialized",
            Self::Application => "embedded server application could not be initialized",
        })
    }
}

impl std::error::Error for EmbeddedBuildError {}
