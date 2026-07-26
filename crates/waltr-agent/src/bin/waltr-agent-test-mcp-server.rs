//! MCP test server used by integration tests.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use jsonwebtoken::{DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use mcp::{
    McpAccessToken, McpCompletionRequest, McpPrincipalId, McpPromptArgument, McpPromptDefinition,
    McpPromptResult, McpResourceDefinition, McpResourceResult, McpResourceTemplateDefinition,
    McpTokenValidation, McpToolCall, McpToolDefinition, McpToolResult,
    protocol::JsonRpcNotification,
    server::{McpAuthorizationContext, ServerContext, ServerError, ServerResult, serve_stdio},
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct EchoMarkerRequest {
    value: String,
}

#[derive(Debug, Deserialize)]
struct DelayedWriteRequest {
    path: String,
    value: String,
    delay_ms: u64,
}

#[derive(Debug, Deserialize)]
struct EmitLogRequest {
    message: String,
}

#[derive(Debug, Deserialize)]
struct OpenIdConfiguration {
    jwks_uri: String,
}

#[derive(Debug, Clone)]
struct JwtValidator {
    issuer: String,
    audience: String,
    jwks: Arc<JwkSet>,
}

#[derive(Debug, Deserialize)]
struct JwtClaims {
    exp: usize,
    sub: String,
    iss: String,
    aud: JwtAudience,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JwtAudience {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Default)]
struct EchoMarkerServer {
    cancelled: Arc<Mutex<BTreeSet<String>>>,
    authorization: Option<AuthorizationMode>,
}

#[derive(Debug, Clone)]
enum AuthorizationMode {
    Jwt(JwtValidator),
    Bearer(String),
}

impl EchoMarkerServer {
    fn with_authorization(mut self, authorization: AuthorizationMode) -> Self {
        self.authorization = Some(authorization);
        self
    }

    fn cancel_request(&self, id: String) {
        if let Ok(mut cancelled) = self.cancelled.lock() {
            cancelled.insert(id);
        }
    }

    fn is_cancelled(&self, id: &str) -> bool {
        self.cancelled
            .lock()
            .is_ok_and(|cancelled| cancelled.contains(id))
    }

    async fn delay_until_done_or_cancelled(&self, request_id: &str, delay_ms: u64) -> bool {
        let mut remaining = delay_ms;
        while remaining > 0 {
            if self.is_cancelled(request_id) {
                return false;
            }
            let chunk = remaining.min(25);
            tokio::time::sleep(Duration::from_millis(chunk)).await;
            remaining -= chunk;
        }
        !self.is_cancelled(request_id)
    }
}

#[mcp::mcp_server(name = "waltr-agent-test-mcp-server", version = env!("CARGO_PKG_VERSION"))]
impl EchoMarkerServer {
    #[instructions]
    fn instructions() -> Option<String> {
        std::env::var("WALTR_TEST_MCP_INSTRUCTIONS")
            .ok()
            .filter(|instructions| !instructions.trim().is_empty())
    }

    #[tool(name = "echo_marker", definition = echo_marker_tool_definition())]
    #[tool(
        name = "delayed_write_marker",
        definition = delayed_write_marker_tool_definition()
    )]
    #[tool(name = "emit_log_marker", definition = emit_log_marker_tool_definition())]
    #[tool(name = "progress_marker", definition = progress_marker_tool_definition())]
    async fn call_tool(
        &self,
        call: McpToolCall,
        context: ServerContext,
    ) -> ServerResult<McpToolResult> {
        match call.name.as_str() {
            "echo_marker" => match serde_json::from_value::<EchoMarkerRequest>(call.arguments) {
                Ok(arguments) => {
                    let mut text = arguments.value;
                    if let Some(marker) = trace_marker(&context, "tool") {
                        text.push('\n');
                        text.push_str(&marker);
                    }
                    Ok(McpToolResult::text(text))
                }
                Err(error) => Err(ServerError::invalid_params(error.to_string())),
            },
            "delayed_write_marker" => {
                let request_id = context
                    .request_id()
                    .map_or_else(|| call.name.clone(), ToString::to_string);
                match serde_json::from_value::<DelayedWriteRequest>(call.arguments) {
                    Ok(arguments) => {
                        if !self
                            .delay_until_done_or_cancelled(&request_id, arguments.delay_ms)
                            .await
                        {
                            return Err(ServerError::internal("request cancelled"));
                        }
                        if let Err(error) = std::fs::write(arguments.path, &arguments.value) {
                            return Err(ServerError::internal(error.to_string()));
                        }
                        Ok(McpToolResult::text(arguments.value))
                    }
                    Err(error) => Err(ServerError::invalid_params(error.to_string())),
                }
            }
            "emit_log_marker" => match serde_json::from_value::<EmitLogRequest>(call.arguments) {
                Ok(arguments) => {
                    if let Err(error) = context
                        .log(
                            "warning",
                            Some("waltr.test"),
                            json!({
                                "message": arguments.message,
                                "marker": "stdio-mcp-log-notification-818"
                            }),
                        )
                        .await
                    {
                        return Err(ServerError::internal(error.to_string()));
                    }
                    Ok(McpToolResult::text(arguments.message))
                }
                Err(error) => Err(ServerError::invalid_params(error.to_string())),
            },
            "progress_marker" => {
                match serde_json::from_value::<EchoMarkerRequest>(call.arguments) {
                    Ok(arguments) => {
                        let token = call
                            .progress_token
                            .ok_or_else(|| ServerError::invalid_params("missing progress token"))?;
                        if let Err(error) = context
                            .progress(
                                token,
                                1.0,
                                Some(2.0),
                                Some("stdio-mcp-progress-marker".to_owned()),
                            )
                            .await
                        {
                            return Err(ServerError::internal(error.to_string()));
                        }
                        Ok(McpToolResult::text(arguments.value))
                    }
                    Err(error) => Err(ServerError::invalid_params(error.to_string())),
                }
            }
            _ => Err(ServerError::invalid_params("unknown test tool")),
        }
    }

    #[log_level]
    #[allow(clippy::needless_pass_by_value)]
    fn set_log_level(level: String, _context: ServerContext) -> ServerResult<()> {
        set_log_level(&level)
    }

    #[prompt(name = "status_prompt", definition = status_prompt_definition())]
    #[allow(clippy::needless_pass_by_value)]
    fn status_prompt(
        arguments: Option<Value>,
        context: ServerContext,
    ) -> ServerResult<McpPromptResult> {
        get_prompt("status_prompt", arguments.as_ref(), &context)
    }

    #[prompt(name = "greet_prompt", definition = greet_prompt_definition())]
    #[allow(clippy::needless_pass_by_value)]
    fn greet_prompt(
        arguments: Option<Value>,
        context: ServerContext,
    ) -> ServerResult<McpPromptResult> {
        get_prompt("greet_prompt", arguments.as_ref(), &context)
    }

    #[prompt(name = "review.code", definition = review_code_prompt_definition())]
    #[allow(clippy::needless_pass_by_value)]
    fn review_code_prompt(
        arguments: Option<Value>,
        context: ServerContext,
    ) -> ServerResult<McpPromptResult> {
        get_prompt("review.code", arguments.as_ref(), &context)
    }

    #[resource(uri = "test://status", definition = status_resource_definition())]
    #[resource_template(
        uri_template = "test://items/{id}",
        definition = item_resource_template_definition()
    )]
    #[allow(clippy::needless_pass_by_value)]
    fn read_resource(uri: String, context: ServerContext) -> ServerResult<McpResourceResult> {
        read_resource(&uri, &context)
    }

    #[complete]
    #[allow(clippy::needless_pass_by_value)]
    fn complete(
        request: McpCompletionRequest,
        context: ServerContext,
    ) -> ServerResult<mcp::McpCompletionResult> {
        complete(request, &context)
    }

    #[notification(method = "notifications/cancelled")]
    #[allow(clippy::needless_pass_by_value)]
    fn handle_cancelled(&self, notification: JsonRpcNotification, _context: ServerContext) {
        if let Some(id) = notification
            .params
            .as_ref()
            .and_then(|params| params.get("requestId"))
            .map(request_id_string)
        {
            self.cancel_request(id);
        }
    }

    #[token_validator]
    #[allow(clippy::needless_pass_by_value)]
    fn validate_token(
        &self,
        token: McpAccessToken,
        _context: McpAuthorizationContext,
    ) -> McpTokenValidation {
        match &self.authorization {
            Some(AuthorizationMode::Jwt(validator)) => validate_jwt_token(validator, &token.0),
            Some(AuthorizationMode::Bearer(required_token)) if token.0 == *required_token => {
                McpTokenValidation::Authorized {
                    principal_id: McpPrincipalId::new("waltr-agent-test-bearer")
                        .expect("static test principal is valid"),
                    expires_at: None,
                }
            }
            Some(AuthorizationMode::Bearer(_)) => McpTokenValidation::Unauthorized {
                error_description: Some("invalid test token".to_owned()),
            },
            None => McpTokenValidation::Unauthorized {
                error_description: Some("test server authorization is not configured".to_owned()),
            },
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--http") {
        let bind = args.next().unwrap_or_else(|| "127.0.0.1:0".to_owned());
        let address_file = args.next();
        serve_http(&bind, address_file.as_deref()).await?;
        return Ok(());
    }

    serve_stdio(Arc::new(EchoMarkerServer::default())).await?;
    Ok(())
}

async fn serve_http(
    bind: &str,
    address_file: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let address = listener.local_addr()?;
    if let Some(path) = address_file {
        std::fs::write(path, address.to_string())?;
    }
    let resource =
        std::env::var("WALTR_TEST_MCP_RESOURCE").unwrap_or_else(|_| format!("http://{address}/mcp"));
    let router = if let Some(validator) = jwt_validator_from_env().await? {
        let issuer = validator.issuer.clone();
        let metadata = EchoMarkerServer::protected_resource_metadata(resource.clone(), [issuer])
            .with_scopes(["openid", "email", "profile"])
            .with_resource_name("Waltr protected MCP test server");
        EchoMarkerServer::default()
            .with_authorization(AuthorizationMode::Jwt(validator))
            .streamable_http_router_with_auth(metadata)?
    } else if let Ok(required_token) = std::env::var("WALTR_TEST_MCP_REQUIRED_BEARER") {
        let issuer = std::env::var("WALTR_TEST_MCP_OAUTH_ISSUER")
            .unwrap_or_else(|_| "https://auth.example.test".to_owned());
        let metadata = EchoMarkerServer::protected_resource_metadata(resource, [issuer]);
        EchoMarkerServer::default()
            .with_authorization(AuthorizationMode::Bearer(required_token))
            .streamable_http_router_with_auth(metadata)?
    } else {
        EchoMarkerServer::default().streamable_http_router()
    };
    axum::serve(listener, router).await?;
    Ok(())
}

async fn jwt_validator_from_env() -> Result<Option<JwtValidator>, Box<dyn std::error::Error>> {
    let issuer = match std::env::var("WALTR_TEST_MCP_OAUTH_ISSUER") {
        Ok(issuer) if !issuer.trim().is_empty() => issuer,
        _ => return Ok(None),
    };
    let audience = std::env::var("WALTR_TEST_MCP_OAUTH_AUDIENCE")
        .or_else(|_| std::env::var("WALTR_TEST_MCP_OAUTH_CLIENT_ID"))
        .unwrap_or_else(|_| "waltr-test".to_owned());
    let jwks_uri = match std::env::var("WALTR_TEST_MCP_OAUTH_JWKS_URI") {
        Ok(uri) if !uri.trim().is_empty() => uri,
        _ => {
            let issuer = issuer.trim_end_matches('/');
            let config: OpenIdConfiguration =
                reqwest::get(format!("{issuer}/.well-known/openid-configuration"))
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
            config.jwks_uri
        }
    };
    let jwks: JwkSet = reqwest::get(jwks_uri)
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(Some(JwtValidator {
        issuer,
        audience,
        jwks: Arc::new(jwks),
    }))
}

fn validate_jwt_token(validator: &JwtValidator, token: &str) -> McpTokenValidation {
    match decode_validated_jwt(validator, token) {
        Ok((principal_id, expires_at)) => McpTokenValidation::Authorized {
            principal_id,
            expires_at: Some(expires_at),
        },
        Err(message) => McpTokenValidation::Unauthorized {
            error_description: Some(message),
        },
    }
}

fn decode_validated_jwt(
    validator: &JwtValidator,
    token: &str,
) -> Result<(McpPrincipalId, SystemTime), String> {
    let header = decode_header(token).map_err(|error| format!("invalid JWT header: {error}"))?;
    let key = validator
        .jwks
        .keys
        .iter()
        .find(|jwk| {
            header
                .kid
                .as_deref()
                .is_some_and(|kid| jwk.common.key_id.as_deref() == Some(kid))
        })
        .or_else(|| validator.jwks.keys.first())
        .ok_or_else(|| "authorization server JWKS is empty".to_owned())?;
    let decoding_key = DecodingKey::from_jwk(key)
        .map_err(|error| format!("invalid authorization server JWK: {error}"))?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[validator.issuer.as_str()]);
    validation.set_audience(&[validator.audience.as_str()]);
    let token = decode::<JwtClaims>(token, &decoding_key, &validation)
        .map_err(|error| format!("invalid JWT: {error}"))?;

    if token.claims.iss != validator.issuer {
        return Err("JWT issuer mismatch".to_owned());
    }
    if !token.claims.aud.contains(&validator.audience) {
        return Err("JWT audience mismatch".to_owned());
    }
    let principal_id = McpPrincipalId::new(token.claims.sub)
        .map_err(|error| format!("invalid JWT subject: {error}"))?;
    let expires_at = UNIX_EPOCH
        .checked_add(Duration::from_secs(
            u64::try_from(token.claims.exp).map_err(|_| "JWT expiry is out of range")?,
        ))
        .ok_or_else(|| "JWT expiry is out of range".to_owned())?;

    Ok((principal_id, expires_at))
}

impl JwtAudience {
    fn contains(&self, audience: &str) -> bool {
        match self {
            Self::One(value) => value == audience,
            Self::Many(values) => values.iter().any(|value| value == audience),
        }
    }
}

fn echo_marker_tool_definition() -> McpToolDefinition {
    tool_definition(
        "echo_marker",
        "Echo a deterministic marker.",
        string_schema("value"),
    )
}

fn delayed_write_marker_tool_definition() -> McpToolDefinition {
    tool_definition(
        "delayed_write_marker",
        "Write a marker after a delay.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "value": { "type": "string" },
                "delay_ms": { "type": "integer" }
            },
            "required": ["path", "value", "delay_ms"]
        }),
    )
}

fn emit_log_marker_tool_definition() -> McpToolDefinition {
    tool_definition(
        "emit_log_marker",
        "Emit a deterministic MCP log message.",
        string_schema("message"),
    )
}

fn progress_marker_tool_definition() -> McpToolDefinition {
    tool_definition(
        "progress_marker",
        "Emit progress and echo a deterministic marker.",
        string_schema("value"),
    )
}

fn tool_definition(name: &str, description: &str, input_schema: Value) -> McpToolDefinition {
    McpToolDefinition {
        name: name.to_owned(),
        title: None,
        description: Some(description.to_owned()),
        icons: Vec::new(),
        input_schema,
        output_schema: None,
        task_support: None,
        annotations: None,
    }
}

fn string_schema(property: &str) -> Value {
    json!({
        "type": "object",
        "properties": { property: { "type": "string" } },
        "required": [property]
    })
}

fn trace_marker(context: &ServerContext, label: &str) -> Option<String> {
    context
        .meta_value("traceparent")
        .and_then(Value::as_str)
        .map(|_| format!("trace:{label}:present"))
}

fn set_log_level(level: &str) -> ServerResult<()> {
    if let Ok(path) = std::env::var("WALTR_TEST_MCP_LOG_LEVEL_FILE")
        && let Err(error) = std::fs::write(path, level)
    {
        return Err(ServerError::internal(error.to_string()));
    }
    Ok(())
}

fn status_prompt_definition() -> McpPromptDefinition {
    prompt_definition(
        "status_prompt",
        "Return a deterministic status prompt.",
        Vec::new(),
    )
}

fn greet_prompt_definition() -> McpPromptDefinition {
    prompt_definition(
        "greet_prompt",
        "Greet a named test subject.",
        vec![McpPromptArgument {
            name: "name".to_owned(),
            description: Some("Name to greet".to_owned()),
            required: true,
        }],
    )
}

fn review_code_prompt_definition() -> McpPromptDefinition {
    prompt_definition(
        "review.code",
        "Spec-valid prompt name outside Waltr slash syntax.",
        Vec::new(),
    )
}

fn prompt_definition(
    name: &str,
    description: &str,
    arguments: Vec<McpPromptArgument>,
) -> McpPromptDefinition {
    McpPromptDefinition {
        name: name.to_owned(),
        title: None,
        description: Some(description.to_owned()),
        icons: Vec::new(),
        arguments,
        meta: None,
    }
}

fn get_prompt(
    name: &str,
    arguments: Option<&Value>,
    context: &ServerContext,
) -> ServerResult<McpPromptResult> {
    let mut text = match name {
        "status_prompt" => "stdio-mcp-prompt-status-313".to_owned(),
        "greet_prompt" => {
            let Some(name) = arguments
                .and_then(|arguments| arguments.get("name"))
                .and_then(Value::as_str)
            else {
                return Err(ServerError::invalid_params("missing prompt argument: name"));
            };
            format!("hello {name} from stdio-mcp-prompt-414")
        }
        "review.code" => "stdio-mcp-prompt-dotted-515".to_owned(),
        _ => return Err(ServerError::invalid_params("unknown test prompt")),
    };
    if let Some(marker) = trace_marker(context, "prompt") {
        text.push('\n');
        text.push_str(&marker);
    }
    Ok(McpPromptResult::text_user(text))
}

fn status_resource_definition() -> McpResourceDefinition {
    McpResourceDefinition {
        uri: "test://status".to_owned(),
        name: "status_resource".to_owned(),
        title: Some("Status Resource".to_owned()),
        description: Some("Return deterministic status resource text".to_owned()),
        mime_type: Some("text/plain".to_owned()),
        icons: Vec::new(),
        size: Some(28),
        annotations: None,
        meta: None,
    }
}

fn item_resource_template_definition() -> McpResourceTemplateDefinition {
    McpResourceTemplateDefinition {
        uri_template: "test://items/{id}".to_owned(),
        name: "item_resource".to_owned(),
        title: Some("Item Resource".to_owned()),
        description: Some("Return deterministic templated item resource text".to_owned()),
        mime_type: Some("text/plain".to_owned()),
        icons: Vec::new(),
        annotations: None,
        meta: None,
    }
}

fn read_resource(uri: &str, context: &ServerContext) -> ServerResult<McpResourceResult> {
    let mut text = match uri {
        "test://status" => "stdio-mcp-resource-status-616",
        "test://items/42" => "stdio-mcp-resource-template-item-42-717",
        _ => return Err(ServerError::invalid_params("unknown test resource")),
    }
    .to_owned();
    if let Some(marker) = trace_marker(context, "resource") {
        text.push('\n');
        text.push_str(&marker);
    }
    Ok(McpResourceResult::text(uri, "text/plain", text))
}

#[allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]
fn complete(
    request: McpCompletionRequest,
    context: &ServerContext,
) -> ServerResult<mcp::McpCompletionResult> {
    let mut values = if request.uri_template == "test://items/{id}" && request.argument_name == "id"
    {
        ["42", "420", "4242"]
            .into_iter()
            .filter(|value| value.starts_with(&request.argument_value))
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if trace_marker(context, "completion").is_some() {
        values.push("trace:completion:present".to_owned());
    }
    Ok(mcp::McpCompletionResult {
        total: Some(values.len() as u64),
        values,
        has_more: false,
    })
}

fn request_id_string(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}
