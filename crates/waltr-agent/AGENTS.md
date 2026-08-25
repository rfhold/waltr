# Boundaries

Waltr agent is an isolated agent library.

`src/mcp/` owns Waltr-side MCP session integration: runtime adaptation, generated tool names, prompt/resource template wiring, progress routing, and conversion into agent tools/templates. Generic MCP protocol behavior belongs in `../mcp/`.

# Contracts

Agent code should not depend on client, server, TUI, or web layers.

Agent code may depend on the generic `mcp` crate. The `mcp` crate must not depend back on `waltr-agent`.

Session lifecycle and list projection are centralized in `src/agent/session/control.rs` and `src/agent/session/list.rs`. Acquire lifecycle before the per-session mutation gate, and enter the synchronous list projection only after both; never hold a synchronous mutex across an await. Pass coordinator-owned run services through `SessionRunContext` rather than adding independent gates or broadcasters to the run loop.

# Hints

Use the agent-modification skill when modifying the isolated agent library.
Use the mcp-modification skill for generic MCP protocol, client/server, transport, trace propagation, or spec-test changes.
