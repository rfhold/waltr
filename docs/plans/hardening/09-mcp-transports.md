# 09 MCP Transport Refactor

Status: Complete

Server: default-deny Origins, strict request ordering, persisted protocol negotiation, long-lived GET delivery. Client: incremental SSE, negotiated session state, backoff, graceful DELETE shutdown.

## Findings Resolved

- Origin policy defaults to accept-all; malformed Origins treated as absent; Waltr installs no policy: DNS-rebinding surface in auth-disabled mode (`server/mod.rs`).
- `axum::Json` extraction precedes handler validation, so malformed JSON can preempt the required 403.
- Initialize params unvalidated; protocol header optional and unpersisted; responses use the compile-time constant.
- GET closes immediately; client reconnects every 100 ms and retries 405/500/503 at the same rate; SSE parser buffers to EOF and is not standards-robust; shutdown sets a flag without cancel/join/DELETE (`client/transport/http.rs`, `sse.rs`).

## Server Changes (`server/mod.rs`)

### Origin

- Default: absent Origin allowed; any present Origin denied unless explicitly allowlisted via `StreamableHttpOptions`. Add `try_with_allowed_origins([...])` canonicalizing scheme/host/effective-port.
- Reject duplicate, malformed, non-UTF-8, `null`, path/query/fragment/userinfo-bearing Origins with 403. Never compare Origin to Host.
- Applied to POST, GET, DELETE before anything else; OAuth metadata routes stay public.

### Request pipeline

- POST accepts `Request<Body>`: order Origin -> Accept/content-type shape -> authorization -> session/header validation -> bounded body collection -> JSON-RPC parse -> store/handler work.
- Every method authenticates; owner + credential expiry flow into store operations and long-lived responses.
- Initialize: parse typed params; missing/invalid `protocolVersion`/`capabilities`/`clientInfo` -> invalid params, no session created; persist exactly the negotiated version from the `InitializeResult`.
- Subsequent requests: single valid header equal to persisted version, or absent header (persisted version applies); duplicate/malformed/mismatched -> 400; owner mismatch -> 404 first.
- `notifications/initialized`: claim -> handler (30s timeout, `ServerContext::idempotency_key()` = initialization operation id) -> atomic complete; duplicate -> 202 without re-invoking; concurrent unexpired claim -> 503 + `Retry-After`; abandoned claims recovered.

### GET delivery

- Hold the response open: validate + apply cursor ack via `prepare_delivery`; emit id-only priming frame (sequence 0) when no cursor presented; emit `retry: 1000`.
- Wake on process-local `tokio::sync::Notify` (append/delete) plus 1s read-only fallback poll for cross-process appends; no SQL transaction while emitting bytes.
- Send a claimed batch immediately, then close to force reconnect-with-ack; keepalive comment at 15s; close idle streams at 30s.
- Bearer-authorized streams end at credential expiry; session DELETE wakes active GETs.
- POST-response SSE remains non-resumable and carries no event IDs (explicitly documented).

## Client Changes (`client/transport/sse.rs`, `http.rs`, `connection.rs`, `api.rs`, `mod.rs`, `stdio.rs`)

### SSE decoder (`sse.rs`)

- Replace buffer-to-EOF parsing with an incremental decoder yielding frames before EOF: strict UTF-8 (split code points across chunks), LF/CRLF/CR, BOM, comments, first-colon field split with optional leading space, multiline data joined with `\n`, id-only frames surfaced, NUL-containing IDs ignored, decimal `retry`, bounded line/event buffers.

### HTTP transport (`http.rs`)

- Session state: `pending_session_id` promoted to `ActiveHttpSession { session_id, protocol_version, last_event_id }` only after validating `InitializeResult`; missing `protocolVersion` is an error; later responses cannot silently replace the session id; cursor resets with a new session.
- Advance `last_event_id` only after the event is delivered to the connection inbound channel.
- GET loop: honor server `retry`; exponential backoff on failures; stop on 405; surface/back off on 503; no fixed 100 ms loop. POST SSE readers run as tracked background tasks; `send()` returns after headers + spawn.
- On session 404: fail the in-flight operation without replay, serialize exactly one re-initialization handshake, then let subsequent operations use the replacement session.
- Shutdown: fallible end-to-end (`McpClient::shutdown() -> Result<()>` through connection/transport/stdio): enter ShuttingDown; cancel + join GET/POST readers; bounded wait for in-flight sends; authenticated DELETE with session id + negotiated version under a 5s timeout; treat 2xx/404/405 as success; local cleanup always; idempotent under concurrent calls; attempt DELETE when initialization fails after a session id was issued.

## Tests

- Spec/transport: default-deny Origin matrix (absent ok; exact allowlisted ok; scheme/port/host/malformed/null/duplicate denied; 403 precedes malformed JSON, auth, and store access); protocol-version lifecycle (missing initialize version creates nothing; persisted version enforced; absent header ok; mismatch 400; reconstruction retains validation).
- Resumability: disconnect before reading -> replay with same IDs; partial read + reconnect with last ID -> suffix after lease expiry; two concurrent GETs never duplicate a message; POST SSE has no IDs.
- Client: decoder unit matrix (1-byte chunks, split UTF-8, CR/CRLF, comments, BOM, id-only, retry, NUL id, invalid UTF-8, oversized); first event delivered while body stays open; cancellation joins a blocked reader; retry honored instead of 10 Hz; shutdown sends exactly one DELETE with correct headers; 401/network DELETE failures return errors after local cleanup; init-failure cleanup.
- Ecosystem interoperability suites still pass against rmcp.

## Exit Criteria

- Present Origins denied by default and validated first.
- Idle clients generate no write churn and no 10 Hz loops.
- Long-lived SSE consumed incrementally; resumability holds across disconnects.
- Graceful shutdown deletes durable sessions; session termination recovers without replaying mutations.

## Verification

```
cargo test -p mcp --test spec
cargo test -p mcp --tests
cargo test -p mcp --features streamable-http-sqlite --test streamable_http_sqlite_store
```
