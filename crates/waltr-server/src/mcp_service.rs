//! Waltr Model Context Protocol server surface.

#![allow(
    clippy::useless_vec,
    reason = "the progressive_server macro generates this allocation"
)]

use crate::config_service::ConfigService;
use waltr::grpc::waltr::config::v1::{
    GetConfigRequest, GetConfigResponse, PatchConfigRequest,
    config_service_server::ConfigService as GeneratedConfigService,
};
use mcp::server::{ServerError, ServerResult};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::sync::Arc;
use tonic::{Code, Request, Status};

/// MCP server that exposes Waltr configuration through the generated config RPC contract.
#[derive(Clone, Debug)]
pub struct WaltrMcpServer {
    config_service: Arc<ConfigService>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PatchConfigurationInput {
    /// Current configuration `ETag` from `configuration.inspect`.
    expected_etag: String,
    /// Replacement providers section: `{ "providers": { "key": { ... } } }`.
    #[serde(default)]
    providers: Option<Value>,
    /// Replacement MCP servers section: `{ "mcp_servers": { "key": { ... } } }`.
    #[serde(default)]
    mcp_servers: Option<Value>,
    /// Replacement runtime defaults section.
    #[serde(default)]
    runtime_defaults: Option<Value>,
}

impl WaltrMcpServer {
    /// Create a Waltr MCP server backed by the generated config service implementation.
    #[must_use]
    #[allow(
        clippy::missing_const_for_fn,
        reason = "keep the public constructor non-const without an API expansion"
    )]
    pub fn new(config_service: Arc<ConfigService>) -> Self {
        Self { config_service }
    }

    async fn get_config_response(&self) -> ServerResult<GetConfigResponse> {
        let response = GeneratedConfigService::get_config(
            self.config_service.as_ref(),
            Request::new(GetConfigRequest::default()),
        )
        .await
        .map_err(|status| rpc_status_to_server_error(&status))?;

        Ok(response.into_inner())
    }
}

#[allow(missing_docs)]
#[mcp::progressive_server(
    name = "waltr",
    version = env!("CARGO_PKG_VERSION"),
    description = "Waltr server configuration MCP surface.",
    namespace(
        configuration,
        description = "Inspect and update Waltr server configuration through the same RPC contract used by Waltr clients."
    ),
    workflow_uri_template = "waltr://workflows/{workflow}"
)]
impl WaltrMcpServer {
    /// Inspect the current Waltr configuration before proposing or applying a change.
    ///
    /// Use this first for configuration requests. The result includes the
    /// current config and `ETag` needed for updates.
    #[action(
        name = "configuration.inspect",
        resource = "waltr://configuration/current"
    )]
    async fn inspect_configuration(&self) -> ServerResult<Value> {
        response_to_value(self.get_config_response().await?)
    }

    /// Patch the Waltr configuration.
    ///
    /// Use this for configuration updates only. Each provided top-level patch
    /// key replaces the entire corresponding config section: `providers`,
    /// `mcp_servers`, or `runtime_defaults`.
    #[action(name = "configuration.patch")]
    async fn patch_configuration(&self, input: PatchConfigurationInput) -> ServerResult<Value> {
        let request = input.into_request()?;
        let response = GeneratedConfigService::patch_config(
            self.config_service.as_ref(),
            Request::new(request),
        )
        .await
        .map_err(|status| rpc_status_to_server_error(&status))?;

        response_to_value(response.into_inner())
    }

    /// Configure Waltr by inspecting the current config, choosing a minimal patch, and applying it.
    ///
    /// Load this workflow for any request to add, remove, or adjust providers,
    /// MCP servers, or runtime defaults. First call `configuration.inspect`,
    /// then prepare one top-level replacement patch with the current `ETag`, and
    /// finally call `configuration.patch`.
    #[workflow(name = "configuration")]
    #[allow(
        clippy::unnecessary_wraps,
        clippy::unused_self,
        reason = "the workflow macro requires this method signature"
    )]
    fn configuration_workflow(&self) -> ServerResult<String> {
        Ok(
            "## Waltr configuration workflow\n\n\
1. Call `configuration.inspect` to load the current configuration and ETag.\n\
2. Decide the smallest top-level section replacement needed: `providers`, `mcp_servers`, or `runtime_defaults`.\n\
3. Build the replacement carefully: any provided top-level key replaces that entire section, not just the fields listed inside it.\n\
4. Call `configuration.patch` with `expected_etag` from the inspected config.\n\
5. Treat validation or stale-ETag errors as authoritative and inspect again before retrying.\n"
                .to_owned(),
        )
    }
}

impl PatchConfigurationInput {
    fn into_request(self) -> ServerResult<PatchConfigRequest> {
        if self.expected_etag.trim().is_empty() {
            return Err(ServerError::invalid_params("missing expected_etag"));
        }

        let mut request = Map::new();
        request.insert(
            "expected_etag".to_owned(),
            Value::String(self.expected_etag),
        );
        if let Some(providers) = self.providers {
            request.insert("providers".to_owned(), providers);
        }
        if let Some(mcp_servers) = self.mcp_servers {
            request.insert("mcp_servers".to_owned(), mcp_servers);
        }
        if let Some(runtime_defaults) = self.runtime_defaults {
            request.insert("runtime_defaults".to_owned(), runtime_defaults);
        }

        serde_json::from_value(Value::Object(request)).map_err(|error| {
            ServerError::invalid_params(format!("invalid configuration patch input: {error}"))
        })
    }
}

fn response_to_value<T>(response: T) -> ServerResult<Value>
where
    T: serde::Serialize,
{
    serde_json::to_value(response).map_err(|error| {
        ServerError::internal(format!("failed to serialize config response: {error}"))
    })
}

fn rpc_status_to_server_error(status: &Status) -> ServerError {
    let message = format!("config RPC {}: {}", status.code(), status.message());
    match status.code() {
        Code::InvalidArgument | Code::FailedPrecondition => ServerError::invalid_params(message),
        _ => ServerError::internal(message),
    }
}
