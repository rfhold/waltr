# MCP Specification Tests

`tests/spec/v2026_07_28/` is the only active revision suite. Tests assert the official
`2026-07-28` wire behavior directly; ecosystem tests are supplementary evidence only.

| Module | Coverage |
| --- | --- |
| `basic.rs` | JSON-RPC builders, signed/string IDs, classification, malformed messages, result envelopes, omitted-resultType tolerance, cancellation shape. |
| `protocol.rs` | Complete stateless discovery output, capabilities, instructions, server identity, cache hints, and exact unsupported-version errors. |
| `transport.rs` | POST-only HTTP, Origin policy, admission order, media/body limits, metadata/header mismatches, notification `202`, and removed lifecycle methods. |
| `authorization.rs` | Protected-resource metadata, bearer outcomes, static/dynamic client credentials, cancellation/failure, and active-stream expiry. |
| `stdio.rs` | Parse/invalid-request recovery, required metadata, discovery, and active-request cancellation. |
| `client.rs` | Real HTTP client metadata, discovery validation, roots MRTR retries/state, Base64 names, nested `x-mcp-header` mirroring, and SSE failures. |
| `request_sse.rs` | Incremental request-scoped delivery and disconnect cancellation. |
| `subscriptions.rs` | Acknowledgment ordering, supported filter subsets, subscription IDs, graceful completion, and required filters. |
| `server/` | Complete tools, prompts, resources, and resource-template payloads, errors, annotations, metadata, and cursors. |
| `server_utilities/` | Typed completion, pagination propagation/errors, exact request logging, progress, and request metadata. |
| `common/` | Active content block wire shapes, annotations, metadata, and formatting helpers. |
| `schema.rs` | Capabilities, extension preservation, JSON Schemas, icons, annotations, tool metadata, and prompt argument titles. |
| `oauth_protocol.rs` | Metadata policy, redirect parsing, CIMD/DCR normalization, secret redaction, bearer challenges, and discovery URL construction. |
| `unsupported.rs` | Capability omission and explicit rejection of unsupported methods and non-roots MRTR. |

Unit tests in `src/http_headers.rs` cover the full header annotation validator and value
encoding matrix. Hosted OAuth flows, hardened network fetching, and store contracts remain
separate integration tests. See [PARITY.md](PARITY.md) for the old-to-new scenario map.

Intentionally unsupported behavior is asserted through capability omission and explicit
errors: resource URI subscriptions, sampling, elicitation, task runtime, non-roots MRTR,
and removed initialize/session-era request methods. Obsolete notifications remain ignorable
under JSON-RPC notification semantics and cannot recreate lifecycle state.

Run:

```bash
cargo test -p mcp --test spec
cargo test -p mcp --tests --all-features
```
