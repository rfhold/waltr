# MCP Apps

## Protocol Boundary

The public `mcp::apps` module supplies typed MCP Apps helpers for SEP-1865 revision `2026-01-26`.
This iframe revision remains independent of core MCP revision `2026-07-28`.
Apps support does not introduce core `initialize`, protocol sessions, or a transport fallback.
Core requests still carry per-request metadata and use `server/discover`.

The generic crate owns metadata, capability, resource decode, and lifecycle payload types.
It does not own browser isolation, authorization, CSP headers, or an iframe lifecycle state machine.
Hosts retain those responsibilities.
For Waltr-specific static tool-linked apps, see [Waltr-served Apps authoring](apps-authoring.md).
That guide describes repository policy, not the generic crate's protocol contract.

## Opt-In And Metadata

`McpClientOptions::with_apps()` enables Apps advertisement; default client options leave it off.
Host-specific asset build defaults do not change this generic protocol opt-in.
For a server-owned example using ordinary tool metadata and resource reads, see [Waltr's status app deployment](../deployment.md#mcp-apps-assets-and-app-container).
The capability occupies `extensions["io.modelcontextprotocol/ui"]` in the per-request client capabilities object.
`UiCapability::html()` advertises `mimeTypes: ["text/html;profile=mcp-app"]`.
`ServerContext::ui_capability()` reads support from the current request, not a connection-wide grant.

Tool `_meta.ui` supports `resourceUri` and `visibility`.
Visibility defaults to both `model` and `app`; an explicit empty array hides both audiences.
The nested `resourceUri` takes precedence over deprecated `_meta["ui/resourceUri"]`.
Malformed visibility, null security fields, and invalid `ui://` resource URIs fail explicitly.
Hosts enforce visibility; the helper itself grants no execution authority.

Resource `_meta.ui` supports `csp`, `permissions`, `domain`, and `prefersBorder`.
Typed CSP declarations cover `connectDomains`, `resourceDomains`, `frameDomains`, and `baseUriDomains`.
Typed permission requests cover camera, microphone, geolocation, and clipboard write.
These fields express requests or hints, not grants.
The parser validates field shapes and CSP origin syntax.
Unknown nested fields survive round trips for host review.

`UiToolDeclaration::try_from(&tool)` validates UI metadata and retains a reference to the unchanged core tool declaration.
`UiResourceDeclaration::try_from(&resource)` validates the selected declaration's URI, MIME type, and UI metadata.
These helpers establish no source-server identity, authorization, or equality between resource declaration metadata and resource-read metadata.
The resource-read decoder does not merge declaration metadata into the response.

## Resource Decode

`decode_resource(result, expected_uri, max_bytes)` accepts exactly one resource-content entry with the exact URI and Apps MIME type.
The entry contains exactly one `text` string or one standard Base64 `blob` string.
Blob content must decode to UTF-8.
The caller supplies a decoded-byte limit; `DEFAULT_MAX_HTML_BYTES` equals one MiB.
The helper also bounds Base64 allocation before decode.

The HTML check requires an HTML5 doctype and an HTML document envelope.
It does not sanitize HTML or establish trust.
`UiResource` retains the HTML, typed UI metadata, and complete raw resource-content metadata.
The host must isolate the document before any script executes.

`McpClient::read_app_resource(uri, max_bytes, cancellation_token)` combines an ordinary resource read with this decoder.
`ReadAppResourceError` distinguishes request failures from typed Apps decode failures.
The method does not enable capability advertisement, authorize the resource, or sandbox its HTML.

## Asset Ownership

The generic MCP crate supplies typed Apps protocol helpers, not an asset build API or browser host.
Hosts own app content, container sources, build tools, browser policy, resource URIs, and deployment.
Waltr uses ordinary workspace package builds for its server-owned status app and web-owned trusted container.
See [Waltr asset deployment](../deployment.md#mcp-apps-assets-and-app-container) for those host-specific build contracts.
Asset builds do not alter generic runtime capability defaults.

## Iframe Payloads

The module defines typed payloads and method constants for these lifecycle messages:

| Method | Role |
| --- | --- |
| `ui/initialize` | View request and host response with the separate Apps revision, identities, capabilities, and host context. |
| `ui/notifications/initialized` | View notification after the handshake. |
| `ui/notifications/tool-input` | Complete arguments object. |
| `ui/notifications/tool-input-partial` | Partial arguments object. |
| `ui/notifications/tool-result` | Original content, optional `structuredContent`, error marker, and raw `_meta`. |
| `ui/resource-teardown` | Host request before teardown, with an optional reason. |

`UiToolResultParams` preserves explicit JSON null in `structuredContent`, opaque content blocks, and unknown result fields.
Typed host capabilities distinguish requested resource policy from applied sandbox policy.
Payload types do not imply that every host implements every Apps method or capability.

## Host Authority And Result Policy

Apps capability advertisement does not establish credential identity or authorize a linked view.
The generic crate supplies no account-incarnation, credential-persistence, or middleware-result policy.
Hosts must bind linked resources and tool calls to current source authority.
Raw result preservation does not authorize a host to bypass its result policy.
If middleware transforms a result, the original raw result is not an equivalent transformed payload.

See [Waltr's session contract](../architecture/agent.md#session-mcp-apps) for credential identity, custom host adapters, and fail-closed result handling.

## References

| Document Or Source | Covers |
| --- | --- |
| [Protocol](protocol.md) | Core revision, per-request metadata, discovery, and transports. |
| [Apps helpers](../../crates/mcp/src/apps.rs) | Public types, constants, validation, and resource decode. |
| [Client API](../../crates/mcp/src/client/api.rs) | Default-off Apps option and per-request advertisement. |
| [Server API](../../crates/mcp/src/server/mod.rs) | Current-request capability access. |
| [Apps conformance tests](../../crates/mcp/tests/spec/v2026_07_28/apps.rs) | Generic metadata, capability, payload, and resource contracts. |
| [Agent architecture](../architecture/agent.md#session-mcp-apps) | Waltr session provenance, permission checks, leases, and audit behavior. |
| [Client architecture](../architecture/clients.md#session-mcp-apps) | Waltr browser host and native limitations. |
| [Deployment](../deployment.md#mcp-apps-assets-and-app-container) | Waltr asset build and separate-origin service. |
| [Testing](../testing.md#mcp-apps) | Layered verification commands and proof limits. |
