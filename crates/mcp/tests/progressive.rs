//! Integration tests for the exported `mcp::progressive_server` macro.

// Fixtures intentionally exercise generated receiver, owned-input, and fallible handler shapes.
#![allow(
    clippy::literal_string_with_formatting_args,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::unnecessary_wraps,
    clippy::unused_self,
    clippy::useless_vec
)]

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use mcp::{
    McpAccessToken, McpProtectedResourceMetadata, McpTokenValidation, McpToolResult,
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
    tool(
        name = "releases",
        description = "Release inventory actions.",
        annotations = release_tool_annotations(),
        namespace(
            releases,
            description = "Query and inspect product releases. Use these to ground any statement about what shipped and when."
        ),
        runbook_uri_template = "playbooks://{runbook}"
    ),
    tool(
        name = "policy",
        description = "Release policy actions.",
        namespace(
            policy,
            description = "Org release policies and compliance checks. Consult before recommending or performing any promotion."
        ),
        runbook_uri_template = "playbooks://policy/{runbook}"
    ),
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
    #[action(tool = "releases", name = "list")]
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
    #[action(
        tool = "releases",
        name = "releases.get",
        resource_template = "releases://{id}"
    )]
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
    #[action(tool = "policy", name = "promotion", resource = "policy://promotion")]
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
    #[action(tool = "policy", name = "policy.check")]
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

    /// Return a complete MCP result for consumers that need semantic content.
    ///
    /// This exercises non-text content, metadata, extensions, and request context.
    #[action(tool = "releases", name = "semantic", resource = "releases://semantic")]
    fn semantic_result(&self, context: ServerContext) -> ServerResult<McpToolResult> {
        Ok(McpToolResult::new(json!({
            "content": [
                { "type": "text", "text": "semantic release result" },
                { "type": "image", "data": "aW1hZ2U=", "mimeType": "image/png" }
            ],
            "structuredContent": {
                "items": [{ "id": "v1.4.2" }, { "id": "v1.4.1" }],
                "client": context
                    .meta_value("io.modelcontextprotocol/clientInfo")
                    .and_then(|value| value.get("name"))
                    .cloned()
            },
            "isError": false,
            "_meta": { "trace": "release-semantic" },
            "x-release-extension": { "preserved": true }
        })))
    }

    /// Return a semantic MCP action failure as a successful tool result.
    #[action(tool = "releases", name = "semantic-error")]
    fn semantic_error(&self) -> ServerResult<McpToolResult> {
        Ok(McpToolResult::new(json!({
            "content": [{ "type": "text", "text": "release not found" }],
            "isError": true,
            "_meta": { "kind": "not-found" }
        })))
    }

    /// Return malformed semantic output to exercise protocol error conversion.
    #[action(tool = "releases", name = "semantic-invalid")]
    fn semantic_invalid(&self) -> ServerResult<McpToolResult> {
        Ok(McpToolResult::new(json!({ "content": "invalid" })))
    }

    /// Return enough JSON values to exercise the jq fan-out bound.
    #[action(tool = "releases", name = "fan-out")]
    fn fan_out(&self) -> ServerResult<Value> {
        Ok(json!((0..257).collect::<Vec<_>>()))
    }

    /// Promote a release candidate to production with required pre-checks.
    ///
    /// Load this runbook when the user wants an end-to-end production
    /// promotion procedure rather than a single release fact.
    #[runbook(tool = "policy", name = "promote-to-production")]
    fn promote_runbook(&self) -> ServerResult<String> {
        Ok("## Promote to production\n1. Load release details.\n2. Check policy.\n3. Confirm rollback plan.\n".to_owned())
    }

    /// Roll back production to the previous known-good release.
    ///
    /// Load this runbook when the user reports production impact or asks for a
    /// rollback playbook.
    #[runbook(tool = "releases", name = "rollback-release")]
    fn rollback_runbook(&self) -> ServerResult<String> {
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

fn release_tool_annotations() -> Value {
    json!({
        "readOnlyHint": true,
        "destructiveHint": false
    })
}

#[test]
fn progressive_tool_results_keep_text_exact_and_structure_non_objects() {
    let result =
        mcp::progressive::tool_result(json!("rendered text"), None).expect("string tool result");
    assert_eq!(result.raw["content"][0]["text"], "rendered text");
    assert_eq!(
        result.raw["structuredContent"],
        json!({ "result": "rendered text" })
    );

    let result = mcp::progressive::tool_result(json!(["v1.4.2", "v1.4.1"]), None)
        .expect("array tool result");
    assert_eq!(
        result.raw["structuredContent"],
        json!({ "result": ["v1.4.2", "v1.4.1"] })
    );
}

#[test]
fn progressive_semantic_results_preserve_envelopes_and_filter_only_structure() {
    let raw = json!({
        "content": [
            { "type": "text", "text": "original text" },
            { "type": "image", "data": "aW1hZ2U=", "mimeType": "image/png" }
        ],
        "structuredContent": { "items": [{ "id": 1 }, { "id": 2 }] },
        "isError": true,
        "_meta": { "trace": "abc" },
        "x-extension": [1, 2, 3]
    });
    let result = mcp::progressive::action_tool_result(McpToolResult::new(raw.clone()), None)
        .expect("semantic tool result");
    assert_eq!(result.raw, raw);

    let result =
        mcp::progressive::action_tool_result(McpToolResult::new(raw.clone()), Some(".items[].id"))
            .expect("filtered semantic result");
    assert_eq!(result.raw["structuredContent"], json!({ "result": [1, 2] }));
    assert_eq!(result.raw["content"], raw["content"]);
    assert_eq!(result.raw["isError"], true);
    assert_eq!(result.raw["_meta"], raw["_meta"]);
    assert_eq!(result.raw["x-extension"], raw["x-extension"]);

    let result =
        mcp::progressive::action_tool_result(McpToolResult::new(raw), Some(".items[0].id"))
            .expect("scalar semantic filter");
    assert_eq!(result.raw["structuredContent"], json!({ "result": 1 }));
}

#[test]
fn progressive_semantic_results_reject_invalid_shapes_and_filter_fan_out() {
    let invalid = [
        json!([]),
        json!({ "content": {} }),
        json!({ "content": ["text"] }),
        json!({ "content": [{}] }),
        json!({ "content": [{ "type": 1 }] }),
        json!({ "content": [], "isError": "yes" }),
        json!({ "content": [], "structuredContent": [] }),
    ];
    for raw in invalid {
        let error = mcp::progressive::action_tool_result(McpToolResult::new(raw), None)
            .expect_err("invalid semantic shape");
        assert!(format!("{error:?}").contains("MCP tool result"));
    }

    let missing_structure = mcp::progressive::action_tool_result(
        McpToolResult::new(json!({ "content": [] })),
        Some("."),
    )
    .expect_err("filter requires structured content");
    assert!(format!("{missing_structure:?}").contains("without `structuredContent`"));

    let outputs = (0..257).collect::<Vec<_>>();
    let overflow = mcp::progressive::tool_result(json!(outputs), Some(".[]"))
        .expect_err("fan-out must be bounded");
    assert!(format!("{overflow:?}").contains("more than 256 results"));
}

#[test]
fn progressive_semantic_resource_conversion_requires_non_error_structure() {
    let result = mcp::progressive::action_resource_result(
        "test://semantic".to_owned(),
        McpToolResult::new(json!({
            "content": [],
            "structuredContent": { "ok": true }
        })),
    )
    .expect("semantic resource result");
    assert!(result.raw.to_string().contains("\\\"ok\\\": true"));

    for raw in [
        json!({ "content": [] }),
        json!({ "content": [], "structuredContent": {}, "isError": true }),
    ] {
        mcp::progressive::action_resource_result(
            "test://semantic".to_owned(),
            McpToolResult::new(raw),
        )
        .expect_err("invalid semantic resource output");
    }
}

#[tokio::test]
async fn progressive_server_isolates_tools_help_runbooks_and_resources() -> mcp::Result<()> {
    let server = RunningServer::start_plain(ReleaseServer::default()).await?;

    let discover = post_json(&server.endpoint, discover_body("discover")).await?;
    assert_eq!(discover.status, StatusCode::OK);
    assert_eq!(
        discover.body["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "release-desk"
    );
    assert_eq!(
        discover.body["result"]["capabilities"]["tools"]["listChanged"],
        false
    );
    assert!(discover.body["result"]["capabilities"]["prompts"].is_object());
    assert!(discover.body["result"]["capabilities"]["resources"].is_object());
    assert!(
        discover.body["result"]["instructions"]
            .as_str()
            .is_some_and(
                |instructions| instructions.contains("`releases`: Release inventory actions.")
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
        2
    );
    let releases_tool = tool_named(&tools.body, "releases");
    let policy_tool = tool_named(&tools.body, "policy");
    assert_eq!(releases_tool["annotations"]["readOnlyHint"], true);
    assert_eq!(releases_tool["annotations"]["destructiveHint"], false);
    assert!(policy_tool.get("annotations").is_none());
    assert_array_contains(
        &releases_tool["inputSchema"]["properties"]["action"]["enum"],
        "help.releases",
    );
    assert_array_contains(
        &releases_tool["inputSchema"]["properties"]["action"]["enum"],
        "runbooks.load",
    );
    assert_array_contains(
        &policy_tool["inputSchema"]["properties"]["action"]["enum"],
        "policy.check",
    );
    assert_array_excludes(
        &releases_tool["inputSchema"]["properties"]["action"]["enum"],
        "policy.check",
    );
    assert_array_excludes(
        &policy_tool["inputSchema"]["properties"]["action"]["enum"],
        "releases.get",
    );
    let input_options = releases_tool["inputSchema"]["properties"]["input"]["oneOf"]
        .as_array()
        .expect("input oneOf");
    assert!(
        input_options
            .iter()
            .any(|schema| schema.to_string().contains("Release id."))
    );

    let root_help = call_tool(&server.endpoint, "releases", json!({ "action": "help" })).await?;
    assert_eq!(root_help["structuredContent"]["topic"], "help");
    assert_eq!(
        root_help["structuredContent"]["actions"][0]["action"],
        "list"
    );
    assert_eq!(
        root_help["structuredContent"]["namespaces"][0]["help_action"],
        "help.releases"
    );
    assert_eq!(root_help["structuredContent"]["runbooks"]["count"], 1);
    assert_eq!(
        root_help["structuredContent"]["description"],
        "Release inventory actions."
    );

    let policy_help = call_tool(
        &server.endpoint,
        "policy",
        json!({ "action": "help.policy" }),
    )
    .await?;
    assert_eq!(policy_help["structuredContent"]["topic"], "help.policy");
    assert_eq!(
        policy_help["structuredContent"]["actions"][0]["action"],
        "policy.check"
    );
    assert_eq!(
        policy_help["structuredContent"]["actions"][0]["description"],
        "Check release policy against one release and target environment."
    );
    assert!(
        policy_help["structuredContent"]["actions"][0]["guidance"]
            .as_str()
            .is_some_and(|guidance| guidance.contains("deploy decision"))
    );

    let runbooks_help = call_tool(
        &server.endpoint,
        "policy",
        json!({ "action": "help.runbooks" }),
    )
    .await?;
    assert_eq!(
        runbooks_help["structuredContent"]["resource_template"],
        "playbooks://policy/{runbook}"
    );
    assert_eq!(
        runbooks_help["structuredContent"]["runbooks"][0]["description"],
        "Promote a release candidate to production with required pre-checks."
    );
    assert_eq!(
        runbooks_help["structuredContent"]["runbooks"][0]["resource"],
        "playbooks://policy/promote-to-production"
    );

    let filtered = call_tool(
        &server.endpoint,
        "releases",
        json!({
            "action": "list",
            "input": { "status": "released", "limit": 2 },
            "filter": ".[].id"
        }),
    )
    .await?;
    assert_eq!(
        filtered["structuredContent"],
        json!({ "result": ["v1.4.2", "v1.4.1"] })
    );

    let semantic = call_tool(
        &server.endpoint,
        "releases",
        json!({ "action": "semantic" }),
    )
    .await?;
    assert_eq!(semantic["isError"], false);
    assert_eq!(semantic["content"][1]["type"], "image");
    assert_eq!(semantic["_meta"]["trace"], "release-semantic");
    assert_eq!(semantic["x-release-extension"]["preserved"], true);
    assert_eq!(semantic["structuredContent"]["client"], "progressive-tests");

    let semantic_filtered = call_tool(
        &server.endpoint,
        "releases",
        json!({ "action": "semantic", "filter": ".items[0].id" }),
    )
    .await?;
    assert_eq!(
        semantic_filtered["structuredContent"],
        json!({ "result": "v1.4.2" })
    );
    assert_eq!(semantic_filtered["content"], semantic["content"]);
    assert_eq!(semantic_filtered["_meta"], semantic["_meta"]);

    let semantic_error = call_tool(
        &server.endpoint,
        "releases",
        json!({ "action": "semantic-error" }),
    )
    .await?;
    assert_eq!(semantic_error["isError"], true);
    assert!(semantic_error["content"][0]["text"] == "release not found");

    let semantic_invalid = call_tool_response(
        &server.endpoint,
        "releases",
        json!({ "action": "semantic-invalid" }),
    )
    .await?;
    assert_eq!(semantic_invalid.body["error"]["code"], -32602);

    let fan_out = call_tool_response(
        &server.endpoint,
        "releases",
        json!({ "action": "fan-out", "filter": ".[]" }),
    )
    .await?;
    assert_eq!(fan_out.body["error"]["code"], -32602);

    let runbook = call_tool(
        &server.endpoint,
        "policy",
        json!({
            "action": "runbooks.load",
            "input": { "runbook": "promote-to-production" }
        }),
    )
    .await?;
    assert!(
        runbook["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("## Promote to production"))
    );
    assert!(
        runbook["structuredContent"]["markdown"]
            .as_str()
            .is_some_and(|text| text.contains("Check policy"))
    );
    assert_eq!(
        runbook["structuredContent"]["runbook"],
        "promote-to-production"
    );

    let isolated_runbook = call_tool_response(
        &server.endpoint,
        "releases",
        json!({
            "action": "runbooks.load",
            "input": { "runbook": "promote-to-production" }
        }),
    )
    .await?;
    assert!(
        isolated_runbook.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unknown runbook"))
    );

    let isolated_action = call_tool_response(
        &server.endpoint,
        "policy",
        json!({ "action": "releases.get", "input": { "id": "v1.4.2" } }),
    )
    .await?;
    assert!(
        isolated_action.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unknown progressive action"))
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
        "playbooks://{runbook}"
    );
    assert_eq!(
        templates.body["result"]["resourceTemplates"][2]["uriTemplate"],
        "playbooks://policy/{runbook}"
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

    let semantic_resource = post_json(
        &server.endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "semantic-resource",
            "method": "resources/read",
            "params": { "uri": "releases://semantic" }
        }),
    )
    .await?;
    let semantic_resource: Value = serde_json::from_str(
        semantic_resource.body["result"]["contents"][0]["text"]
            .as_str()
            .expect("semantic resource text"),
    )?;
    assert_eq!(semantic_resource["items"][0]["id"], "v1.4.2");

    let runbook_resource = post_json(
        &server.endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "runbook-resource",
            "method": "resources/read",
            "params": { "uri": "playbooks://rollback-release" }
        }),
    )
    .await?;
    assert!(
        runbook_resource.body["result"]["contents"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("## Rollback release"))
    );

    let overlapping_runbook_resource = post_json(
        &server.endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "overlapping-runbook-resource",
            "method": "resources/read",
            "params": { "uri": "playbooks://policy/promote-to-production" }
        }),
    )
    .await?;
    assert!(
        overlapping_runbook_resource.body["result"]["contents"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("## Promote to production"))
    );

    let missing_input = post_json(
        &server.endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "missing-input",
            "method": "tools/call",
            "params": { "name": "releases", "arguments": { "action": "releases.get" } }
        }),
    )
    .await?;
    assert!(
        missing_input.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("help.releases"))
    );
    assert_eq!(missing_input.body["error"]["code"], -32602);

    let unknown_action = post_json(
        &server.endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "unknown-action",
            "method": "tools/call",
            "params": { "name": "releases", "arguments": { "action": "releases.delete" } }
        }),
    )
    .await?;
    assert!(
        unknown_action.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("call `help`"))
    );
    assert_eq!(unknown_action.body["error"]["code"], -32602);

    let unknown_field = call_tool_response(
        &server.endpoint,
        "releases",
        json!({ "action": "help", "unexpected": true }),
    )
    .await?;
    assert_eq!(unknown_field.body["error"]["code"], -32602);

    let unknown_runbook_field = call_tool_response(
        &server.endpoint,
        "releases",
        json!({
            "action": "runbooks.load",
            "input": { "runbook": "rollback-release", "unexpected": true }
        }),
    )
    .await?;
    assert_eq!(unknown_runbook_field.body["error"]["code"], -32602);

    let null_filter = call_tool(
        &server.endpoint,
        "releases",
        json!({ "action": "help", "filter": null }),
    )
    .await?;
    assert_eq!(null_filter["structuredContent"]["topic"], "help");

    let invalid_filter = call_tool_response(
        &server.endpoint,
        "releases",
        json!({ "action": "help", "filter": ".]" }),
    )
    .await?;
    assert_eq!(invalid_filter.body["error"]["code"], -32602);

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
        .header("content-type", "application/json")
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .header("mcp-method", "server/discover")
        .json(&request_body(discover_body("unauthorized")))
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
        .header("content-type", "application/json")
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .header("mcp-method", "server/discover")
        .bearer_auth("release-desk-secret")
        .json(&request_body(discover_body("authorized")))
        .send()
        .await?;
    assert_eq!(authorized.status(), StatusCode::OK);
    let body = sse_response(authorized).await?;
    assert_eq!(body["result"]["supportedVersions"][0], MCP_PROTOCOL_VERSION);
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

async fn call_tool(endpoint: &str, name: &str, arguments: Value) -> mcp::Result<Value> {
    let response = call_tool_response(endpoint, name, arguments).await?;
    Ok(response.body["result"].clone())
}

async fn call_tool_response(
    endpoint: &str,
    name: &str,
    arguments: Value,
) -> mcp::Result<JsonResponse> {
    let response = post_json(
        endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "progressive-tool",
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }),
    )
    .await?;
    Ok(response)
}

async fn post_json(endpoint: &str, body: Value) -> mcp::Result<JsonResponse> {
    let client = reqwest::Client::new();
    let body = request_body(body);
    let method = body["method"].as_str().unwrap_or_default();
    let mut request = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .header("mcp-method", method)
        .json(&body);
    if let Some(name) = request_name(method, &body) {
        request = request.header("mcp-name", name);
    }
    let response = request.send().await?;
    let status = response.status();
    let body = sse_response(response).await?;
    Ok(JsonResponse { status, body })
}

fn request_body(mut body: Value) -> Value {
    if body.get("params").is_none() {
        body["params"] = json!({});
    }
    body["params"]["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": MCP_PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "progressive-tests", "version": "1" }
    });
    body
}

fn request_name<'a>(method: &str, body: &'a Value) -> Option<&'a str> {
    match method {
        "tools/call" | "prompts/get" => body["params"]["name"].as_str(),
        "resources/read" => body["params"]["uri"].as_str(),
        _ => None,
    }
}

async fn sse_response(response: reqwest::Response) -> mcp::Result<Value> {
    let text = response.text().await?;
    let data = text
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("data: "))
        .ok_or_else(|| mcp::Error::protocol("MCP SSE response omitted data"))?;
    Ok(serde_json::from_str(data)?)
}

fn discover_body(id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "server/discover"
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

fn assert_array_excludes(values: &Value, excluded: &str) {
    assert!(
        values
            .as_array()
            .expect("array")
            .iter()
            .all(|value| value.as_str() != Some(excluded)),
        "expected array to exclude {excluded}, got {values}"
    );
}

fn tool_named<'a>(body: &'a Value, name: &str) -> &'a Value {
    body["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("missing tool {name}"))
}
