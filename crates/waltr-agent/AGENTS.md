# Index

| Path | Info |
| --- | --- |
| [src/agent/](src/agent/) | Generic runner, sessions, coordination, cancellation, completion, and durable host fences. |
| [src/provider/](src/provider/) | Provider inference encoding, streaming, and authentication. |
| [src/middleware/](src/middleware/) | Neutral request/context hooks, not concrete host policy. |
| [../waltr-server/src/agent_host/](../waltr-server/src/agent_host/) | Concrete definitions, tools, templates, middleware, Skills, worker, and MCP integration. |
| [../../docs/architecture/agent.md](../../docs/architecture/agent.md) | Canonical runner/host boundary. |

# Boundaries

Waltr agent is an isolated agent library.

No MCP, permission engine, built-in tools, shell feature, Skills, concrete middleware, or worker implementation belongs here. Definitions are opaque JSON interpreted by the server. Workspace effects execute in `../waltr-agent-sandbox/`; server `agent_host/` owns its gateway and shared dispatcher. Generic protocol belongs in `../mcp/`.

# Contracts

Agent code should not depend on client, server, TUI, or web layers.

Keep provider inference auth, generic session/history/coordinator, neutral hooks, cancellation/completion, and durable host dispatch fences/receipts here. Persist the host fence before polling an effect; unknown dispatches never automatically replay. Private approvals and interactions belong to the server broker, never session metadata, journal, or timeline. Do not add a local workspace fallback or an external-response resume API.

Session/item metadata remains opaque. `SessionMutation` checks revision, context, incarnation and metadata predicates with dependent masks atomically. Preserve absence versus null and canonical item annotations across copies/projections. Category schemas, checkpoints, mask names and background policy belong to Routine JavaScript. Generic post-commit events carry no helper/task policy; trusted server-local roles own that classification.

Keep public complete-record revisions separate from incarnation-bound execution tokens. Adopt atomic append receipts before publication; never infer public revisions from local arithmetic. Metadata-only changes preserve execution freshness; journal, masks and actual configuration changes invalidate it. Strict public CAS/context predicates remain mandatory. Preserve legacy JSON through read-only sequence seeding and the next authorized write; never invent incarnations or reset retained stores. See [revision and compatibility contracts](../../docs/agent/storage.md#revision-domains-and-append-receipts).

Session lifecycle and list projection are centralized in `src/agent/session/control.rs` and `src/agent/session/list.rs`. Acquire lifecycle before the per-session mutation gate, and enter the synchronous list projection only after both; never hold a synchronous mutex across an await. Pass coordinator-owned run services through `SessionRunContext` rather than adding independent gates or broadcasters to the run loop.

Source-backed Routine helpers persist rollback-aware provider-neutral inference history through the latest completed turn, followed by the task prompt. Ordinary provider requests derive from durable helper state; invocation-local attachments are the explicit exception below. Preserve replay prompts, assistant text, tool calls and results, and images. Exclude lifecycle, middleware, status, usage and cost, request audit, provider metadata, provider-native thinking payloads, rollback barriers, and active partial work. Provider adapters retain normal encoding rules.

`src/agent/completion.rs` defines optional consumer-neutral completion validation. Rejections append durable feedback to the same session. Stream deltas remain previews; only accepted snapshots form committed helper output. Active validation markers forbid restart replay without the original callback.

Server `agent_host/authorized_tools.rs` supplies exact concrete dispatch through the shared host dispatcher. Ask awaits the original live operation. `SessionExecutionOwner::close` joins coordinator work after dropped consumer futures; hosts must call cleanup.

The server interprets `codeMode` and injects optional code tools only under explicit grants. The agent owns the neutral `HostDispatchBinding` fence, not concrete policy. Physical dispatch records survive logical rollback; interrupted associations reject resume before effects. Model visibility never grants authority.

`src/transient_input.rs` owns nonserializable, accounted invocation-local bytes. Hosts explicitly obtain `TransientInputBudget::process()`, transfer validated PNG/JPEG/WebP, opaque authorized native PDF, or exact UTF-8 bytes, and pin model capabilities and the durable initial-task cursor/turn/prompt index. Bind only a per-run builder or `SessionExecutionOwner::helper_with_transient_input`; do not put payloads in durable input, middleware, copied context, or tool images. The adapter retains them across live turns at the initial task position, not corrective prompts. Interrupted `transient-input` markers reject resume before templates or effects even without a completion observer; successful completion clears only the active fence, never grants replay authority.

Transient bounds are four attachments, 8 MiB each, 16 MiB source/run, 64 MiB shared source credit, and 32 MiB serialized request bodies (or a stricter caller-pinned provider/model limit). Source credits charge owned vector capacity, including unused slack, and survive Arc clones. Two fail-fast encoder permits bound concurrent attachment requests through transport-stream teardown; idle bound providers consume no encoder permit. Exact JSON sizing includes escaping and base64 expansion before binary encoding and send. Serialization allocates the measured body length. These bounds cover the attachment path, not total agent memory, MCP catalogs, storage, provider decoders, or process RSS. Retain owned-driver cancellation and join.

Attachments use the normal runtime, middleware, MCP/Skills policy, and SessionStore contract. No store-specific eligibility or structural admission applies. Keep model/API preflight and the early durable non-replay fence; do not add a second agent loop or disable Skills for attachment requests.

Transient `project_request` is explicitly a metadata-only, non-replayable view, produced without wire/base64 construction; ordinary text projections retain their exact-payload contract. Audits contain SHA-256/type/size and target identity, never attachment text or bytes. Anthropic Messages uses native image/document PDF/text parts; OpenAI Chat uses image URLs/PDF file/text parts; Responses HTTP and WebSocket use input-image/input-file/input-text parts. Other file types, conversions, extraction, URLs/remote IDs, and uploads are not supported. PDF authorization is not a PDF validation/sanitizer claim. Generated model output can quote input. Local ephemerality and `store:false` do not guarantee provider retention behavior.

Use `src/outbound_http.rs` for OpenAI/Anthropic inference and provider OAuth refresh. Server MCP orchestration retains the same TLS policy outside this crate. Android uses ring, TLS 1.2/1.3, bundled `webpki_roots`, and normal chain/hostname verification, not platform/user/private roots or `SSL_CERT_FILE`. Preserve non-Android reqwest defaults. OpenAI websocket uses its separate explicit ring/bundled-root connector everywhere. Generic MCP defaults remain platform-independent.

# Hints

Use the agent-modification skill when modifying the isolated agent library.
Use the mcp-modification skill for generic MCP protocol, client/server, transport, trace propagation, or spec-test changes.
