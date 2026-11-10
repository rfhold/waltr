# Optional code API

`CodeModeHost` adapts agent and ordinary MCP calls to the current `InvocationService`.
It does not create another engine, workspace worker, or tool-policy loop.
Production route registration remains outside this module.
The production composition installs a principal-scoped factory for actual authenticated gRPC and hosted MCP session runs, retains that binding across queued work, and uses only explicit operator grants. Hosted MCP grants match the configured hosted issuer and validated opaque OAuth subject; the original token expiry and revocation lease remain authoritative after the HTTP response. Local-protocol admission cannot fabricate that origin. Public MCP `code` remains off unless the operator sets `RoutineCodeGrant.public_mcp_session` to one of that grant's exact durable sessions. That session must also opt into code mode. Each request resolves the normal runtime and binds its own original authorization proof; no globally shared caller adapter or guest-selected profile is installed.

Hosted session submissions have a bounded server-owned completion task. Original-token expiry or revocation cancels only that exact submitted run and awaits its existing owner, including queued cancellation. Dropping the HTTP waiter does not cancel valid work. A stale cancellation handle cannot affect a recreated session incarnation, and another valid request does not replace the first request's lease. Unknown owner completion closes shared admission; in-flight code uncertainty remains durable rather than becoming success or replay.

## Configuration and authority

An agent selects the API explicitly:

```yaml
spec:
  features:
    - codeMode: true
```

Absent or false leaves ordinary agents unchanged.
True requires an explicit runtime factory host provider.
The host grants one authenticated principal, session, and durable incarnation a `CodeProfile`.
Authentication alone grants no code authority.
The profile selects exact concrete targets, named helpers, and an independent permission restriction.
The expanded capability set must fit the existing 64-capability definition limit before a grant is installed or replaced.
The normal named policy and caller restriction intersect.
`mcp.exposeTools: false` does not grant hidden concrete tools.
Code mode hides eager concrete MCP schemas from the model but retains the internal exact inventory.
Apps-only tools remain unavailable.

`CodeModeHost::install` installs the normal session factory provider.
`CodeModeHost::bind` produces a principal-scoped public adapter.
`WaltrMcpServer::with_code_port` exposes that adapter through the optional ordinary `code` tool.
Hosts must not share a principal-scoped adapter across callers.
Request metadata, correlation, and response state grant no authority or retry identity.

## Calls and bounds

| Action | Contract |
| --- | --- |
| `discover` | At most 16 exact targets and descriptions; no schemas. |
| `schemas` | At most eight explicit exact targets per call. |
| `execute` | At most 32 KiB of JavaScript async-function body. |

Code uses the current Routine `host.call` API and the same service admission gate.
Concrete execution uses `SessionAuthorizedToolExecutor` and the complete normal agent lifecycle.
The current remote declaration must match the pinned declaration before concrete dispatch.
Direct concrete Ask fails before dispatch; code never resumes a JavaScript heap after consent.
Named helpers retain their normal permissions and live-consent behavior.
Explicit consent applies to the original owned helper even without a payload; approval never restarts the JavaScript call.
Nested helpers do not inherit a code host provider or grant.

The current Routine defaults remain: 30 seconds, bounded fuel and heap, and bounded host calls.
Code does not reset fuel or add ambient imports, filesystem, environment, or network access.
The outer JSON output has a 64 KiB limit.
Code returns chosen business data; concrete tool results retain their original durable content.

## Durability and cancellation

The neutral agent fence persists before the handler runs.
It binds the actual session incarnation, stable tool-call ID, reserved invocation ID, and exact source revision.
The SQL association persists before script admission or host effects.
Every action, admission, and host dispatch checks the current durable session incarnation, not only the captured record.
An earlier association never authorizes another dispatch, including after a lost ACK or changed source.
The early agent fence precedes resume input and templates.
Logical rollback does not remove the physical dispatch record.

The current invocation owner retains cancellation, cleanup, and join responsibility after a waiter disappears.
Finished driver tasks are joined before releasing accounting; join failure leaves sticky shutdown uncertainty.
Grant revocation and both authentication leases cancel the same owner.
Completion-store failure reports uncertainty rather than a false no-effects outcome.
Loss of the original MCP lease cancels and joins the original helper even after the response is dropped. In-flight helper cancellation can leave a durable uncertain outcome; it does not authorize replay.
There is no JavaScript checkpoint, automatic source replay, or general retry engine.

## Tests

`tests.rs` covers real JavaScript, fresh SQLite, loopback MCP, normal runtime composition, exact identities, partial effects, SQL faults, bearer expiry, original-owner permission/Skills consent, and cancellation.
The public MCP HTTP test lives in `mcp_service.rs`.
The guarded PostgreSQL migration/CAS test lives in `persistence/script_code_dispatch.rs`.
These tests use scripted model output and require no live provider or credentials.
