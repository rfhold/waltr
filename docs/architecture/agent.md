# Agent

Waltr agent architecture keeps model interaction, prompt assembly, tool execution, permissions, middleware, and durable session replay in `crates/waltr-agent/`. The crate is an isolated library: it can be used by hosts, tests, or future runtimes without depending on the server, shared client, TUI, or web layers.

## Responsibilities

| Area | Owns |
| --- | --- |
| Agent configuration | `AgentBuilder`, selected model, enabled tools, middleware chain, and permission policy. |
| Agent definitions | Strict YAML parsing, resource registration, inheritance, reference resolution, and resolved runtime inputs. |
| Provider abstraction | Provider-neutral requests, stream events, tool calls, tool results, thinking blocks, and provider metadata. |
| Provider adapters | OpenAI and Anthropic request encoding, streaming, parsing, and provider-specific continuation data. |
| Agent loop | Turn progression, provider streaming, timeline events, tool execution, permission checks, and host-facing `AgentTimelineEvent` output. |
| Tools | Built-in filesystem, question, skills, task, MCP, and optional shell tool integration. |
| Permissions | Generic permission claims, resource matching, policy evaluation, and allow/ask/deny behavior before tool execution. |
| Middleware | Request construction hooks, prompt assembly, tool visibility and metadata, claims, skills, `AGENTS.md`, and suspension hooks. |
| Durable sessions | Event-sourced session records, snapshots, suspensions, resume inputs, and storage backends. |

## Boundaries

`crates/waltr-agent/` is a normal Rust library boundary. Code in this crate should not depend on `waltr-server`, `waltr-client`, `waltr-tui`, or `waltr-web`.

Adjacent layers should treat the agent as a hostable capability:

| Layer | Relationship To Agent |
| --- | --- |
| Server | Hosts and coordinates agent sessions through `SessionCoordinator`, while server state, authorization, and RPC/MCP contracts stay outside the agent crate. |
| Shared client | May render or control server-exposed agent state, but shared client reducers stay outside the agent crate. |
| Web and TUI | May present agent output, prompts, approvals, and session state, but UI routing and rendering stay outside the agent crate. |
| Tests | Exercise the agent directly through scripted providers, live providers, tools, and storage backends. |

The agent crate owns provider-neutral domain types and host-tool contracts. Hosts own policy choices, UI presentation, server APIs, process lifecycle, and runtime wiring around the library.

## Agent Definitions

`waltr-agent` accepts strict, multi-document YAML containing `waltr.dev/v1alpha1` `Agent`, `FeatureSet`, and `PermissionSet` resources. Unknown fields, unsupported values or constructs, malformed YAML, empty required names, and unsupported API versions fail parsing. A single YAML source may not repeat the same `(kind, metadata.name)` resource. Loading a later source replaces an earlier resource with the same kind and name, which lets the server layer define source precedence.

The implemented resource model is:

| Kind | Implemented fields |
| --- | --- |
| `Agent` | `metadata.name`; optional `spec.description`, `spec.systemPrompt`, and `spec.inheritsFrom.name`; ordered `spec.features` and `spec.permissions` entries. |
| `FeatureSet` | Optional `filesystem`, `agents`, `skills`, `shell`, `task`, and `mcp` feature configurations. Workspace-root fields accept only `$(session.workspace.root)`, the filesystem profile is `auto`, and the instruction filename is `AGENTS.md`. MCP selection and trace propagation belong to the `mcp` feature. |
| `PermissionSet` | Ordered `spec.tools` entries selected by exact tool `name` or by implemented `feature`, with whole-tool `allow`, `ask`, or `deny`, or filesystem path and shell command resource rules. |

See [the agent-definition examples](../examples/agents/README.md) and the [bundled server definitions](../../crates/waltr-server/agent-definitions.yaml) for concrete implemented resources.

Resolution is parent-first. Nonempty parent and child system-prompt fragments compose in that order, separated by a blank line. The parent fully composes before the child applies each list left-to-right. Every `spec.features[]` entry contains exactly one `featureSetRef: { name }` or one inline `filesystem`, `agents`, `skills`, `shell`, `task`, or `mcp` feature. A later feature replaces that whole feature configuration. Every `spec.permissions[]` entry contains exactly one `permissionSetRef: { name }` or one inline tool `name`/`feature` selector and permission. Permissions append in composition order and use the existing conflict rules. Missing references and inheritance cycles fail resolution.

```yaml
spec:
  features:
    - featureSetRef: { name: coding }
    - shell:
        root: $(session.workspace.root)
        systemPrompt: false
        defaultPermissions: true
  permissions:
    - permissionSetRef: { name: coding }
    - name: deploy
      permission: ask
```

The server owns definition loading and selection. It loads its compile-time source, then optional external sources, compiles every final valid agent into a name-sorted startup catalog, and marks the configured default. Definition files remain startup inputs: the server reads them only during startup and has no watcher, shared TOML, or RPC editor. `ListAgents` returns catalog names and descriptions plus the default name.

Each new top-level session stores an `AgentDefinitionBundle` with a selected catalog name and the exact ordered YAML source texts. Current clients require an explicit selection; omission in the older create RPC shape uses the configured default. Session summaries project the effective agent name. Legacy sessions without a bundle also project and use the configured default without being backfilled.

`UpdateSessionAgent` accepts an exact catalog name and persists the new bundle immediately. An active runtime continues with its already captured bundle. The next captured cycle, including queued or resumed work, uses the new agent; later template searches also reconstruct from it. History is preserved. The new prompt identity materializes a new base prompt on that cycle, approvals remain subject to the new policy, and task children inherit the cycle-captured bundle. MCP replacement waits for the next reconstruction and structural-fingerprint decision.

Multiple switches before a capture boundary coalesce from the first old name to the final name. The next cycle appends one duplicate-safe JSON `agent_switch` event on the `session_info` channel and clears the pending marker. A malformed present bundle fails reconstruction instead of falling back.

## Durable Ownership

`SessionRecord.inference_settings` stores only `InferenceSettings`. Its exact durable shape is one `provider: InferenceProviderSettings` value with these fields:

| Field | Durable value |
| --- | --- |
| `key` | Provider configuration key. |
| `api` | Provider API family. |
| `url` | Provider endpoint URL snapshot. |
| `model_id` | Provider model identifier. |
| `model_cost` | Optional model cost snapshot. |
| `headers` | Provider header templates. |
| `oauth` | Optional provider OAuth refresh settings. |
| `reasoning_effort` | Optional OpenAI reasoning effort. |

Session JSON uses the `inference_settings` field. The old `runtime_settings` field is unsupported. Waltr does not alias or backfill it, so an old field does not populate inference settings.

The selected definition bundle supplies the resolved base prompt, features, permissions, and MCP selection at reconstruction. Durable middleware state preserves the prompt materialized for that definition identity, captured `AGENTS.md` content, skill catalog, and loaded skill bodies. Task children inherit the parent's inference settings and cycle-captured bundle.

The schema supports strict nested input permissions. `permission.inputs` is a nonempty list; each rule has a nonempty `match` list whose conditions are ANDed. Conditions use canonical RFC 6901 pointers and exactly one value matcher: an exact typed JSON scalar (`null`, boolean, number, or string) or a string wildcard containing exactly one terminal `*`. Missing pointers do not match and leave the baseline or another rule in effect. A selected object or array denies closed. Matching precedence is condition count, exact-condition count, total wildcard-prefix length, effect conflict rank, then later composed position.

```yaml
permissions:
  - name: waltr
    permission:
      inputs:
        - match:
            - pointer: /action
              value: { wildcard: "sessions.*" }
            - pointer: /options/dryRun
              value: { exact: false }
          effect: ask
```

Routine/routine tools, deferred tools, and harness tools are not implemented. Generic MCP selection remains definition feature behavior. Unsupported fields fail strict parsing rather than being ignored. The removed `featureSetRefs` and `permissionSetRefs` fields have no alias or migration: old external definitions and stored source bundles that use them fail.

## Runtime Flow

An agent run starts from an `Agent` built with `AgentBuilder`. The run binds that configuration to a provider implementing `LlmStream`, a prompt or resume input, and either an ephemeral or durable session store.

```mermaid
flowchart TD
    Host[Host runtime or test]
    Agent[Agent config]
    Middleware[Middleware chain]
    Provider[LlmStream provider]
    Policy[PermissionPolicy]
    Tools[Agent tools]
    Store[SessionStore]

    Host -->|prompt or resume input| Agent
    Agent -->|build request| Middleware
    Middleware -->|CompletionRequest| Provider
    Provider -->|CompletionEvent stream| Agent
    Agent -->|tool call| Policy
    Policy -->|allow, ask, or deny| Agent
    Agent -->|allowed call| Tools
    Tools -->|ToolResult| Agent
    Agent -->|timeline and journal records| Store
    Agent -->|AgentTimelineEvent stream| Host
```

The loop persists session progress before provider dispatch and while consuming provider events. Provider text and thinking deltas become durable timeline events. Tool-call previews are recorded and streamed before tool execution. A provider turn may include multiple tool calls; the loop records every preview before executing the collected calls. Generic tool progress can stream while each tool runs, and each final tool result is persisted as soon as that tool completes. The provider turn is marked finished only after all collected tool calls have resolved, so interruptions preserve completed results and cancel only unfinished work. Final tool results are returned to the provider as replayable tool results in later turns.

Suspending work is part of the runtime contract. Middleware or permission handling can stop progression with an external request, such as a permission approval or user question. The host later resumes the session with `ResumeInput`.

## Provider Boundary

`crates/waltr-agent/src/stream.rs` defines the provider-neutral request and event model:

| Type | Role |
| --- | --- |
| `LlmStream` | Trait implemented by provider adapters. |
| `CompletionRequest` | Prompt, optional system prompt, visible tools, and replayed turns sent to a provider. |
| `CompletionEvent` | Provider output normalized into text deltas, thinking data, tool calls, metadata, and finished signals. |
| `CompletionTurn` | Replayable turn history preserving thinking blocks, tool calls, tool results, and provider metadata. |
| `ToolDefinition`, `ToolCall`, `ToolResult` | Provider-neutral tool contract used by built-in tools, MCP tools, and hosts. Tool results preserve ordered text and validated inline-image blocks plus explicit error state. |

Provider adapters under `crates/waltr-agent/src/provider/` encode this neutral model into provider-specific protocols and parse provider-specific streams back into neutral events. Anthropic tool results carry native Base64 image sources, OpenAI Responses function outputs carry `input_image` items, and Chat Completions emits required text tool messages before labeled user image messages. Provider-native metadata and thinking payloads may be stored for continuation, but replay correctness should not depend on metadata being present.

## Tools And Permissions

Tools are opt-in capabilities added through `AgentBuilder` or MCP configuration. A default agent built with only a model has no host tools.

Built-in tool areas:

| Tool Area | Purpose |
| --- | --- |
| `tools::filesystem` | Controlled file operations rooted in configured workspace paths. |
| `tools::question` | Structured external questions that suspend the run until a host answer is provided. |
| `tools::skills` | Agent skill discovery and loading. |
| `tools::task` | Task-oriented agent tool support. |
| `tools::shell` | Optional shell execution tool behind the `shell` feature. |
| `mcp` | Waltr-side Model Context Protocol integration exposed as agent tools and templates. |

Tool execution passes through generic permission claims. Tools and middleware describe requested operations as `PermissionClaim` values, and `PermissionPolicy` evaluates those claims by tool selector, operation class, filesystem path, shell command, or resource pattern.

Resolved agent-definition permissions start deny-by-default, which also keeps tools without an effective permission out of provider requests. `defaultPermissions: true` on filesystem, skills, shell, task, or MCP installs allow rules for the concrete tools owned by that enabled feature; omitting it or setting it to false leaves those tools denied unless a permission-set entry exposes them. Permission-set selectors can name one concrete tool or all concrete tools generated by an implemented feature. Ordered resource rules support `outsideRoot: $(session.workspace.root)` for filesystem claims and executable names for shell claims.

The bundled `coding` definition enables default permissions for its feature-owned tools, then asks before filesystem or shell access outside the session workspace. It also asks before shell commands named `git`, `pulumi`, `kubectl`, `terraform`, `gcloud`, `aws`, `gh`, `help`, `pyinfra`, `ansible`, `sudo`, or `ssh`, regardless of their arguments.

Permission outcomes are runtime behavior, not UI behavior:

| Outcome | Meaning |
| --- | --- |
| `Allow` | Execute the tool and record the result. |
| `Ask` | Suspend with an external permission request for the host to answer. |
| `Deny` | Return a permission error instead of executing the tool. |

Whole-tool `deny` and `ask` gate execution before tool middleware. After an allowed whole-tool gate, input and resource checks evaluate the final arguments and claims produced by the complete pre-tool middleware chain. A matching input `ask` records reusable approval claims only for the exact tool, pointer, and typed scalar observed after middleware; a wildcard policy never creates a wildcard durable approval. Permission state writes version 2, reads legacy version 1 or an absent version, and rejects unknown versions. Stored approvals do not override a newly applicable deny.

## Middleware

Middleware is the internal extension point for request construction, tool handling, and suspensions. Configured middleware behavior is exposed through builder options and public agent features.

Middleware can:

- Add or edit system prompt fragments.
- Add, remove, or annotate tools before provider request assembly.
- Observe or alter provider-neutral requests.
- Observe or replace tool calls and tool results.
- Suspend a run with an external request.

`AgentBuilder::build` assembles middleware in deterministic order. The stateful system prompt middleware runs first, feature and custom middleware run in registration order, `AGENTS.md` instruction middleware runs after that when configured, and skills middleware runs last when configured. This ordering is part of the request-construction contract because it can affect provider request hashes and tool visibility.

Durable middleware state preserves first-execution prompt inputs across reconstruction. After the first execution, the stored fully composed system prompt and captured `AGENTS.md` content override newly composed values. The first execution also stores the discovered skill catalog. Individual skill content becomes durable only when that skill loads; Waltr does not pin every skill body during initial discovery.

## Durable Sessions

Agent sessions are event-sourced. A session store persists cursor-assigned timeline events and internal `SessionJournalEvent` records identified by `SessionId` and guarded by revisions. Runtime snapshots are derived from the persisted journal and are used to resume with a new prompt, external response, tool results, or interruption. Inline tool-result images remain in the durable provider-neutral turn for replay. Provider-request audit records redact image data and retain safe media, size, and digest metadata; server timeline projections likewise omit Base64.

`waltr-server` currently hosts sessions and exposes the same registered session service and coordinator through gRPC and the MCP `sessions` tool. MCP supports list, get, interrupt, wait, send, and wipe operations. Both surfaces must share that coordinator because active runs, queues, and live broadcasts are process-local.

The global session wipe directly removes every raw record from the configured session store, clears cached session MCP runtimes, and publishes one session-list cleared event. It does not inspect, reject, cancel, wait for, or gate running or queued work. Operators must stop session activity first; work that continues concurrently may recreate records after deletion. The gRPC and MCP surfaces invoke this same coordinator operation and require explicit confirmation.

Session storage implementations include memory, filesystem, OpenDAL-backed stores, S3, Scylla, and PostgreSQL through OpenDAL helpers. Storage details are below the `SessionStore` trait; the agent loop only depends on the trait contract.

Crash recovery is retry-the-turn. The request is journaled before provider dispatch, while parser progress inside an in-flight provider stream is not persisted as an authoritative checkpoint.

## Dependencies

The agent crate depends on provider, storage, protocol, and async infrastructure crates such as `reqwest`, `tokio`, `mcp`, `opendal`, `aws-sdk-s3`, `scylla`, `tokio-postgres`, and provider protocol helpers. The optional `shell` feature adds shell parsing dependencies through `tree-sitter` and `tree-sitter-bash`.

These dependencies stay inside the agent layer. Higher layers should use the public agent API instead of depending on provider adapter internals, middleware internals, or storage implementation details.

## Dependents

Current and expected dependents include:

| Dependent | Uses |
| --- | --- |
| Agent tests | Scripted-provider, live-provider, session, permission, MCP, filesystem, shell, question, and loop coverage. |
| Host runtimes | Agent configuration, timeline streams, durable sessions, suspensions, and resume inputs. |
| UI or server layers | Public timeline events after a host layer chooses how to authorize and expose them. |

## Invariants

- Agent code must not depend on server, shared client, TUI, or web crates.
- Provider adapters must cross the provider boundary through provider-neutral request and event types.
- Built-in host tools are opt-in; a default agent has no filesystem, shell, MCP, skills, task, or question tools unless configured.
- Permission claims must be evaluated before executing host tools that can read, write, delete, execute, or otherwise affect external resources.
- Middleware assembly order must remain deterministic.
- Durable session replay must be based on persisted timeline/journal records and derived projections, not provider-specific transient parser state.
- Provider-native metadata is an optimization for continuation and compatibility, not a requirement for replay correctness.

## Tests

Agent coverage is split between deterministic tests and ignored live-provider tests. See [../testing.md](../testing.md) for canonical commands.

Focused paths:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent/tests/agent.rs](../../crates/waltr-agent/tests/agent.rs) | Deterministic agent integration test entry point. |
| [../../crates/waltr-agent/tests/agent/](../../crates/waltr-agent/tests/agent/) | Deterministic loop, session, permission, MCP, and question behavior. |
| [../../crates/waltr-agent/tests/live.rs](../../crates/waltr-agent/tests/live.rs) | Ignored live-provider test entry point. |
| [../../crates/waltr-agent/tests/live/agent/](../../crates/waltr-agent/tests/live/agent/) | Live-provider agent flows for tools, storage, context, permissions, MCP, shell, and filesystem behavior. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/AGENTS.md](../../crates/waltr-agent/AGENTS.md) | Local crate boundary guidance. |
| [../../crates/waltr-agent/src/lib.rs](../../crates/waltr-agent/src/lib.rs) | Public exports and module boundary. |
| [../../crates/waltr-agent/src/agent/builder.rs](../../crates/waltr-agent/src/agent/builder.rs) | Agent construction, tools, permissions, prompt, MCP, and middleware assembly. |
| [../../crates/waltr-agent/src/agent/loop.rs](../../crates/waltr-agent/src/agent/loop.rs) | Internal run loop, provider streaming, tool execution, permissions, and timeline/journal persistence. |
| [../../crates/waltr-agent/src/agent/session/mod.rs](../../crates/waltr-agent/src/agent/session/mod.rs) | Durable session API surface. |
| [../../crates/waltr-agent/src/stream.rs](../../crates/waltr-agent/src/stream.rs) | Provider-neutral request, event, turn, tool, thinking, and metadata types. |
| [../../crates/waltr-agent/src/provider/mod.rs](../../crates/waltr-agent/src/provider/mod.rs) | Provider adapter exports. |
| [../../crates/waltr-agent/src/middleware/mod.rs](../../crates/waltr-agent/src/middleware/mod.rs) | Middleware hook contract. |
| [../../crates/waltr-agent/src/tools/mod.rs](../../crates/waltr-agent/src/tools/mod.rs) | Built-in tool modules. |
| [../../crates/waltr-agent/src/mcp/mod.rs](../../crates/waltr-agent/src/mcp/mod.rs) | Waltr-side MCP integration boundary. |
| [../../crates/mcp/](../../crates/mcp/) | Generic MCP protocol crate. |
| [../../crates/waltr-agent/src/permission/mod.rs](../../crates/waltr-agent/src/permission/mod.rs) | Permission policy, claims, matching, and effects. |
| [../../crates/waltr-agent/src/definition/mod.rs](../../crates/waltr-agent/src/definition/mod.rs) | Strict agent-definition registry, schema model, and resolution. |
| [../../crates/waltr-server/agent-definitions.yaml](../../crates/waltr-server/agent-definitions.yaml) | Bundled server agent definitions. |
| [../agent/README.md](../agent/README.md) | Index of detailed agent feature docs. |
| [../mcp/README.md](../mcp/README.md) | Generic MCP protocol documentation. |
