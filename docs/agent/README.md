# Agent Docs

Agent docs describe Waltr's agent library features, provider integrations, tool behavior, and related contracts. The [agent architecture](../architecture/agent.md#private-workspace-boundary) defines the private workspace worker boundary shared by filesystem, shell, skills, and context operations.

| Document | Covers |
| --- | --- |
| [agents_md.md](agents_md.md) | `AGENTS.md` discovery and prompt contribution behavior. |
| [filesystem.md](filesystem.md) | Worker-backed filesystem tool behavior, permissions, and boundaries. |
| [mcp.md](mcp.md) | Waltr agent MCP integration, tool exposure, and template adaptation. |
| [middleware.md](middleware.md) | Agent middleware responsibilities and ordering. |
| [model-catalog.md](model-catalog.md) | Model catalog data and provider model metadata. |
| [otel.md](otel.md) | OpenTelemetry instrumentation behavior. |
| [permissions.md](permissions.md) | Permission policy and approval behavior. |
| [shell.md](shell.md) | Worker-backed shell behavior, command execution, and safety boundaries. |
| [skills.md](skills.md) | Worker-backed skill discovery and prompt/resource loading. |
| [storage.md](storage.md) | Agent session and storage behavior. |
| [timeline.md](timeline.md) | Durable agent timeline events, querying, and text retrieval contract. |
