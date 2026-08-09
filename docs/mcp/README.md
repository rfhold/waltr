# MCP Docs

MCP docs describe Waltr's generic Model Context Protocol crate, protocol behavior, interoperability expectations, and tracing contracts.

| Document | Covers |
| --- | --- |
| [progressive.md](progressive.md) | Opinionated progressive MCP server macro pattern, explicit tool declarations, tool-scoped actions and runbooks, resources, filters, and auth helpers. |
| [protocol.md](protocol.md) | Generic `mcp` crate boundaries, typed client/server APIs, transports, protected resources, hosted OAuth/DCR/CIMD, optional one-shot resource-owner OIDC, persistence, revocation leases, shutdown, and tests. |
| [tracing.md](tracing.md) | MCP request spans, trace context propagation, and span fields. |

For Waltr agent session integration on top of the generic crate, see [../agent/mcp.md](../agent/mcp.md).
