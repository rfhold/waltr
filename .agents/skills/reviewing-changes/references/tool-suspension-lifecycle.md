# Tool Dispatch And Live Interaction Lifecycle

## Invariant

The generic agent owns neutral tool callbacks, durable tool results, interruption, and host dispatch fences.
The server's shared `CapabilityDispatcher` owns exact concrete dispatch and policy checks.
Permission, question, form, and URL-consent requests await the original live operation through `InteractionBroker`.
Private waits and approvals never enter metadata, middleware state, journal, timeline, or external-response resume inputs.

## Why It Matters

A live answer continues one exact operation; it does not rerun the model, template, JavaScript, or tool setup.
Atomic answers prevent duplicate effects. Durable host fences prevent automatic replay after uncertain dispatch.
Generic interruption and tool-result continuation remain separate from private approval state.

## Source Files To Inspect

- `crates/waltr-agent/src/agent/loop.rs`: neutral callbacks, durable fences, and tool results.
- `crates/waltr-agent/src/agent/session/`: history, generic continuation, and lifecycle ownership.
- `crates/waltr-server/src/agent_host/dispatcher.rs`: exact identity, middleware, final claims, and worker Prepare/authorize/Execute.
- `crates/waltr-server/src/permission/live.rs`: one-shot receipts, reusable patterns, deny precedence, and retirement.
- `crates/waltr-server/src/interaction.rs`: operation-bound live waits, atomic answers, snapshots, and owner cancellation.
- `crates/waltr-server/src/session/interactions.rs`: session authorization and typed response validation.
- `crates/waltr-server/src/agent_host/mcp/`: upstream continuations and authenticated external routes.
- `crates/waltr-server/src/agent_host/middleware/`: concrete before/after hooks and live waits.
- `docs/architecture/agent.md`: canonical runner/host contract.
- `docs/agent-host/permissions.md`: policy order and private approval lifetime.
- `docs/runbooks/server-capability-cutover.md`: coordinated upgrade and incompatible history boundary.

## Common Drift Signs

- An entry point bypasses the shared dispatcher or derives authority from a visible tool name.
- Whole-tool admission follows concrete middleware, or final input/claim checks use pre-middleware arguments.
- Middleware redirects the approved tool name or publishes an unfiltered Apps result after transformation.
- A worker ticket survives denial or cancellation, or worker loss triggers local execution fallback.
- A response lacks session, generation, request, or operation identity, or bypasses current session authorization.
- Concurrent responders both succeed, or an invalid answer consumes the pending request.
- An observer disconnect decides or cancels work; owner cancellation or drop leaves a pending request behind.
- Snapshot/watch consumers merge partial pending state instead of full-replacement snapshots with generation and revision.
- One-shot receipts escape their exact operation, capability, policy, final arguments, or claims.
- Reusable patterns escape their session incarnation, capability, or policy, or override deny.
- Restart or runtime retirement restores private grants or replays pending interactions.
- Public MCP continuations trust upstream keys instead of authenticated principal, exact call, and opaque `requestState`.
- URL consent triggers navigation, a fetch, or an assumed external completion.
- An effect starts before its applicable durable host fence, or an uncertain dispatch automatically replays.
- Concrete middleware loses before/after observation, or the loop fails to persist each completed tool result.

## Suggested Tests

Use deterministic server tests for deny-before-effect behavior, final claims, receipt scope, cancellation, and broker identity.
Cover concurrent answers, invalid-answer retention, stale generations, observer reconnect, owner drop, and restart without replay.
Cover typed forms, consent-only URLs, and external continuations that reject unrelated callers or missing routes.
Use generic agent tests for fence-before-effect order, uncertain-dispatch non-replay, interruption, and completed-result persistence.
Keep private broker waits out of durable suspension tests.

## Finding Format

`[tool-suspension-lifecycle] <path>:<line> bypasses <dispatch/policy/live-owner/fence> during <lifecycle step>; <evidence> permits <concrete risk>.`
