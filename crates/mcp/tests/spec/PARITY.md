# MCP Specification Test Parity

This matrix maps the former `v2025_11_25` suite to the only active revision suite,
`v2026_07_28`. Parity means equivalent coverage for behavior that still exists. It does
not preserve initialization, protocol sessions, GET polling, DELETE cleanup, resumability,
event IDs, ping, global logging levels, or task-core shapes removed by `2026-07-28`.

| Former area | `2026-07-28` coverage | Disposition |
| --- | --- | --- |
| JSON-RPC builders and message classification | `basic.rs` | Adapted; signed numeric IDs and stricter malformed/result checks added. |
| Cancellation parameter shape | `basic.rs`, `stdio.rs` | Adapted and expanded with active-handler cancellation. |
| Initialize negotiation, initialized ordering, and sessions | `protocol.rs`, `transport.rs` | Replaced by mandatory metadata and stateless `server/discover`; removed request methods fail and obsolete notifications create no state. |
| Server identity, instructions, and capabilities | `protocol.rs`, `client.rs` | Adapted to discovery and malformed-discovery client validation. |
| Ping | `transport.rs` | Removed and explicitly returns method-not-found. |
| Request/result/notification metadata | `client.rs`, `server_utilities/progress.rs` | Adapted to mandatory per-request metadata; opaque fields and trace metadata are preserved. |
| Progress token and notification fields | `server_utilities/progress.rs` | Adapted; exact fields, token association, finite values, and monotonicity are exercised. |
| HTTP POST negotiation and JSON response | `transport.rs`, `protocol.rs` | Adapted to stateless POST and request-scoped SSE. |
| HTTP GET polling, DELETE, reconnect, event replay, leases, and sessions | `transport.rs` | Removed; GET/DELETE explicitly return method-not-allowed. |
| Origin validation and admission order | `transport.rs` | Preserved: default deny, canonical allowlist, duplicates, and validation before body/media work. |
| Content negotiation, malformed JSON, and body limits | `transport.rs` | Preserved for active POST behavior. |
| Incremental SSE and response-stream cancellation | `request_sse.rs` | Adapted to request-scoped SSE without GET polling or event IDs. |
| Malformed and premature SSE termination | `client.rs` | Preserved with originating-request and subscription failures. |
| stdio sequencing and recovery | `stdio.rs` | Expanded for parse errors, invalid requests, mandatory metadata, cancellation, discovery, and EOF. |
| Tool list definitions and cursors | `server/tools.rs`, `server_utilities/pagination.rs` | Preserved with modern cache/result envelopes. |
| Tool call content, structured content, and error result | `server/tools.rs` | Preserved, including missing-name invalid parameters. |
| Prompt list/get payloads and required arguments | `server/prompts.rs` | Preserved with argument title and metadata additions. |
| Resource list/read text and blob payloads | `server/resources.rs` | Preserved; missing/unknown URI now uses active-spec invalid parameters. |
| Resource-template payloads | `server/resource_templates.rs` | Preserved with annotations, metadata, and cursors. |
| Resource URI subscriptions | `subscriptions.rs`, `unsupported.rs` | Unsupported filter is omitted and subscription/update methods explicitly fail. |
| Completion resource/prompt refs and context | `server_utilities/completion.rs` | Preserved with typed active-spec references and complete result envelope. |
| Completion missing argument and 100-value cap | `server_utilities/completion.rs` | Preserved. |
| Cursor omission, propagation, next cursor, and invalid cursor | `server_utilities/pagination.rs` | Preserved for all four list methods. |
| Global `logging/setLevel` | `transport.rs` | Removed and explicitly returns method-not-found. |
| Logging notification fields and filtering | `server_utilities/logging.rs` | Adapted to request-scoped log-level metadata. |
| Roots client capability and server-initiated request | `client.rs` | Replaced by roots-only MRTR input/retry state. |
| Sampling and elicitation placeholders | `unsupported.rs` | Still unsupported; capabilities/methods and non-roots MRTR are executable failures. |
| Task capability and execution shapes | `unsupported.rs` | Removed; capability omission and task methods are executable failures. |
| List-change delivery | `subscriptions.rs` | Expanded with acknowledgment ordering, filter negotiation, correlation IDs, and completion. |
| Content block variants | `common/content.rs` | Preserved for text, image, audio, links, and embedded text/blob resources. |
| Feature annotations and opaque metadata | `common/annotations.rs`, `schema.rs` | Preserved and expanded for modern icons and raw JSON Schema forms. |
| Bearer challenge and protected-resource metadata | `authorization.rs`, `oauth_protocol.rs` | Preserved and adapted to stateless POST. |
| Static/dynamic bearer injection and failures | `authorization.rs` | Preserved per POST, including cancellation, typed errors, and secret redaction. |
| Token expiry/revocation behavior | `authorization.rs`, `oauth_authorization_server.rs` | Expiry and revocation both directly terminate active request streams; hosted flow tests also reject revoked tokens. |
| OAuth metadata defaults and endpoint policy | `oauth_protocol.rs` | Preserved with active issuer and application-type rules. |
| Authorization response parsing | `oauth_protocol.rs` | Preserved with RFC 9207 `iss` acceptance. |
| CIMD identity/public profile and DCR normalization | `oauth_protocol.rs`, `oauth_authorization_server.rs` | Preserved and expanded for application type and immutable snapshots. |
| OAuth redirects, response bounds/types, and destination policy | `oauth_authorization_server.rs` | Preserved in hardened hosted integration coverage. |
| OAuth secret redaction | `oauth_protocol.rs` | Preserved for tokens and registration credentials. |

The old suite contained 142 tests, many of which asserted the removed session transport.
The active suite uses fewer tests while retaining focused positive, negative, and edge-case
coverage for every supported protocol area. Ecosystem and hosted/store OAuth tests remain
supplementary rather than substitutes for revision-scoped wire assertions.
