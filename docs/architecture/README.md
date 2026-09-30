# Architecture

Architecture docs describe Waltr's layer boundaries, responsibilities, data flow, and cross-crate contracts.

| Document | Covers |
| --- | --- |
| [access-authentication.md](access-authentication.md) | Waltr access authentication, hosted MCP authorization, browser/native/MCP flows, security boundaries, and lifecycle. |
| [agent.md](agent.md) | Agent library, private worker reconnect, prompt-template tools, and provider tool dispatch fence. |
| [clients.md](clients.md) | Shared client, web, and TUI architecture boundaries. |
| [config.md](config.md) | Unified YAML resource catalog, exact-path administration, snapshots, and validation. |
| [persistence.md](persistence.md) | Server database selection, schema ownership, durable OAuth/session state, migrations, and startup readiness. |
| [routines.md](routines.md) | Routine definitions, durable invocation lifecycle, replay, restart policy, clients, and UI boundaries. |
| [sandbox-reconnect-plan.md](sandbox-reconnect-plan.md) | Agreed implementation work and failure semantics for process-local sandbox reconnects and template tools. |
| [workspace-agent-orchestration-vision.md](workspace-agent-orchestration-vision.md) | Long-term vision for workspaces, subagents, sidecars, and event processing. |

Deployment configuration and operational startup behavior are documented in [../deployment.md](../deployment.md).
