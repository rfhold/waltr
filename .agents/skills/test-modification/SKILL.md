---
name: test-modification
description: Helps an agent modify Waltr tests when the user asks to add, refactor, or fix test coverage.
---

# Test Modification

Preserve Waltr's intentional layered feature testing strategy.

# Workflow

1. Identify every layer the feature crosses.
2. Preserve layered coverage across agent, server, client, web, and TUI where the feature applies.
3. Keep assertions focused on the layer under test instead of collapsing coverage into one layer.
4. Use existing shared setup before adding new fixtures.
5. Read the matching reference before changing specialized test areas.

# References

| Path | Info |
| --- | --- |
| [references/feature-testing.md](references/feature-testing.md) | General layered feature testing requirements. |
| [references/tui-server-backed.md](references/tui-server-backed.md) | TUI tests that run against a real server. |
| [references/web-k6.md](references/web-k6.md) | Browser-level k6 end-to-end tests. |
| [references/agent-live.md](references/agent-live.md) | Ignored live-provider agent tests. |
| [references/client-server.md](references/client-server.md) | Client and server integration test boundaries. |
| [references/test-containers.md](references/test-containers.md) | Tests that require Docker or container-backed services. |
