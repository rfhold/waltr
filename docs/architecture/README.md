# Architecture

Architecture docs describe Waltr's layer boundaries, responsibilities, data flow, and cross-crate contracts.

| Document | Covers |
| --- | --- |
| [access-authentication.md](access-authentication.md) | Waltr access authentication, QR and same-device sign-in, hosted MCP authorization, and OAuth transport boundaries. |
| [agent.md](agent.md) | Generic runner, session commits, capability policy, workspace dispatch, and worker-side stdio MCP. |
| [clients.md](clients.md) | Shared client, web, TUI, and native app boundaries, including desktop microphone capture, playback, and Computer Use text output. |
| [config.md](config.md) | Unified YAML resource catalog, exact-path administration, snapshots, validation, and desktop shortcut configuration. |
| [persistence.md](persistence.md) | Server database selection, schema ownership, durable OAuth/session state, migrations, and startup readiness. |
| [routines.md](routines.md) | Routine definitions, invocation-owned MCP calls, saved helper images, terminal receipts, and client boundaries. |
| [Script engine boundary](../../crates/waltr-script/README.md) | Isolated JavaScript engine contracts, ownership, and limits, used by server Routines. |
| [sandbox-reconnect-plan.md](sandbox-reconnect-plan.md) | Historical reconnect design and failure semantics; current ownership and interaction contracts are in `agent.md`. |
| [workspace-agent-orchestration-vision.md](workspace-agent-orchestration-vision.md) | Long-term vision for workspaces, subagents, sidecars, and event processing. |

Deployment configuration and operational startup behavior are documented in [../deployment.md](../deployment.md).
