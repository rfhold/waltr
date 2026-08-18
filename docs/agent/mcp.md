# MCP Integration

Waltr agent MCP integration lets a session expose tools, prompt templates, and resource templates from configured MCP servers. Generic protocol behavior lives in [../mcp/protocol.md](../mcp/protocol.md); this document covers how the agent layer adapts that protocol surface into Waltr session behavior.

MCP servers are owned by the durable session coordinator so they can remain initialized alongside a session. MCP is not attached directly to `AgentRuntimeBuilder`; runtime construction receives tools and template middleware from the coordinator-owned MCP runtime for that session.

## Entry Points

| API | Purpose |
| --- | --- |
| `McpConfig` | Agent-side collection of MCP servers, request timeout, log level, and trace propagation mode. |
| `McpServerConfig` | One named server and its generic `mcp::client::McpTransportConfig`. |
| `McpClientRuntime` | Session-scoped runtime that starts configured servers and adapts their registries. |
| `McpServerRuntime` | Per-server runtime that owns the typed generic `McpClient`, registries, progress routing, and refresh behavior. |
| `McpInstructionsMiddleware` | Middleware that adds initialized MCP server instructions to the system prompt. |
| `McpFeatureConfig::servers` | Definition-owned include or exclude selector for current configured servers. |
| `McpFeatureConfig::trace_propagation` | Definition-owned Waltr switch for MCP request trace context propagation. |
| `SessionCoordinator` | Resolves current MCP configuration and caches a session-scoped MCP runtime. |

The agent integration depends on the generic `mcp` crate. The generic crate must not depend back on `waltr-agent`.

## Definition Selection

`FeatureSet.spec.mcp.servers` is omitted or contains exactly one of `include` and `exclude`:

```yaml
mcp:
  servers:
    include: [filesystem, issue-tracker]
```

```yaml
mcp:
  servers:
    exclude: [production-admin]
```

Selection uses these exact rules:

| Definition value | Selected current servers |
| --- | --- |
| `servers` omitted | All configured servers. |
| `exclude: []` | All configured servers. |
| `include: []` | No servers. |
| `include: [a, b]` | Only current keys `a` and `b`. |
| `exclude: [a, b]` | All current configured keys except `a` and `b`. |

Include names match current configuration keys exactly. If any include is absent, reconstruction fails and names every missing key in deterministic order. Missing exclude names have no effect. Blank or whitespace-only names, duplicate names, both selectors, an empty selector mapping, `null`, and unknown fields fail strict definition parsing.

The bundled `coding` feature uses `servers: { exclude: [] }`. This selects every configured server, including `waltr` when it is present.

MCP server selection is part of the selected agent definition stored with the session, not an independent MCP setting. Session RPCs and clients can create or switch the session's agent definition, but expose no separate MCP selector, endpoint snapshot, drift status, or trace switch.

## Current Connection Authority

Every normal run and template search reconstruction uses the cycle's selected agent definition. Waltr resolves all MCP keys against the current catalog snapshot. The current stdio command or Streamable HTTP URL is authoritative. Provider resource changes do not affect MCP resolution.

A new reconstruction, including one after an agent switch, sees selected server additions, removals, command changes, URL changes, transport changes, and definition trace changes. An active captured runtime continues with its old MCP runtime. Unselected server changes have no effect. An absent required include fails that reconstruction instead of exposing a stale cached runtime.

Template search uses the same current resolution. It treats MCP preparation failures as best effort and continues without MCP templates. It never returns templates from a stale mismatched runtime.

## Cache Lifecycle

The coordinator caches one MCP runtime per durable session. Its structural fingerprint contains selected keys, transport kinds, targets, and trace propagation. Provider settings, credentials, and unselected MCP entries do not belong to this fingerprint.

An equal fingerprint reuses the cached runtime after credential preflight. Provider-only and unselected-server changes therefore reuse it. A changed fingerprint starts the replacement before the cache swap. Startup failure keeps the old cached runtime for a later attempt, but the failed reconstruction receives the error and cannot use the stale runtime. After a successful swap, current `Arc` users finish with the old runtime. Waltr shuts it down after those users release it.

## Tool Discovery

For each configured server, the session MCP runtime:

1. Starts a typed generic MCP client for each server.
2. Stores server capabilities from the initialize response.
3. Lists tools when the server advertises tool support, following cursors until all pages are loaded.
4. Stores tool metadata, including `inputSchema`, `outputSchema`, and annotations.
5. Converts the current tool registry into `AgentTool`s when an agent runtime is resolved.

MCP tools are exposed directly. Tool names are generated as:

```text
mcp_<server>_<tool>
```

Non-alphanumeric characters are sanitized so the name is provider-safe.

## Tool Calls

When a provider calls an MCP-backed tool, the agent uses the normal tool execution path:

- The tool call and result are recorded as generic timeline events and journal-backed session projection data.
- Permission behavior can target the generated tool name through normal tool selectors.
- The MCP runtime sends `tools/call` to the server.
- The MCP request includes a progress token.
- If the Waltr tool call is canceled or times out, the MCP runtime sends `notifications/cancelled` for the in-flight JSON-RPC request.
- The response is converted into ordered provider-neutral `ToolResult` text and image blocks.

MCP result conversion preserves text order, represents unknown content with explicit unsupported-content markers, appends `structuredContent` as text, and preserves MCP `isError` as structured tool-result state. Image blocks must use canonical standard Base64, declare `image/png`, `image/jpeg`, or `image/webp`, and match that type's file signature. Waltr accepts at most four images and 4 MiB of decoded image data in one result. Invalid encoding, unsupported media, signature mismatches, and excess data become bounded MCP protocol errors; image data is never included in the error.

Waltr opts into an 8 MiB MCP message and event-stream line limit. This limit fits one bounded 4 MiB decoded image plus Base64 and JSON framing. Generic MCP client defaults remain smaller. Tool results persist ordered blocks for session replay. Text-only consumers receive media type, decoded size, and SHA-256 digest instead of Base64.

Provider adapters deliver validated images as follows:

| Provider API | Image mapping |
| --- | --- |
| Anthropic Messages | Native image blocks inside the `tool_result`; MCP error state maps to `is_error`. |
| OpenAI Responses HTTP and WebSocket | `input_image` items inside `function_call_output.output`. |
| OpenAI Chat Completions | The required text tool message is emitted first, followed by a labeled user multimodal message for images from the completed tool-call batch. |

The selected model must support image input. Waltr does not silently omit images or automatically switch models when a configured model rejects multimodal input.

`outputSchema` is preserved in the tool registry for future validation and presentation work. Waltr does not yet validate `structuredContent` against `outputSchema`.

## Prompt Discovery

For each configured server, the session MCP runtime also lists prompts when the server advertises prompt support. Prompt definitions are stored with their names, titles, descriptions, and argument metadata.

MCP prompts are surfaced through the normal session template search target for prompts. Templates use the server key and prompt name:

```text
/foo:bar
```

If the prompt declares required arguments, the inserted template includes JSON placeholders:

```text
/foo:bar({"name":""})
```

Prompt arguments follow the MCP `prompts/get` schema and must be a JSON object with string values. Invalid template arguments render a template error instead of sending a protocol-invalid MCP request.

Prompt names and server keys are inserted only when they can be represented by Waltr's slash-template syntax: ASCII alphanumeric characters, `_`, and `-`. Other spec-valid MCP prompt names remain registered but are not currently offered as slash-template search results.

When a user prompt starts with an MCP prompt template, the template middleware sends `prompts/get` to that server and inserts the returned prompt content before the provider request. Prompt content can contain nested template references, which are rendered by the normal template pipeline.

## Resource Discovery

For each configured server, the session MCP runtime lists concrete resources and resource URI templates when the server advertises resource support. Concrete resource definitions are stored with their URI, name, title, description, MIME type, and size when provided. Resource URI templates are stored with their URI template, name, title, description, and MIME type when provided.

MCP resources are supported only as prompt templates for now. They are surfaced through the normal session template search target for resources, and inserted templates use the server key plus a JSON string URI:

```text
@resource(foo, "test://status")
```

When a user prompt contains an MCP resource template, the template middleware sends `resources/read` to that server and inserts the returned resource content before the provider request. Text resource contents are inserted directly. Blob resource contents render an unsupported-content marker.

MCP `resources/templates/list` entries are surfaced through the same resource search target. Waltr inserts them as concrete resource references containing the server's URI template:

```text
@resource(foo, "test://items/{id}")
```

The user fills the URI path in-place before submitting, for example:

```text
@resource(foo, "test://items/42")
```

The renderer permits `resources/read` when the requested URI either exactly matches a listed concrete resource or matches one of the server's listed URI templates. Resource template search can call the MCP completion API for URI template arguments when the server advertises completion support.

MCP resources are not exposed as provider tools.

## Server Instructions

If an MCP server returns `instructions` from `server/discover`, Waltr stores them with that session's MCP server runtime and adds them to the provider system prompt through MCP middleware.

Agent definitions control instruction exposure through
`FeatureSet.spec.mcp.instructions`. The runtime factory requires a resolved agent
definition and does not provide a second MCP-instructions override.

Prompt fragments are emitted in server-key order and include the source server key:

```text
MCP server instructions from `foo`:
...
```

The instructions middleware is registered with the other MCP session middleware, after the durable base system prompt and before AGENTS/skills middleware.

## Notifications and MRTR

The generic MCP client opens `subscriptions/listen` for the list-change filters advertised by the server and reconnects after an unexpected closure. The agent-side handler converts subscription messages and request-scoped notifications into registry refreshes, progress updates, or logs. Roots are supplied through multi-round-trip request (MRTR) input rather than a server-initiated JSON-RPC request.

| MCP message | Current behavior |
| --- | --- |
| `notifications/tools/list_changed` | Refreshes that server's tool registry in the background. Newly built agent runtimes see the refreshed tools. |
| `notifications/prompts/list_changed` | Refreshes that server's prompt registry in the background. Newly built agent runtimes and template searches see the refreshed prompts. |
| `notifications/resources/list_changed` | Refreshes that server's concrete resource and resource URI template registries in the background. Template searches see the refreshed resources. |
| `notifications/message` | Publishes an internal MCP log event and writes a tracing event when the originating request opted into a log level. |
| `notifications/progress` | Publishes an internal MCP progress event. |
| MRTR `roots/list` input request | Retries the originating tool, prompt, or resource request with the coordinator runtime root as a single `file://` root response. |

The internal MCP event stream is not yet exposed as a public client or UI contract.

## Trace Propagation and Spans

`FeatureSet.spec.mcp.tracePropagation` is the only Waltr switch. Omission defaults to `true`; explicit `false` disables propagation. When enabled, runtime construction maps it to `McpTracePropagation::TraceContext` before connection startup. The generic `mcp` crate default remains `McpTracePropagation::Disabled`.

Trace context is propagated through MCP request `_meta.traceparent` and `_meta.tracestate`, not only through HTTP headers. See [../mcp/tracing.md](../mcp/tracing.md) for generic client/server span and propagation details.

The agent integration also wraps Waltr-facing MCP operations in `mcp.agent.operation` spans:

| Operation | Fields |
| --- | --- |
| `tools/call` | `mcp.server`, `mcp.transport`, `rpc.method`, `mcp.tool.name`. |
| `prompts/get` | `mcp.server`, `mcp.transport`, `rpc.method`, `mcp.prompt.name`. |
| `resources/read` | `mcp.server`, `mcp.transport`, `rpc.method`, `mcp.resource.uri`. |
| `completion/complete` | `mcp.server`, `mcp.transport`, `rpc.method`, `mcp.resource_template.uri_template`. |

## Outbound OAuth

The bundled `mcp-servers/waltr.yaml` resource uses the same target-scoped credentials as every other Streamable HTTP MCP server. Public hosted `/mcp` OAuth remains a separate inbound authorization boundary.

Streamable HTTP authentication failures preserve the parsed bearer challenge instead of flattening it into a display string. Session errors carry the server endpoint, HTTP status, protected-resource metadata URL, scope, bearer `error`, sanitized `error_description`, `error_uri`, and realm through `McpAuthorizationContext`; the server exposes that context in typed `AuthenticationFailure` gRPC status details under the outbound-MCP domain.

A Streamable HTTP bearer challenge becomes an ordinary target-scoped session failure and never begins authorization itself. The user explicitly starts authorization for that MCP server from Config. The server then discovers the exact RFC 9728 protected resource, tries advertised authorization servers until it finds a compatible public server, and discovers RFC 8414 endpoints before selecting registration. Production discovery and every discovered authorization, token, registration, revocation, and JWKS destination require HTTPS and a public network destination by default. Waltr automatically supplies its initialized hosted MCP origin to agent session refresh, while server-owned authorization orchestration admits that origin only for the exact initialized self resource. Operators may additionally allow RFC 1918 IPv4 or IPv6 unique-local ingress for exact HTTPS origins with `WALTR_MCP_OUTBOUND_OAUTH_TRUSTED_PRIVATE_ORIGINS`; the comma-separated entries contain no path and match scheme, host, and effective port. Provider OAuth never receives this trust. Redirects are disabled, DNS answers are checked and pinned for direct requests, loopback/link-local/metadata addresses remain rejected, and the connected peer is checked where the HTTP stack exposes it. Registration selection is:

1. A configured preregistered client for that server key.
2. A still-valid persisted preregistered or CIMD registration for the same issuer, resource, and redirect.
3. Waltr's `/.well-known/oauth-client/waltr` CIMD client ID when the server advertises CIMD.
4. A still-valid persisted dynamic registration.
5. Public DCR when the server advertises a registration endpoint.

Configured preregistration is optional, target-scoped, and issuer-bound. Normalize the server key by uppercasing ASCII letters/digits and replacing every other character with `_`; `WALTR_MCP_<NORMALIZED_SERVER_KEY>_OAUTH_CLIENT_ID` supplies the public client ID, required `WALTR_MCP_<NORMALIZED_SERVER_KEY>_OAUTH_ISSUER` must exactly match discovered authorization-server metadata, and optional `WALTR_MCP_<NORMALIZED_SERVER_KEY>_OAUTH_REDIRECT_URI` overrides the default server callback. There is no global registration, registration-specific `self` branch, or deployment-owned `WALTR_MCP_SELF_OAUTH_*` contract. If no mechanism is available, authorization returns typed `CONFIGURATION_REQUIRED` in the outbound-MCP domain.

The default redirect is the server-owned `/outbound-oauth/callback`. Callback attempts, state, PKCE material, registration method, completion mode, and expiry are durable in Waltr SQL so another replica can claim completion. Completion requires the exact state for both callback and manual input. When the authorization server advertises RFC 9207, the response must also contain its exact issuer; a supplied issuer is always checked. Completion claims carry an opaque identity and expiry, use compare-and-swap for every terminal write, and can be reclaimed after a worker crashes. A configured nondefault redirect uses manual completion. Shared clients receive only the authorization URL and sanitized attempt lifecycle; web polls callback attempts and TUI can submit a final redirect URL for manual completion.

Target-scoped durable credential state preserves the exact resource, issuer, selected registration, authorization/token/revocation endpoints, access and refresh tokens, granted scopes, expiry, and refresh generation. Expiry distinguishes a known deadline, a true non-expiring token, and a token whose successful response omitted `expires_in`; omitted expiry does not retain the replaced token's deadline. The Waltr SQL backend uses a 30-second identity-bound compare-and-swap refresh lease across sessions/replicas. A claimant commits only against its generation and claim identity. An indeterminate exchange or an expired refresh lease requires reauthorization rather than risking reuse of a rotated token family. Code-exchange and refresh responses must be JSON and fit within 64 KiB. Authorization, token, refresh, and revocation requests preserve the discovered resource. Debug output for attempts, claims, durable OAuth state, and token values is redacted.

Agent transports use a bearer provider rather than copying one token into transport configuration. The provider reads current durable state before each stateless POST, refreshes shortly before a known expiry, and returns the replacement token without reconstructing the session runtime. The same refresh coordinator supports an explicit target-scoped forced refresh while preserving generation/claim coordination and resource binding. A newly issued bearer with unknown expiry is reused until the MCP server rejects it rather than refreshing on every request. Config clients expose sanitized MCP credential status and authorize, force-refresh, and reset actions, while registration remains discovery- and environment-owned. Outbound logout attempts RFC 7009 revocation without redirects or a long-lived wait, then deletes local state unconditionally. External MCP expiration or rejection is an ordinary outbound-MCP failure carrying the exact server key and sanitized challenge context; it never starts OAuth, replays work, or clears Waltr access credentials.

## Boundaries

- MCP support is opt-in through the current resolved agent definition.
- Generic MCP protocol implementation belongs in `crates/mcp`; Waltr conversion into tools, templates, permissions, and session runtime belongs in `crates/waltr-agent/src/mcp/`.
- MCP server instructions are injected into the provider system prompt by default for factory-built session runtimes; this is a factory policy, not generic MCP protocol behavior.
- Resource URI subscriptions, sampling, elicitation, non-roots MRTR input, and task runtime extensions are not implemented. List-change subscriptions are implemented through `subscriptions/listen`.
- Generic MCP meta-tools such as `search_mcp_tools` or `call_mcp_tool` are not exposed; discovered MCP tools are exposed directly.
- Stdio env/cwd configuration and HTTP auth/header configuration are not implemented in the agent MCP config.
- Session RPC and UI timeline results expose the safe text projection rather than inline image data; direct model delivery and durable agent replay retain validated image blocks.
- MCP server resources select `streamableHttp` or `stdio`. See [Resource Catalog](../architecture/config.md#resource-schemas).
- Streamable HTTP sends one POST per message and uses request-scoped JSON or event-stream responses. Protocol sessions, GET delivery, resumability, and DELETE termination are absent.
- Confidential downstream clients and registration-management credentials are not supported; outbound registration is public-client only.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/mcp/tests/spec.rs](../../crates/mcp/tests/spec.rs) and [../../crates/mcp/tests/spec/](../../crates/mcp/tests/spec/) | Generic MCP protocol conformance. |
| [../../crates/mcp/tests/ecosystem.rs](../../crates/mcp/tests/ecosystem.rs) and [../../crates/mcp/tests/ecosystem/](../../crates/mcp/tests/ecosystem/) | Generic MCP ecosystem interoperability. |
| [../../crates/waltr-agent/tests/agent/mcp.rs](../../crates/waltr-agent/tests/agent/mcp.rs) | Deterministic stdio and streamable HTTP MCP tool flows, server instructions, plus stdio MCP prompt/resource template discovery and rendering. |
| [../../crates/waltr-agent/tests/live/agent/mcp.rs](../../crates/waltr-agent/tests/live/agent/mcp.rs) | Ignored live-provider MCP coverage. |
| [../../crates/waltr-agent/src/bin/waltr-agent-test-mcp-server.rs](../../crates/waltr-agent/src/bin/waltr-agent-test-mcp-server.rs) | Test MCP server binary. |
| [../../crates/waltr-server/tests/mcp_oauth_server.rs](../../crates/waltr-server/tests/mcp_oauth_server.rs) | Waltr-hosted issuer, browser bridge/consent, token lifecycle, direct-upstream rejection, and restart behavior. |

## References

| Path | Purpose |
| --- | --- |
| [../mcp/protocol.md](../mcp/protocol.md) | Generic MCP protocol crate behavior and boundaries. |
| [../mcp/tracing.md](../mcp/tracing.md) | Generic MCP spans and trace propagation. |
| [../../crates/mcp/src/client/](../../crates/mcp/src/client/) | Typed generic MCP client, connection loop, and transports. |
| [../../crates/mcp/src/server/mod.rs](../../crates/mcp/src/server/mod.rs) | Typed generic MCP server. |
| [../../crates/mcp/src/protocol.rs](../../crates/mcp/src/protocol.rs) | JSON-RPC helpers and MCP protocol version. |
| [../../crates/mcp/src/trace.rs](../../crates/mcp/src/trace.rs) | Generic request `_meta` trace propagation helpers. |
| [../../crates/waltr-agent/src/mcp/config.rs](../../crates/waltr-agent/src/mcp/config.rs) | MCP configuration types. |
| [../../crates/waltr-agent/src/mcp/runtime.rs](../../crates/waltr-agent/src/mcp/runtime.rs) | Session MCP runtime, `AgentTool` conversion, prompt access, and resource access. |
| [../../crates/waltr-agent/src/mcp/server.rs](../../crates/waltr-agent/src/mcp/server.rs) | Waltr-side MCP server runtime, registries, notifications, progress, roots, and request behavior. |
| [../../crates/waltr-agent/src/middleware/mcp_instructions.rs](../../crates/waltr-agent/src/middleware/mcp_instructions.rs) | System-prompt middleware for initialized MCP server instructions. |
| [../../crates/waltr-agent/src/mcp/name.rs](../../crates/waltr-agent/src/mcp/name.rs) | Generated tool-name sanitization. |
