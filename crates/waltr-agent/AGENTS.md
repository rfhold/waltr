# Boundaries

Waltr agent is an isolated agent library.

`src/mcp/` owns Waltr-side MCP session integration: runtime adaptation, generated tool names, prompt/resource template wiring, progress routing, and conversion into agent tools/templates. Generic MCP protocol behavior belongs in `../mcp/`.

# Contracts

Agent code should not depend on client, server, TUI, or web layers.

Agent code may depend on the generic `mcp` crate. The `mcp` crate must not depend back on `waltr-agent`.

# Hints

Use the agent-modification skill when modifying the isolated agent library.
Use the mcp-modification skill for generic MCP protocol, client/server, transport, trace propagation, or spec-test changes.
