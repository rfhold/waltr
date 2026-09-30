# MCP Docs

MCP docs describe Waltr's generic Model Context Protocol crate, protocol behavior, interoperability expectations, and tracing contracts.

| Document | Covers |
| --- | --- |
| [progressive.md](progressive.md) | Progressive MCP tools, grouped actions, explicit Skills catalogs, resources, filters, and auth helpers. |
| [skills.md](skills.md) | Official Skills extension, embedded file manifests, raw-byte integrity, client opt-in, and host authority boundaries. |
| [protocol.md](protocol.md) | Generic `mcp` crate boundaries, typed client/server APIs, transports, protected resources, hosted OAuth/DCR/CIMD, optional one-shot resource-owner OIDC, persistence, revocation leases, shutdown, and tests. |
| [tracing.md](tracing.md) | MCP request spans, server metrics and lifecycle outcomes, trace context propagation, and telemetry fields. |

For Waltr agent session integration on top of the generic crate, see [../agent/mcp.md](../agent/mcp.md).
