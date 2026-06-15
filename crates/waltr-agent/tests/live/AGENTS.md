# Audience

Agent live tests validate behavior that requires real provider or external service integration.

# Boundaries

Live tests should use the real provider through shared setup instead of local provider wiring.

# Index

| Path | Info |
| --- | --- |
| [../support/](../support/) | Shared live provider and capture setup. |
| [agent/](agent/) | Live agent-loop, tool, permission, storage, and MCP coverage. |

# Hints

Use the test-modification skill when modifying tests.
