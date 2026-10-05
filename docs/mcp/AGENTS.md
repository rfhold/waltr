# Boundaries

Docs in this directory describe the generic `mcp` crate and protocol-level behavior.
The explicit exception is [apps-authoring.md](apps-authoring.md), which describes Waltr's static served-app addition workflow.
Keep that guide clearly Waltr-specific; it does not define generic MCP behavior.

Waltr agent session integration belongs in `docs/agent/mcp.md`. Server/client/web/TUI behavior belongs in those layer-specific docs.

# Contracts

Keep protocol docs aligned with `crates/mcp/` and its tests. Do not document Waltr-specific session defaults, UI behavior, or server config policy as generic MCP behavior.

Document `OAuthAuthorizationStore`, hosted public-client OAuth/DCR/CIMD, authorization leases, optional SQL features, immutable standard SQLx migration history in MCP-owned storage, and explicit client shutdown here. Waltr identity adapters and deployment selection belong in server documentation.

# Hints

Use the documenting skill when changing these docs.
Use the mcp-modification skill when a docs change requires code or test changes in `crates/mcp/`.
