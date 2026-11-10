# Construction Inputs

## Invariant

When several callers assemble the same service or runtime, keep its required inputs explicit and coherent across the construction boundary. Review both accumulating `*_with_*` constructor variants and loosely typed positional inputs (including builder callbacks): name the dependencies as one request/context when that clarifies ownership and prevents misordered forwarding. A required capability must not silently default to a disconnected placeholder just to preserve an older entry point. Distinct constructors are justified when they represent genuinely different lifecycles or ownership, not merely another optional dependency; a long signature alone does not require a struct.

## Why It Matters

Positional forwarding through multiple wrappers makes it hard to see which host owns a dependency or whether a caller supplies it at all. A fake default can leave a service constructible but unable to perform work that requires the capability. Cohesive inputs make required and optional values visible without multiplying entry points for the same lifecycle. [SessionCreateRequest](../../../../crates/waltr-agent/src/agent/session/create.rs) gathers one durable session creation input; [SessionRunContext](../../../../crates/waltr-agent/src/agent/session/control.rs) carries coordinator-owned run services. These are examples of meaningful bundles, not a rule to wrap every signature.

## Source Files To Inspect

The generic factory accepts neutral host contracts. The server host factory owns concrete capabilities, policy, broker, and worker dependencies.

- [Server session service constructors](../../../../crates/waltr-server/src/session/service.rs)
- [Server session registration and forwarding](../../../../crates/waltr-server/src/session/mod.rs)
- [App builders and session-service callback](../../../../crates/waltr-server/src/app.rs)
- [Agent session runtime factory](../../../../crates/waltr-agent/src/agent/session/runtime_factory.rs)
- [Server host runtime builder](../../../../crates/waltr-server/src/agent_host/builder.rs)
- [Server host runtime factory](../../../../crates/waltr-server/src/agent_host/runtime_factory.rs)
- [Server workspace request gateway](../../../../crates/waltr-server/src/agent_host/sandbox_gateway.rs)
- [Server prompt template runtime](../../../../crates/waltr-server/src/agent_host/template.rs)
- [Server provider-only credential adapter](../../../../crates/waltr-server/src/auth/credentials.rs)
- [Server live interaction broker](../../../../crates/waltr-server/src/interaction.rs)
- [SessionCreateRequest](../../../../crates/waltr-agent/src/agent/session/create.rs)
- [SessionRunContext](../../../../crates/waltr-agent/src/agent/session/control.rs)

## Common Drift Signs

- A new `from_sessions_with_agent_state_and_sandbox`-style variant only appends arguments to an existing construction path, while wrappers continue to duplicate positional forwarding.
- A long callback passes `ConfigHandle`, credentials, agent state, event sink, sandbox handle, and root through layers without clarifying which layer owns or requires each input.
- A workspace request uses operation strings, placeholder arguments, or positional bounds instead of operation-specific fields at the gateway boundary.
- An older constructor reaches the same runtime by supplying a disconnected sandbox handle instead of requiring the caller to provide the capability or making a truly separate lifecycle explicit.
- A one-field context only forwards another context, a request groups unrelated values only to shorten a signature, or separate constructors for genuinely different ownership are collapsed into one bag of optional fields.
- A generic agent constructor acquires concrete MCP, permission, worker, or upstream credential dependencies instead of neutral host contracts.
- A host constructs a second broker or policy instance, so RPC observers cannot reach the original live operation.
- A Routine adapter interprets category metadata, hides helper tasks/budgets, or selects workflow policy by bundled Routine name.
- A host treats public metadata as helper/task provenance, original-history access or private approval evidence.
- SQL publishes a program/text reference before immutable payload verification, or a missing object silently falls back to catalog bytes.
- Embedded storage reads standalone environment defaults, or a schema upgrade converts or deletes retained Routine state without separate authority.

## Suggested Tests

At the owning layer, exercise each supported construction path with its real required inputs and check that missing required capabilities cannot be hidden by a placeholder. For cross-layer assembly, test that the host-provided capability reaches the runtime and that loss of the worker does not turn into a local fallback. If two constructors intentionally have different lifecycles, test the distinguishing behavior rather than their argument count.

## Finding Format

`[construction-inputs] <path>:<line> <variant/positional request/default> obscures <required input or owner>; <concrete caller/path> can <miswire or bypass the capability> because <evidence>.`
