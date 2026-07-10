# MCP Spec Tests

This directory is reserved for conformance tests derived directly from the MCP specification.

Planned coverage belongs here when a test case is validating protocol requirements rather than ecosystem interoperability. Ecosystem cross-crate tests live under `tests/ecosystem/`.

## Contract

- Scope tests to the single supported protocol revision. Current coverage belongs under `v2025_11_25`.
- Prefer focused files that mirror spec pages or features.
- Assert protocol requirements directly. Use ecosystem tests for cross-crate compatibility.
- Keep shared helpers minimal and move logic into `support.rs` only after at least two files need it.
- Leave unsupported areas empty or document them as unsupported. Do not add passing placeholders that imply coverage.

## Coverage

| Area | File | Status | Notes |
| --- | --- | --- | --- |
| Basic / JSON-RPC | `v2025_11_25/basic/json_rpc.rs` | Partial | Covers public builder/parser shapes, malformed message rejection, and HTTP JSON-RPC error shape. Batch requests remain unsupported. |
| Basic / Authorization | `v2025_11_25/basic/authorization.rs` | Partial | Covers protected-resource metadata validation, well-known discovery paths and fallback, bearer challenges, bearer token injection, validator outcomes, post-initialize polling authorization failures, structured client authorization errors, and discovery URL ordering. Full OAuth authorization-code flow, registration, token exchange, refresh, and storage remain outside the crate. |
| Basic / Lifecycle | `v2025_11_25/basic/lifecycle.rs` | Partial | Covers initialize result shape, client initialized notification, pre-initialize request rejection, and repeated initialize rejection. Stdio sequencing remains uncovered. |
| Basic / Transports | `v2025_11_25/basic/transports.rs` and `v2025_11_25/transports.rs` | Partial | Covers Streamable HTTP POST behavior, required POST `Accept`, protocol-version rejection, GET polling, DELETE termination, Origin rejection, and end-to-end client polling. |
| Basic / Cancellation | `v2025_11_25/basic/cancellation.rs` | Partial | Covers cancellation notification parameter shape. End-to-end cancellation remains unsupported. |
| Basic / Ping | `v2025_11_25/basic/ping.rs` | Partial | Covers server response to `ping`, request ID preservation, and client-side inbound ping response. |
| Basic / Progress | `v2025_11_25/basic/progress.rs` | Partial | Covers server progress notification over SSE with numeric token, progress, total, and message. Central uniqueness/increasing enforcement remains uncovered. |
| Basic / `_meta` | `v2025_11_25/basic/meta.rs` | Partial | Covers opaque `_meta` preservation in responses and notifications. Reserved-key constraints remain uncovered. |
| Baseline Metadata | `v2025_11_25/baseline.rs` | Partial | Covers active protocol version, implementation description/icons, task capability shape, tool icons, and `execution.taskSupport`. |
| Server / Prompts | `v2025_11_25/server/prompts.rs` | Partial | Covers capability declaration, `prompts/list`, `prompts/get`, arguments, messages, cursor, and invalid params. |
| Server / Resources | `v2025_11_25/server/resources.rs` | Partial | Covers capability declaration, `resources/list`, `resources/read`, text/blob content, cursor, invalid params, and resource-not-found `-32002`. |
| Server / Resource Templates | `v2025_11_25/server/resource_templates.rs` | Partial | Covers `resources/templates/list` metadata and cursor. |
| Server / Resource Subscriptions | `v2025_11_25/server/resource_subscriptions.rs` | Unsupported | No subscription behavior is implemented yet. |
| Server / Tools | `v2025_11_25/server/tools.rs` | Partial | Covers capability declaration, `tools/list`, `tools/call`, schemas, structured content, cursor, and invalid params. |
| Server Utilities / Completion | `v2025_11_25/server_utilities/completion.rs` | Partial | Covers resource-ref and prompt-ref completion shape, invalid params, and 100-value maximum enforcement. |
| Server Utilities / Logging | `v2025_11_25/server_utilities/logging.rs` | Partial | Covers logging capability, `logging/setLevel`, emitted message notifications, and central level validation. Logging is expected to be deprecated in a future MCP revision. |
| Server Utilities / Pagination | `v2025_11_25/server_utilities/pagination.rs` | Partial | Covers opaque cursor propagation and `nextCursor` across paginated list methods. |
| Client / Roots | `v2025_11_25/client/roots.rs` | Partial | Covers roots capability advertisement and `roots/list` responses. Roots are expected to be deprecated in a future MCP revision. |
| Client / Sampling | `v2025_11_25/client/sampling.rs` | Unsupported | No sampling behavior is implemented; sampling is expected to be deprecated in a future MCP revision. |
| Client / Elicitation | `v2025_11_25/client/elicitation.rs` | Unsupported | No elicitation behavior is implemented yet. |
| Common / Annotations | `v2025_11_25/common/annotations.rs` | Partial | Covers tool annotations, content annotation wire keys, and resource/template annotation round-trip via schema-shape tests. |
| Common / Content | `v2025_11_25/common/content.rs` | Partial | Covers public result helpers and raw supported content block variants. |
| Common / Capabilities | `v2025_11_25/common/capabilities.rs` | Partial | Covers client-side server capability parsing and unknown raw preservation. Public server emission remains indirect. |
| Common / Schema Shapes | `v2025_11_25/common/schema_shapes.rs` | Partial | Covers public list, completion, tool-call, root, tool metadata, prompt `_meta`, resource annotations/`_meta`, and template annotations/`_meta` wire shapes. |
