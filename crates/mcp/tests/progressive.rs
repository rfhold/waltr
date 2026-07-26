//! Integration tests for the exported `mcp::progressive_server` macro.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use mcp::{
    McpAccessToken, McpProtectedResourceMetadata, McpTokenValidation,
    protocol::MCP_PROTOCOL_VERSION,
    server::{McpAuthorizationContext, ServerContext, ServerResult},
};
use reqwest::StatusCode;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};

#[derive(Clone, Default)]
struct ReleaseServer {
    validations: Arc<Mutex<Vec<String>>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListReleasesInput {
    /// Release status to include.
    status: Option<String>,
    /// Maximum number of releases to return.
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetReleaseInput {
    /// Release id.
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PolicyCheckInput {
    /// Release id to check.
    id: String,
    /// Target deployment environment.
    environment: String,
}

#[mcp::progressive_server(
    name = "release-desk",
    version = "1.0.0",
    description = "Release management for the example org.",
    namespace(
        releases,
        description = "Query and inspect product releases. Use these to ground any statement about what shipped and when."
    ),
    namespace(
        policy,
        description = "Org release policies and compliance checks. Consult before recommending or performing any promotion."
    ),
    workflow_uri_template = "playbooks://{workflow}",
    auth(
        scopes = ["releases:read"],
        required_scopes = ["releases:read"],
        realm = "release-desk",
        resource_name = "Release Desk"
    )
)]
impl ReleaseServer {
    /// List releases when the user needs release ids, dates, or status context.
    ///
    /// Use this before summarizing release state or choosing a release for a
    /// follow-up action. Prefer a status filter when the user asks about a
    /// specific lifecycle state.
    #[action(name = "releases.list")]
    fn list_releases(
        &self,
        input: ListReleasesInput,
        _context: ServerContext,
    ) -> ServerResult<Value> {
        let releases = [
            json!({ "id": "v1.4.2", "status": "released", "date": "2026-07-01" }),
            json!({ "id": "v1.4.1", "status": "released", "date": "2026-06-12" }),
            json!({ "id": "v1.5.0-rc.1", "status": "candidate", "date": "2026-07-09" }),
        ];
        let mut releases = releases
            .into_iter()
            .filter(|release| {
                input.status.as_ref().is_none_or(|status| {
                    release["status"]
                        .as_str()
                        .is_some_and(|value| value == status)
                })
            })
            .collect::<Vec<_>>();
        releases.truncate(input.limit.unwrap_or(releases.len()));
        Ok(Value::Array(releases))
    }

    /// Read a specific release when the user names or selects a release id.
    ///
    /// Use this after `releases.list` or when a resource URI already contains
    /// the release id. This is the grounding action for release-specific facts.
    #[action(name = "releases.get", resource_template = "releases://{id}")]
    fn get_release(&self, input: GetReleaseInput, _context: ServerContext) -> ServerResult<Value> {
        Ok(json!({
            "id": input.id,
            "status": "released",
            "owner": "platform",
            "notes": ["fix login redirect", "improve deploy telemetry"]
        }))
    }

    /// Explain the promotion policy before recommending a production promotion.
    ///
    /// Use this whenever the user asks whether something can be promoted, or
    /// when you need the static promotion criteria without checking a specific
    /// release candidate.
    #[action(name = "policy.promotion", resource = "policy://promotion")]
    fn promotion_policy(&self, _context: ServerContext) -> ServerResult<Value> {
        Ok(json!({
            "requires_approval": true,
            "required_checks": ["release notes", "security review", "rollback plan"]
        }))
    }

    /// Check release policy against one release and target environment.
    ///
    /// Use this before advising a deploy decision. The result is scoped to the
    /// requested release id and environment, unlike `policy.promotion`.
    #[action(name = "policy.check")]
    fn check_policy(
        &self,
        input: PolicyCheckInput,
        _context: ServerContext,
    ) -> ServerResult<Value> {
        Ok(json!({
            "id": input.id,
            "environment": input.environment,
            "allowed": true,
            "missing": []
        }))
    }

    /// Promote a release candidate to production with required pre-checks.
    ///
    /// Load this workflow when the user wants an end-to-end production
    /// promotion procedure rather than a single release fact.
    #[workflow(name = "promote-to-production")]
    fn promote_workflow(&self) -> ServerResult<String> {
        Ok("## Promote to production\n1. Load release details.\n2. Check policy.\n3. Confirm rollback plan.\n".to_owned())
    }

    /// Roll back production to the previous known-good release.
    ///
    /// Load this workflow when the user reports production impact or asks for a
    /// rollback playbook.
    #[workflow(name = "rollback-release")]
    fn rollback_workflow(&self) -> ServerResult<String> {
        Ok("## Rollback release\n1. Identify the bad release.\n2. Select previous release.\n3. Announce rollback.\n".to_owned())
    }

    #[token_validator]
    fn validate_token(
        &self,
        token: McpAccessToken,
        context: McpAuthorizationContext,
    ) -> McpTokenValidation {
        self.validations
            .lock()
            .expect("validations lock")
            .push(format!(
                "{}:{}",
                context.resource,
                context.required_scopes.join(" ")
            ));
        if token.0 == "release-desk-secret" {
            McpTokenValidation::Authorized {
                principal_id: mcp::McpPrincipalId::new("progressive-test")
                    .expect("valid principal"),
                expires_at: None,
            }
        } else {
            McpTokenValidation::Unauthorized {
                error_description: Some("invalid release desk token".to_owned()),
            }
        }
    }
}

#[tokio::test]
async fn progressive_server_generates_exec_help_workflows_and_resources() -> mcp::Result<()> {
    let server = RunningServer::start_plain(ReleaseServer::default()).await?;

    let initialize = post_json(&server.endpoint, initialize_body("initialize")).await?;
    assert_eq!(initialize.status, StatusCode::OK);
    assert_eq!(
        initialize.body["result"]["serverInfo"]["name"],
        "release-desk"
    );
    assert_eq!(
        initialize.body["result"]["capabilities"]["tools"]["listChanged"],
        false
    );
    assert!(initialize.body["result"]["capabilities"]["prompts"].is_object());
    assert!(initialize.body["result"]["capabilities"]["resources"].is_object());
    assert!(
        initialize.body["result"]["instructions"]
            .as_str()
            .is_some_and(
                |instructions| instructions.contains("Use `exec` with action `help` first")
            )
    );

    let tools = post_json(
        &server.endpoint,
        json!({ "jsonrpc": "2.0", "id": "tools-list", "method": "tools/list" }),
    )
    .await?;
    assert_eq!(
        tools.body["result"]["tools"]
            .as_array()
            .expect("tools")
            .len(),
        1
    );
    assert_eq!(tools.body["result"]["tools"][0]["name"], "exec");
    assert_array_contains(
        &tools.body["result"]["tools"][0]["inputSchema"]["properties"]["action"]["enum"],
        "help.releases",
    );
    assert_array_contains(
        &tools.body["result"]["tools"][0]["inputSchema"]["properties"]["action"]["enum"],
        "workflows.load",
    );
    let input_options =
        tools.body["result"]["tools"][0]["inputSchema"]["properties"]["input"]["oneOf"]
            .as_array()
            .expect("input oneOf");
    assert!(
        input_options
            .iter()
            .any(|schema| schema.to_string().contains("Release id."))
    );

    let root_help = call_exec(&server.endpoint, json!({ "action": "help" })).await?;
    assert_eq!(root_help["structuredContent"]["topic"], "help");
    assert_eq!(
        root_help["structuredContent"]["namespaces"][0]["help_action"],
        "help.releases"
    );
    assert_eq!(
        root_help["structuredContent"]["namespaces"][1]["description"],
        "Org release policies and compliance checks. Consult before recommending or performing any promotion."
    );

    let policy_help = call_exec(&server.endpoint, json!({ "action": "help.policy" })).await?;
    assert_eq!(policy_help["structuredContent"]["topic"], "help.policy");
    assert_eq!(
        policy_help["structuredContent"]["actions"][0]["action"],
        "policy.promotion"
    );
    assert_eq!(
        policy_help["structuredContent"]["actions"][0]["description"],
        "Explain the promotion policy before recommending a production promotion."
    );
    assert!(
        policy_help["structuredContent"]["actions"][0]["guidance"]
            .as_str()
            .is_some_and(|guidance| guidance.contains("static promotion criteria"))
    );
    assert_eq!(
        policy_help["structuredContent"]["actions"][0]["resource"],
        "policy://promotion"
    );

    let workflows_help = call_exec(&server.endpoint, json!({ "action": "help.workflows" })).await?;
    assert_eq!(
        workflows_help["structuredContent"]["resource_template"],
        "playbooks://{workflow}"
    );
    assert_eq!(
        workflows_help["structuredContent"]["workflows"][0]["description"],
        "Promote a release candidate to production with required pre-checks."
    );
    assert_eq!(
        workflows_help["structuredContent"]["workflows"][0]["resource"],
        "playbooks://promote-to-production"
    );

    let filtered = call_exec(
        &server.endpoint,
        json!({
            "action": "releases.list",
            "input": { "status": "released", "limit": 2 },
            "filter": ".[].id"
        }),
    )
    .await?;
    assert_eq!(filtered["structuredContent"], json!(["v1.4.2", "v1.4.1"]));

    let workflow = call_exec(
        &server.endpoint,
        json!({
            "action": "workflows.load",
            "input": { "workflow": "promote-to-production" }
        }),
    )
    .await?;
    assert!(
        workflow["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("## Promote to production"))
    );
    assert!(
        workflow["structuredContent"]["markdown"]
            .as_str()
            .is_some_and(|text| text.contains("Check policy"))
    );
    assert_eq!(
        workflow["structuredContent"]["workflow"],
        "promote-to-production"
    );

    let prompts = post_json(
        &server.endpoint,
        json!({ "jsonrpc": "2.0", "id": "prompts-list", "method": "prompts/list" }),
    )
    .await?;
    assert_eq!(
        prompts.body["result"]["prompts"][0]["name"],
        "promote-to-production"
    );
    assert_eq!(
        prompts.body["result"]["prompts"][1]["name"],
        "rollback-release"
    );

    let prompt = post_json(
        &server.endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "prompt-get",
            "method": "prompts/get",
            "params": { "name": "promote-to-production" }
        }),
    )
    .await?;
    assert!(
        prompt.body["result"]["messages"][0]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("## Promote to production"))
    );

    let resources = post_json(
        &server.endpoint,
        json!({ "jsonrpc": "2.0", "id": "resources-list", "method": "resources/list" }),
    )
    .await?;
    assert_eq!(
        resources.body["result"]["resources"][0]["uri"],
        "policy://promotion"
    );

    let templates = post_json(
        &server.endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "templates-list",
            "method": "resources/templates/list"
        }),
    )
    .await?;
    assert_eq!(
        templates.body["result"]["resourceTemplates"][0]["uriTemplate"],
        "releases://{id}"
    );
    assert_eq!(
        templates.body["result"]["resourceTemplates"][1]["uriTemplate"],
        "playbooks://{workflow}"
    );

    let policy_resource = post_json(
        &server.endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "policy-resource",
            "method": "resources/read",
            "params": { "uri": "policy://promotion" }
        }),
    )
    .await?;
    assert!(
        policy_resource.body["result"]["contents"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("requires_approval"))
    );

    let release_resource = post_json(
        &server.endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "release-resource",
            "method": "resources/read",
            "params": { "uri": "releases://v1.4.2" }
        }),
    )
    .await?;
    let release: Value = serde_json::from_str(
        release_resource.body["result"]["contents"][0]["text"]
            .as_str()
            .expect("release resource text"),
    )?;
    assert_eq!(release["id"], "v1.4.2");

    let workflow_resource = post_json(
        &server.endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "workflow-resource",
            "method": "resources/read",
            "params": { "uri": "playbooks://rollback-release" }
        }),
    )
    .await?;
    assert!(
        workflow_resource.body["result"]["contents"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("## Rollback release"))
    );

    let missing_input = post_json(
        &server.endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "missing-input",
            "method": "tools/call",
            "params": { "name": "exec", "arguments": { "action": "releases.get" } }
        }),
    )
    .await?;
    assert!(
        missing_input.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("help.releases"))
    );

    let unknown_action = post_json(
        &server.endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "unknown-action",
            "method": "tools/call",
            "params": { "name": "exec", "arguments": { "action": "releases.delete" } }
        }),
    )
    .await?;
    assert!(
        unknown_action.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("call `help`"))
    );

    Ok(())
}

#[tokio::test]
async fn progressive_server_generates_auth_helpers() -> mcp::Result<()> {
    let handler = ReleaseServer::default();
    let validations = Arc::clone(&handler.validations);
    let server = RunningServer::start_authorized(handler).await?;

    let metadata = reqwest::Client::new()
        .get(format!(
            "{}/.well-known/oauth-protected-resource/mcp",
            server.origin
        ))
        .send()
        .await?
        .error_for_status()?
        .json::<McpProtectedResourceMetadata>()
        .await?;
    assert_eq!(metadata.resource, server.endpoint);
    assert_eq!(metadata.authorization_servers, ["https://auth.example.com"]);
    assert_eq!(metadata.scopes_supported, ["releases:read"]);
    assert_eq!(metadata.resource_name.as_deref(), Some("Release Desk"));

    let unauthorized = reqwest::Client::new()
        .post(&server.endpoint)
        .header("accept", "application/json, text/event-stream")
        .json(&initialize_body("unauthorized"))
        .send()
        .await?;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert!(
        unauthorized
            .headers()
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .any(|value| value.contains("realm=\"release-desk\"")
                && value.contains("scope=\"releases:read\""))
    );

    let authorized = reqwest::Client::new()
        .post(&server.endpoint)
        .header("accept", "application/json, text/event-stream")
        .bearer_auth("release-desk-secret")
        .json(&initialize_body("authorized"))
        .send()
        .await?;
    assert_eq!(authorized.status(), StatusCode::OK);
    let body = authorized.json::<Value>().await?;
    assert_eq!(body["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
    assert_eq!(
        validations.lock().expect("validations lock").as_slice(),
        [format!("{}:releases:read", server.endpoint)]
    );

    Ok(())
}

struct RunningServer {
    endpoint: String,
    origin: String,
    _task: JoinHandle<()>,
}

impl RunningServer {
    async fn start_plain(handler: ReleaseServer) -> mcp::Result<Self> {
        Self::start_with_router(handler, |handler, _endpoint| {
            Ok(handler.streamable_http_router())
        })
        .await
    }

    async fn start_authorized(handler: ReleaseServer) -> mcp::Result<Self> {
        Self::start_with_router(handler, |handler, endpoint| {
            let metadata = ReleaseServer::protected_resource_metadata(
                endpoint.to_owned(),
                ["https://auth.example.com"],
            );
            handler.streamable_http_router_with_auth(metadata)
        })
        .await
    }

    async fn start_with_router(
        handler: ReleaseServer,
        router: impl FnOnce(ReleaseServer, &str) -> mcp::Result<mcp::server::StreamableHttpRouter>,
    ) -> mcp::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address: SocketAddr = listener.local_addr()?;
        let origin = format!("http://{address}");
        let endpoint = format!("{origin}/mcp");
        let router = router(handler, &endpoint)?;
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::debug!(%error, "mcp progressive test server stopped");
            }
        });
        Ok(Self {
            endpoint,
            origin,
            _task: task,
        })
    }
}

struct JsonResponse {
    status: StatusCode,
    body: Value,
}

async fn call_exec(endpoint: &str, arguments: Value) -> mcp::Result<Value> {
    let response = post_json(
        endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "exec",
            "method": "tools/call",
            "params": { "name": "exec", "arguments": arguments }
        }),
    )
    .await?;
    Ok(response.body["result"].clone())
}

async fn post_json(endpoint: &str, body: Value) -> mcp::Result<JsonResponse> {
    let client = reqwest::Client::new();
    let session_id = if should_initialize_before_post(&body) {
        Some(initialized_session(endpoint).await?)
    } else {
        None
    };

    let mut request = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(session_id) = &session_id {
        request = request
            .header("mcp-session-id", session_id)
            .header("mcp-protocol-version", MCP_PROTOCOL_VERSION);
    }
    let response = request.send().await?;
    let status = response.status();
    let body = response.json::<Value>().await?;
    Ok(JsonResponse { status, body })
}

async fn initialized_session(endpoint: &str) -> mcp::Result<String> {
    let client = reqwest::Client::new();
    let initialize = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .json(&initialize_body("progressive-test-initialize"))
        .send()
        .await?;
    let session_id = initialize
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| mcp::Error::protocol("missing MCP session id"))?
        .to_owned();
    drop(initialize.json::<Value>().await?);
    drop(
        client
            .post(endpoint)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session_id)
            .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
            }))
            .send()
            .await?
            .bytes()
            .await?,
    );
    Ok(session_id)
}

fn should_initialize_before_post(body: &Value) -> bool {
    let Some(method) = body.get("method").and_then(Value::as_str) else {
        return false;
    };
    !matches!(method, "initialize" | "ping") && !method.starts_with("notifications/")
}

fn initialize_body(id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "mcp-progressive-test-client", "version": "1.0.0" }
        }
    })
}

fn assert_array_contains(values: &Value, expected: &str) {
    assert!(
        values
            .as_array()
            .expect("array")
            .iter()
            .any(|value| value.as_str() == Some(expected)),
        "expected array to contain {expected}, got {values}"
    );
}
