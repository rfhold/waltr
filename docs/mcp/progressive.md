# Progressive Workflow Servers

The `mcp` crate exposes an opinionated progressive-workflow server pattern behind the `progressive` feature. It is implemented by `#[mcp::progressive_server]` and runtime helpers in `mcp::progressive`.

Progressive servers expose one tool, `exec`. Clients choose a dot-notated `action`, optional action-specific `input`, and an optional jq-compatible `filter` applied to JSON output.

## Server Surface

| Surface | Generated Behavior |
| --- | --- |
| Tool | One `exec` tool with `action`, `input`, and `filter` fields. |
| Help actions | Built-in `help`, `help.<namespace>`, `help.workflows`, and `workflows.load` actions. |
| Actions | `#[action(name = "namespace.action")]` methods dispatch through `exec`. |
| Workflows | `#[workflow(name = "...")]` methods dispatch through `workflows.load`, prompts, and a workflow resource template. |
| Resources | Actions can opt into fixed resources with `resource = "..."` or templates with `resource_template = "..."`. |
| Instructions | Initialize instructions tell clients to call `exec` with action `help` first unless the user supplied a specific action. |
| HTTP auth | If a `#[token_validator]` method is present, the macro generates the same Streamable HTTP protected-resource helpers as `#[mcp::mcp_server]`. Stdio remains unauthenticated. |

The default workflow resource template is `workflows://{workflow}`. Override it with `workflow_uri_template = "..."`.

## Help Content

Namespaces are declared on the server macro with descriptions. Root `help` lists each namespace and when to use it.

Every `#[action]` and `#[workflow]` method must have a doc comment. The first non-empty doc line becomes the short description. The remaining doc body becomes guidance for why and when an LLM should use it.

Namespace-specific help, such as `help.releases`, lists the namespace actions with their descriptions, guidance, input schemas, and resource bindings. `help.workflows` lists workflows with prompt and resource links.

## Input Schemas And Filters

Action input types should derive `serde::Deserialize` and `schemars::JsonSchema`. The macro uses those schemas to build the `exec.input` `oneOf` schema.

The optional `filter` field uses a pure-Rust jaq evaluator. Tests cover common jq-style filters such as `.[].id`; invalid filters return MCP invalid-params errors.

## Example

```rust
use mcp::{
    McpAccessToken, McpTokenValidation,
    server::{McpAuthorizationContext, ServerContext, ServerResult},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

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
    namespace(releases, description = "Query and inspect product releases."),
    workflow_uri_template = "playbooks://{workflow}",
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
    #[action(name = "releases.get", resource_template = "releases://{id}")]
    fn get_release(
        &self,
        input: GetReleaseInput,
        _context: ServerContext,
    ) -> ServerResult<Value> {
        Ok(json!({ "id": input.id, "status": "released" }))
    }

    /// Promote a release candidate to production with required pre-checks.
    ///
    /// Load this workflow when the user wants an end-to-end production
    /// promotion procedure rather than a single release fact.
    #[workflow(name = "promote-to-production")]
    fn promote_workflow(&self) -> ServerResult<String> {
        Ok("## Promote to production\n1. Load release details.\n2. Check policy.\n".to_owned())
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
