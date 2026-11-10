# Server Agent Host

`waltr-server/src/agent_host/` owns concrete capabilities, definitions, templates, MCP, Skills, and worker integration.
The [agent architecture](../architecture/agent.md) defines the generic runner boundary.
Trusted process-local session roles own helper/task provenance, never public metadata or generic agent events.
Routine JavaScript owns workflow policy; the host supplies authorized primitives and ordinary permission/Skills consent.
See [Routines](../architecture/routines.md) for source ceilings, explicit helpers/captures and joined lifecycle ownership.

| Document | Covers |
| --- | --- |
| [agents_md.md](agents_md.md) | Worker-backed instruction context. |
| [filesystem.md](filesystem.md) | Filesystem schemas and worker contracts. |
| [shell.md](shell.md) | Server and worker shell feature, limits, and security. |
| [skills.md](skills.md) | Local Skills and shared loader. |
| [mcp.md](mcp.md) | MCP adaptation, remote Skills, Apps authority, and upstream OAuth. |
| [permissions.md](permissions.md) | Server policy and live approvals. |
| [middleware.md](middleware.md) | Concrete context hooks and host assembly. |

See [coordinated upgrade](../runbooks/server-capability-cutover.md) before an upgrade from the former agent-owned host runtime.
