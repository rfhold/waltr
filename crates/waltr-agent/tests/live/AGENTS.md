# Audience

Agent live tests validate behavior that requires real provider or external service integration.

# Boundaries

Live tests should use the real provider through shared setup instead of local provider wiring.

# Index

| Path | Info |
| --- | --- |
| [../support/](../support/) | Shared live provider and capture setup. |
| [agent/](agent/) | Generic live inference and storage coverage. Concrete tool/permission/MCP live coverage lives in `../../../waltr-server/tests/agent_host/live/`. |

# Hints

Use the test-modification skill when modifying tests.
