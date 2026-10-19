# Progressive Tool Servers

The `mcp` crate exposes an opinionated progressive-tool server pattern behind the `progressive` feature. It is implemented by `#[mcp::progressive_server]` and runtime helpers in `mcp::progressive`.

A progressive server declares named MCP tools with grouped actions. Each tool accepts an `action`, optional action-specific `input`, and optional jq-compatible `filter`.

Caller-authored [MCP Skills](skills.md) supply workflow guidance. The macro exposes action schemas, not generated guidance documents.

## Server Surface

| Surface | Generated Behavior |
| --- | --- |
| Tools | Every nested `tool(...)` declaration produces an independently described MCP tool and can set MCP tool annotations. |
| Actions | `#[action(tool = "...", name = "...")]` methods dispatch only through their declared tool. |
| Skills | `skills = self.catalog` registers an embedded `SkillCatalog` for `skills/list`, `skills/get`, and exact-byte `resources/read`. |
| Resources | Actions can opt into fixed resources with `resource = "..."` or templates with `resource_template = "..."`. |
| HTTP auth | A `#[token_validator]` method generates the same Streamable HTTP protected-resource helpers as `#[mcp::mcp_server]`. Authentication is server-wide; stdio remains unauthenticated. |

Tool declarations are explicit. There is no implicit `exec` tool. Each declaration requires a unique, non-empty name and a description.
Namespaces and optional `annotations = <Rust expression>` belong to the tool declaration; `skills` belongs to the server.
The annotation expression accepts `serde_json::Value` or `Option<serde_json::Value>`. The macro preserves it in the tool's `annotations` field and rejects duplicate annotation arguments.

## Tool Isolation

Each tool has its own action enum and request-level `oneOf`. Each branch binds an action `const` to that action's input schema. Identical input types and schemas that overlap remain valid. A call first selects the MCP tool and can then dispatch only an action that the tool owns. The same flat action name can exist in different tools.

Actions accept flat names such as `list` or dot-namespaced names such as `releases.get` and `alert-rule.list`.
Namespaced actions must use a namespace that their tool declares.

Namespace declarations accept `namespace(releases, description = "...")` or `namespace(name = "alert-rule", description = "...")`.
Explicit public names must match `[a-z0-9]+(?:-[a-z0-9]+)*` and remain unique within the tool.
Action namespace validation uses the complete declared name exactly.

Every `#[action]` method requires a doc comment. Its first non-empty line supplies the short action description.
Skill authors supply workflow prose and supporting files explicitly. The macro does not generate mechanical reference files from action declarations.

## Input Schemas And Filters

Action input types derive `serde::Deserialize` and `schemars::JsonSchema`. The macro pairs each input schema with its action in a request-level `oneOf` branch. The top-level schema remains an object with explicit `action`, `input`, and `filter` properties and `additionalProperties: false`.
For payload discovery, `properties.input.anyOf` also exposes each raw input schema with an action-specific title and no extra `allOf` wrapper. No-input shapes include both `null` and an empty object. This redundant union permits duplicate schemas; it does not enforce action/input associations.
For validation, consumers must retain the request-level `oneOf` and select branches by `properties.action.const`, not array position. Those branches enforce the action/input association, required input, and non-null typed input. The root discovery union alone does not describe valid requests.
Typed actions require a non-null `input`, even for nullable types or types with only optional fields. Request parsing treats explicit `null` as absent before deserialization. No-input actions accept omission, `null`, or an empty object, but reject other values.
Progressive request parsing rejects unknown envelope fields. Action types control their own input deserialization.
The optional filter accepts omission or explicit `null`.

The optional `filter` field uses a pure-Rust jaq evaluator. Tests cover common jq-style filters such as `.[].id`; invalid filters and filters producing more than 256 values return MCP invalid-params errors. This is a result-count bound only; progressive servers do not impose a byte, message, or individual-result size limit.

Actions may return either `ServerResult<serde_json::Value>` or `ServerResult<McpToolResult>`. JSON actions retain their existing behavior: the output supplies text content, object output is used directly as structured content, and scalar, array, or null output is exposed as `structuredContent.result`.

An `McpToolResult` action supplies the complete semantic result envelope. MCP permits `structuredContent` to contain any JSON value. Without a filter, Waltr preserves the complete envelope exactly. If `isError` is `true`, Waltr likewise skips any requested filter and preserves the complete error envelope. For a successful result with a filter, Waltr requires `structuredContent`, evaluates jq only against that value, and stores the exact zero, one, or many-output result directly in `structuredContent`.

Waltr also serializes a successful filtered value as compact JSON for text compatibility. It replaces the first text block's `text` while retaining that block's extension fields, removes additional text blocks, and inserts a generated text block at the beginning when none exists. Non-text blocks and their extensions retain their original values and relative order; envelope extensions, `_meta`, and `isError` are unchanged. The result must be an object with array-shaped `content`; every content entry must be an object with a string `type`; and optional `isError` must be boolean. Shape failures are invalid-params errors. Progressive tool definitions continue to omit `outputSchema`, and Waltr does not apply runtime output-schema validation.

Resource-exposed semantic actions use their validated `structuredContent` as the JSON resource value. Resource conversion rejects semantic errors and results without `structuredContent`. Every `application/json` resource text is JSON serialization, so scalar strings include their required quotes.

## Example

```rust
use mcp::{
    McpAccessToken, McpTokenValidation,
    server::{McpAuthorizationContext, ServerContext, ServerResult},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone)]
struct ReleaseServer {
    catalog: std::sync::Arc<mcp::SkillCatalog>,
}

#[derive(Deserialize, JsonSchema)]
struct GetReleaseInput {
    /// Release id.
    id: String,
}

#[mcp::progressive_server(
    name = "release-desk",
    version = "1.0.0",
    description = "Release management for the example org.",
    skills = self.catalog,
    tool(
        name = "releases",
        description = "Inspect product releases.",
        annotations = json!({
            "readOnlyHint": true,
            "destructiveHint": false
        }),
        namespace(releases, description = "Query and inspect product releases."),
        namespace(name = "alert-rule", description = "Inspect release alert rules.")
    ),
    tool(
        name = "sessions",
        description = "Inspect release automation sessions."
    ),
    auth(
        scopes = ["releases:read"],
        required_scopes = ["releases:read"],
        realm = "release-desk",
        resource_name = "Release Desk"
    )
)]
impl ReleaseServer {
    /// Read a specific release when the user names or selects a release id.
    ///
    /// Use this after listing releases or when a resource URI already contains
    /// the release id.
    #[action(tool = "releases", name = "releases.get", resource_template = "releases://{id}")]
    fn get_release(
        &self,
        input: GetReleaseInput,
        _context: ServerContext,
    ) -> ServerResult<Value> {
        Ok(json!({ "id": input.id, "status": "released" }))
    }

    /// List alert rules associated with releases.
    #[action(tool = "releases", name = "alert-rule.list")]
    fn list_alert_rules(&self) -> ServerResult<Value> {
        Ok(json!([]))
    }

    /// List recent release automation sessions.
    #[action(tool = "sessions", name = "list")]
    fn list_sessions(&self) -> ServerResult<Value> {
        Ok(json!([]))
    }

    /// Report a read failure with semantic MCP error content.
    #[action(tool = "sessions", name = "read-error")]
    fn read_error(&self) -> ServerResult<mcp::McpToolResult> {
        Ok(mcp::McpToolResult::new(json!({
            "content": [{ "type": "text", "text": "session not found" }],
            "isError": true,
            "_meta": { "kind": "not-found" }
        })))
    }

    #[token_validator]
    fn validate_token(
        &self,
        token: McpAccessToken,
        _context: McpAuthorizationContext,
    ) -> McpTokenValidation {
        if token.0 == "release-desk-secret" {
            McpTokenValidation::Authorized
        } else {
            McpTokenValidation::Unauthorized {
                error_description: Some("invalid release desk token".to_owned()),
            }
        }
    }
}
```

Focused coverage lives in [../../crates/mcp/tests/progressive.rs](../../crates/mcp/tests/progressive.rs).
