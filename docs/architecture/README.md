# Architecture

Architecture docs describe Waltr's layer boundaries, responsibilities, data flow, and cross-crate contracts.

| Document | Covers |
| --- | --- |
| [access-authentication.md](access-authentication.md) | Waltr access authentication, hosted MCP authorization, browser/native/MCP flows, security boundaries, and lifecycle. |
| [agent.md](agent.md) | Agent library architecture and provider/tool boundaries. |
| [clients.md](clients.md) | Shared client, web, and TUI architecture boundaries. |
| [config.md](config.md) | Config ownership, persistence, watch behavior, and validation flow. |
| [persistence.md](persistence.md) | Server database selection, schema ownership, durable OAuth/session state, migrations, and startup readiness. |
| [workspace-agent-orchestration-vision.md](workspace-agent-orchestration-vision.md) | Long-term vision for workspaces, subagents, sidecars, and event processing. |

Deployment configuration and operational startup behavior are documented in [../deployment.md](../deployment.md).
