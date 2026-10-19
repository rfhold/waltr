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
struct ListRunsInput {
    repository: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetReleaseInput {
    /// Release id.
    id: String,
}

#[derive(JsonSchema)]
#[expect(
    dead_code,
    reason = "used only to generate a schema regression fixture"
)]
enum SchemaFixtureState {
    Ready,
    Paused,
}

#[derive(JsonSchema)]
#[expect(
    dead_code,
    reason = "used only to generate a schema regression fixture"
)]
struct SchemaFixtureInput {
    state: SchemaFixtureState,
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
        namespace(
            name = "alert-rule",
            description = "Query alert rules associated with releases."
        ),
    ),
    tool(
        name = "policy",
        description = "Release policy actions.",
        namespace(
            policy,
            description = "Org release policies and compliance checks. Consult before recommending or performing any promotion."
        ),
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

    /// Exercise identical input schemas on different actions.
    #[action(tool = "releases", name = "get-copy")]
    fn get_copy(&self, input: GetReleaseInput) -> ServerResult<Value> {
        Ok(json!({ "id": input.id }))
    }

    /// Exercise overlapping optional input schemas.
    #[action(tool = "releases", name = "list-runs")]
    fn list_runs(&self, input: ListRunsInput) -> ServerResult<Value> {
        Ok(json!({ "repository": input.repository, "limit": input.limit }))
    }

    /// Exercise nullable typed input after envelope parsing.
    #[action(tool = "releases", name = "nullable")]
    fn nullable(&self, input: Option<String>) -> ServerResult<Value> {
        Ok(json!(input))
    }

    /// Exercise a typed input whose only schema value is null.
    #[action(tool = "releases", name = "unit")]
    fn unit(&self, _input: ()) -> ServerResult<Value> {
        Ok(Value::Null)
    }

    /// List alert rules associated with releases.
    #[action(tool = "releases", name = "alert-rule.list")]
    fn list_alert_rules(&self) -> ServerResult<Value> {
        Ok(json!([{ "name": "release-health" }]))
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
fn progressive_filters_load_boolean_literals_and_standard_definitions() {
    let result = mcp::progressive::tool_result(
        json!({ "running": false, "items": [1, 2, 3] }),
        Some("select(.running == false) | .items | map(. + 1) | length"),
    )
    .expect("standard jq definitions should compile and evaluate");
    assert_eq!(result.raw["structuredContent"], json!({ "result": 3 }));
}

#[test]
fn progressive_semantic_results_preserve_unfiltered_envelopes_and_json_values() {
    for structured_content in [
        json!({ "ok": true }),
        json!([1, 2]),
        json!("value"),
        json!(42),
        json!(true),
        json!(null),
    ] {
        let raw = json!({
            "content": [{ "type": "text", "text": "original" }],
            "structuredContent": structured_content,
            "_meta": { "trace": "abc" },
            "x-extension": true
        });
        let result = mcp::progressive::action_tool_result(McpToolResult::new(raw.clone()), None)
            .expect("semantic tool result");
        assert_eq!(result.raw, raw);
    }
}

#[test]
fn progressive_semantic_filters_synchronize_text_and_preserve_extensions() {
    let raw = json!({
        "content": [
            { "type": "image", "data": "aW1hZ2U=", "mimeType": "image/png", "x-image": 1 },
            { "type": "text", "text": "original text", "x-text": { "preserved": true } },
            { "type": "audio", "data": "YXVkaW8=", "mimeType": "audio/wav", "x-audio": 2 },
            { "type": "text", "text": "stale text", "x-stale": true },
            { "type": "resource", "resource": { "uri": "test://item", "text": "body" } }
        ],
        "structuredContent": { "items": [{ "id": 1 }, { "id": 2 }] },
        "isError": false,
        "_meta": { "trace": "abc" },
        "x-extension": [1, 2, 3]
    });
    let result =
        mcp::progressive::action_tool_result(McpToolResult::new(raw.clone()), Some(".items[].id"))
            .expect("filtered semantic result");
    assert_eq!(result.raw["structuredContent"], json!([1, 2]));
    assert_eq!(
        result.raw["content"],
        json!([
            { "type": "image", "data": "aW1hZ2U=", "mimeType": "image/png", "x-image": 1 },
            { "type": "text", "text": "[1,2]", "x-text": { "preserved": true } },
            { "type": "audio", "data": "YXVkaW8=", "mimeType": "audio/wav", "x-audio": 2 },
            { "type": "resource", "resource": { "uri": "test://item", "text": "body" } }
        ])
    );
    assert_eq!(result.raw["isError"], false);
    assert_eq!(result.raw["_meta"], raw["_meta"]);
    assert_eq!(result.raw["x-extension"], raw["x-extension"]);

    let no_text = json!({
        "content": [
            { "type": "image", "data": "aW1hZ2U=", "mimeType": "image/png" },
            { "type": "audio", "data": "YXVkaW8=", "mimeType": "audio/wav" }
        ],
        "structuredContent": { "value": "quoted" }
    });
    let result =
        mcp::progressive::action_tool_result(McpToolResult::new(no_text.clone()), Some(".value"))
            .expect("filtered semantic result without text");
    assert_eq!(result.raw["structuredContent"], "quoted");
    assert_eq!(result.raw["content"][0]["text"], "\"quoted\"");
    assert_eq!(result.raw["content"][1], no_text["content"][0]);
    assert_eq!(result.raw["content"][2], no_text["content"][1]);
}

#[test]
fn progressive_semantic_filters_store_every_exact_json_result_shape() {
    for filtered in [
        json!({ "ok": true }),
        json!([1, 2]),
        json!("value"),
        json!(42),
        json!(true),
        json!(null),
    ] {
        let result = mcp::progressive::action_tool_result(
            McpToolResult::new(json!({
                "content": [{ "type": "text", "text": "stale" }],
                "structuredContent": { "value": filtered }
            })),
            Some(".value"),
        )
        .expect("filtered semantic value");
        assert_eq!(result.raw["structuredContent"], filtered);
        assert_eq!(
            result.raw["content"][0]["text"],
            serde_json::to_string(&filtered).expect("JSON value")
        );
    }

    let zero = mcp::progressive::action_tool_result(
        McpToolResult::new(json!({
            "content": [],
            "structuredContent": []
        })),
        Some(".[]"),
    )
    .expect("zero filter outputs");
    assert_eq!(zero.raw["structuredContent"], Value::Null);
    assert_eq!(zero.raw["content"][0]["text"], "null");
}

#[test]
fn progressive_semantic_errors_skip_filters_and_preserve_exact_envelopes() {
    let raw = json!({
        "content": [
            { "type": "text", "text": "release not found", "x-text": true },
            { "type": "text", "text": "detail" },
            { "type": "image", "data": "aW1hZ2U=", "mimeType": "image/png" }
        ],
        "isError": true,
        "_meta": { "kind": "not-found" },
        "x-error": { "preserved": true }
    });
    let result = mcp::progressive::action_tool_result(
        McpToolResult::new(raw.clone()),
        Some("invalid[filter"),
    )
    .expect("semantic errors skip filters");
    assert_eq!(result.raw, raw);
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

    let maximum = mcp::progressive::tool_result(json!((0..256).collect::<Vec<_>>()), Some(".[]"))
        .expect("256 filter outputs are allowed");
    assert_eq!(
        maximum.raw["structuredContent"]["result"]
            .as_array()
            .expect("legacy wrapped outputs")
            .len(),
        256
    );

    let outputs = (0..257).collect::<Vec<_>>();
    let overflow = mcp::progressive::tool_result(json!(outputs), Some(".[]"))
        .expect_err("fan-out must be bounded");
    assert!(format!("{overflow:?}").contains("more than 256 results"));
}

#[test]
fn progressive_semantic_resource_conversion_requires_non_error_structure() {
    let scalar = mcp::progressive::json_resource_result(
        "test://legacy".to_owned(),
        json!("quoted legacy JSON"),
    );
    assert_eq!(scalar.raw["contents"][0]["text"], "\"quoted legacy JSON\"");

    let result = mcp::progressive::action_resource_result(
        "test://semantic".to_owned(),
        McpToolResult::new(json!({
            "content": [],
            "structuredContent": { "ok": true }
        })),
    )
    .expect("semantic resource result");
    assert_eq!(result.raw["contents"][0]["text"], "{\"ok\":true}");

    for value in [
        json!([1, 2]),
        json!("quoted"),
        json!(42),
        json!(true),
        json!(null),
    ] {
        let result = mcp::progressive::action_resource_result(
            "test://semantic".to_owned(),
            McpToolResult::new(json!({
                "content": [],
                "structuredContent": value
            })),
        )
        .expect("semantic scalar resource result");
        let text = result.raw["contents"][0]["text"]
            .as_str()
            .expect("resource text");
        assert_eq!(
            serde_json::from_str::<Value>(text).expect("valid JSON"),
            value
        );
    }

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

#[test]
fn progressive_input_schemas_inline_subschemas() {
    let schema = mcp::progressive::json_schema_for::<SchemaFixtureInput>();
    let serialized = schema.to_string();

    assert!(schema.get("$defs").is_none());
    assert!(!serialized.contains("\"$ref\""));
    assert_eq!(
        schema["properties"]["state"]["enum"],
        json!(["Ready", "Paused"])
    );
}

#[tokio::test]
async fn advertised_progressive_schemas_validate_action_bound_requests() -> mcp::Result<()> {
    let server = RunningServer::start_plain(ReleaseServer::default()).await?;
    let tools = post_json(
        &server.endpoint,
        json!({ "jsonrpc": "2.0", "id": "schema-tools", "method": "tools/list" }),
    )
    .await?;
    let schema = &tool_named(&tools.body, "releases")["inputSchema"];
    let validator = jsonschema::draft202012::new(schema).expect("valid advertised JSON Schema");
    assert!(
        validator.is_valid(&json!({
            "action": "releases.get", "input": { "id": "v1" }
        })),
        "identical schemas on different actions must not compete"
    );
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert!(schema["properties"]["input"].get("oneOf").is_none());
    let discovery_inputs = schema["properties"]["input"]["anyOf"]
        .as_array()
        .expect("root payload discovery union");
    let discovery_validator = jsonschema::draft202012::new(&schema["properties"]["input"])
        .expect("valid payload discovery schema");
    assert!(discovery_validator.is_valid(&json!({ "limit": 1 })));
    assert!(!validator.is_valid(&json!({ "action": "releases.get", "input": { "limit": 1 } })));
    let get_input = discovery_inputs
        .iter()
        .find(|input| input["title"] == "releases.get input")
        .expect("action-labelled get input");
    assert!(get_input.get("allOf").is_none());
    assert_eq!(get_input["properties"]["id"]["type"], "string");
    let runs_input = discovery_inputs
        .iter()
        .find(|input| input["title"] == "list-runs input")
        .expect("action-labelled runs input");
    assert!(runs_input.get("allOf").is_none());
    assert!(runs_input["properties"]["repository"].is_object());
    assert!(runs_input["properties"]["limit"].is_object());
    let no_input = discovery_inputs
        .iter()
        .find(|input| input["title"] == "alert-rule.list input")
        .expect("action-labelled no-input shape");
    assert_eq!(
        no_input["anyOf"],
        json!([
            { "type": "null" },
            { "type": "object", "maxProperties": 0 }
        ])
    );
    assert!(schema["properties"]["filter"].is_object());
    assert!(
        schema["oneOf"]
            .as_array()
            .expect("request alternatives")
            .iter()
            .all(|branch| branch["properties"]["action"]["const"].is_string())
    );

    let cases = [
        (
            true,
            json!({ "action": "releases.get", "input": { "id": "v1" } }),
        ),
        (
            true,
            json!({ "action": "get-copy", "input": { "id": "v1" } }),
        ),
        // The optional list shape overlaps other inputs and must not compete with them.
        (true, json!({ "action": "list", "input": {} })),
        (true, json!({ "action": "list-runs", "input": {} })),
        (
            true,
            json!({ "action": "list-runs", "input": { "repository": "rfhold/axol", "limit": 1 } }),
        ),
        (
            true,
            json!({ "action": "list", "input": { "id": "v1", "limit": 1 } }),
        ),
        (
            true,
            json!({ "action": "list", "input": { "status": "released", "limit": 1 } }),
        ),
        (true, json!({ "action": "nullable", "input": "value" })),
        (true, json!({ "action": "alert-rule.list" })),
        (true, json!({ "action": "alert-rule.list", "input": null })),
        (true, json!({ "action": "alert-rule.list", "input": {} })),
        (true, json!({ "action": "semantic", "input": null })),
        (true, json!({ "action": "semantic-error", "input": {} })),
        (true, json!({ "action": "alert-rule.list", "filter": null })),
        (
            true,
            json!({ "action": "alert-rule.list", "filter": ".[0].name" }),
        ),
        (false, json!({})),
        (false, json!({ "input": {} })),
        (false, json!({ "action": "unknown", "input": {} })),
        (false, json!({ "action": "releases.get" })),
        (false, json!({ "action": "releases.get", "input": null })),
        (false, json!({ "action": "releases.get", "input": {} })),
        (
            false,
            json!({ "action": "releases.get", "input": { "limit": 1 } }),
        ),
        (
            false,
            json!({ "action": "releases.get", "input": { "id": "v1", "extra": true } }),
        ),
        (false, json!({ "action": "list" })),
        (false, json!({ "action": "list-runs" })),
        (false, json!({ "action": "list", "input": null })),
        (
            false,
            json!({ "action": "list", "input": { "limit": "one" } }),
        ),
        (false, json!({ "action": "nullable" })),
        (false, json!({ "action": "nullable", "input": null })),
        (false, json!({ "action": "nullable", "input": 1 })),
        (false, json!({ "action": "unit" })),
        (false, json!({ "action": "unit", "input": null })),
        (false, json!({ "action": "unit", "input": {} })),
        (
            false,
            json!({ "action": "alert-rule.list", "input": { "id": "v1" } }),
        ),
        (false, json!({ "action": "alert-rule.list", "input": [] })),
        (false, json!({ "action": "alert-rule.list", "extra": true })),
        (false, json!({ "action": "alert-rule.list", "filter": 1 })),
    ];
    for (valid, arguments) in cases {
        assert_eq!(validator.is_valid(&arguments), valid, "schema: {arguments}");
        let response = call_tool_response(&server.endpoint, "releases", arguments.clone()).await?;
        assert_eq!(
            response.body.get("error").is_none(),
            valid,
            "dispatch: {arguments}: {}",
            response.body
        );
    }
    Ok(())
}

#[test]
fn progressive_input_schema_associates_inputs_by_action_not_position() {
    use mcp::progressive::{ActionInputSchema, input_schema, json_schema_for};

    let schema = input_schema(
        &["get", "list"],
        vec![
            ActionInputSchema {
                action: "list",
                schema: Some(json_schema_for::<ListRunsInput>()),
            },
            ActionInputSchema {
                action: "get",
                schema: Some(json_schema_for::<GetReleaseInput>()),
            },
        ],
    );
    let validator = jsonschema::draft202012::new(&schema).expect("valid schema");
    assert!(validator.is_valid(&json!({ "action": "list", "input": { "limit": 1 } })));
    assert!(validator.is_valid(&json!({ "action": "get", "input": { "id": "v1" } })));
    assert!(!validator.is_valid(&json!({ "action": "get", "input": { "limit": 1 } })));
    assert_eq!(schema["oneOf"][0]["properties"]["action"]["const"], "list");
    assert_eq!(
        schema["properties"]["input"]["anyOf"][0]["title"],
        "list input"
    );
}

#[tokio::test]
async fn progressive_server_isolates_actions_and_resources_without_help() -> mcp::Result<()> {
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
    assert!(
        discover.body["result"]["capabilities"]
            .get("prompts")
            .is_none()
    );
    assert!(discover.body["result"]["capabilities"]["resources"].is_object());
    assert!(discover.body["result"].get("instructions").is_none());

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
    assert!(releases_tool.get("outputSchema").is_none());
    assert!(policy_tool.get("outputSchema").is_none());
    assert_eq!(releases_tool["annotations"]["readOnlyHint"], true);
    assert_eq!(releases_tool["annotations"]["destructiveHint"], false);
    assert!(policy_tool.get("annotations").is_none());
    assert_array_excludes(
        &releases_tool["inputSchema"]["properties"]["action"]["enum"],
        "help.releases",
    );
    assert_array_excludes(
        &releases_tool["inputSchema"]["properties"]["action"]["enum"],
        "help.alert-rule",
    );
    assert_array_contains(
        &releases_tool["inputSchema"]["properties"]["action"]["enum"],
        "alert-rule.list",
    );
    assert_array_excludes(
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
    let input_options = releases_tool["inputSchema"]["oneOf"]
        .as_array()
        .expect("request oneOf");
    assert!(
        input_options
            .iter()
            .any(|schema| schema.to_string().contains("Release id."))
    );

    let alert_rules = call_tool(
        &server.endpoint,
        "releases",
        json!({ "action": "alert-rule.list" }),
    )
    .await?;
    assert_eq!(
        alert_rules["structuredContent"],
        json!({ "result": [{ "name": "release-health" }] })
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
    assert_eq!(semantic_filtered["structuredContent"], json!("v1.4.2"));
    assert_eq!(semantic_filtered["content"][0]["text"], "\"v1.4.2\"");
    assert_eq!(semantic_filtered["content"][1], semantic["content"][1]);
    assert_eq!(semantic_filtered["_meta"], semantic["_meta"]);

    let semantic_error = call_tool(
        &server.endpoint,
        "releases",
        json!({ "action": "semantic-error" }),
    )
    .await?;
    assert_eq!(semantic_error["isError"], true);
    assert!(semantic_error["content"][0]["text"] == "release not found");

    let filtered_semantic_error = call_tool(
        &server.endpoint,
        "releases",
        json!({ "action": "semantic-error", "filter": "invalid[filter" }),
    )
    .await?;
    assert_eq!(filtered_semantic_error, semantic_error);

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
        templates.body["result"]["resourceTemplates"]
            .as_array()
            .unwrap()
            .len(),
        1
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
            .is_some_and(|message| message.contains("missing input"))
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
            .is_some_and(|message| message.contains("unknown progressive action"))
    );
    assert_eq!(unknown_action.body["error"]["code"], -32602);

    let unknown_field = call_tool_response(
        &server.endpoint,
        "releases",
        json!({ "action": "help", "unexpected": true }),
    )
    .await?;
    assert_eq!(unknown_field.body["error"]["code"], -32602);

    for action in ["help", "help.releases", "help.runbooks", "runbooks.load"] {
        let response =
            call_tool_response(&server.endpoint, "releases", json!({"action": action})).await?;
        assert_eq!(response.body["error"]["code"], -32602);
        assert!(
            response.body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown progressive action")
        );
    }
    let null_filter = call_tool(
        &server.endpoint,
        "releases",
        json!({ "action": "alert-rule.list", "filter": null }),
    )
    .await?;
    assert_eq!(
        null_filter["structuredContent"]["result"][0]["name"],
        "release-health"
    );

    let invalid_filter = call_tool_response(
        &server.endpoint,
        "releases",
        json!({ "action": "alert-rule.list", "filter": ".]" }),
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
