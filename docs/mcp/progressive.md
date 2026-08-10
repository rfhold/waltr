# Progressive Tool Servers

The `mcp` crate exposes an opinionated progressive-tool server pattern behind the `progressive` feature. It is implemented by `#[mcp::progressive_server]` and runtime helpers in `mcp::progressive`.

A progressive server declares one or more named MCP tools. Each tool accepts an `action`, optional action-specific `input`, and optional jq-compatible `filter`. Tools own separate actions, namespaces, help, schemas, runbooks, and runbook resource templates.

## Server Surface

| Surface | Generated Behavior |
| --- | --- |
| Tools | Every nested `tool(...)` declaration produces an independently described MCP tool and can set MCP tool annotations. |
| Help actions | Each tool provides `help`, `help.<namespace>`, and, when applicable, `help.runbooks`. |
| Actions | `#[action(tool = "...", name = "...")]` methods dispatch only through their declared tool. |
| Runbooks | `#[runbook(tool = "...", name = "...")]` methods are available through that tool's `runbooks.load`, prompts, and resource template. |
| Resources | Actions can opt into fixed resources with `resource = "..."` or templates with `resource_template = "..."`. |
| Instructions | Initialize instructions list the progressive tools and direct clients to call the relevant tool with action `help` first. |
| HTTP auth | A `#[token_validator]` method generates the same Streamable HTTP protected-resource helpers as `#[mcp::mcp_server]`. Authentication is server-wide; stdio remains unauthenticated. |

Tool declarations are explicit. There is no implicit `exec` tool. Each declaration requires a unique, non-empty name and a description. Namespaces, `runbook_uri_template`, and optional `annotations = <Rust expression>` belong to the tool declaration rather than the server. The annotation expression may produce either `serde_json::Value` or `Option<serde_json::Value>` and is preserved as the listed tool's `annotations` field. Duplicate `annotations` arguments are rejected during macro expansion.

A tool that owns runbooks must declare a `runbook_uri_template` containing the `{runbook}` binding. Effective runbook templates must be unique across tools. `runbooks.load` returns Markdown as text content and object-shaped structured content containing `runbook` and `markdown`.

## Tool Isolation

Each listed tool has an independent action enum and action-specific `input.oneOf` schema. A call first selects the MCP tool and can then dispatch only an action owned by that tool. The same flat action name may exist in different tools.

Actions may use flat names such as `list` or dot-namespaced names such as `releases.get` and `alert-rule.list`. Namespaced actions must use a namespace declared by their tool. Root `help` lists flat actions and declared namespaces; `help.<namespace>` lists that namespace's actions with descriptions, guidance, schemas, and resource bindings.

Namespace declarations support two forms. `namespace(releases, description = "...")` preserves the original Rust-identifier form. `namespace(name = "alert-rule", description = "...")` declares a string-valued public name for namespaces that are not Rust identifiers. Explicit public names must match `[a-z0-9]+(?:-[a-z0-9]+)*`: non-empty ASCII lowercase letters or digits separated by single hyphens. This rejects dots, whitespace, uppercase letters, underscores, leading or trailing hyphens, and repeated hyphens. `help` and `runbooks` remain reserved in either form, namespace names must be unique within a tool, and action namespace matching uses the complete declared name exactly.

Runbooks are tool-owned for loading and help, while MCP prompts are server-global. Runbook names must therefore be unique across the complete server and remain unprefixed in prompt listings.

Prompts and resources exposed by the server are the union of declarations owned by its tools.

## Help Content

Every `#[action]` and `#[runbook]` method must have a doc comment. The first non-empty line becomes the short description. The remaining body explains why and when an LLM should use it.

`help.runbooks` lists only the runbooks belonging to the called tool, with prompt and resource links. `runbooks.load` cannot load another tool's runbook.

## Input Schemas And Filters

Action input types should derive `serde::Deserialize` and `schemars::JsonSchema`. The macro uses those schemas to build the owning tool's `input.oneOf` schema. Progressive request and generated runbook input parsing reject unknown fields to match their `additionalProperties: false` schemas. The optional filter may be omitted or explicitly set to `null`.

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

#[derive(Clone, Default)]
struct ReleaseServer;

#[derive(Deserialize, JsonSchema)]
struct GetReleaseInput {
    /// Release id.
    id: String,
}

#[mcp::progressive_server(
    name = "release-desk",
    version = "1.0.0",
    description = "Release management for the example org.",
    tool(
        name = "releases",
        description = "Inspect product releases.",
        annotations = json!({
            "readOnlyHint": true,
            "destructiveHint": false
        }),
        namespace(releases, description = "Query and inspect product releases."),
        namespace(name = "alert-rule", description = "Inspect release alert rules."),
        runbook_uri_template = "playbooks://{runbook}"
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

    /// Promote a release candidate to production with required pre-checks.
    ///
    /// Load this runbook for an end-to-end production procedure.
    #[runbook(tool = "releases", name = "promote-to-production")]
    fn promote_runbook(&self) -> ServerResult<String> {
        Ok("## Promote to production\n1. Load release details.\n2. Check policy.\n".to_owned())
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
