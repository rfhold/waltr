# Audience

Agent tests should be structured to validate behavior from a library consumption audience.

# Boundaries

Deterministic agent tests should use scripted providers and shared support instead of live provider calls.

# Index

| Path | Info |
| --- | --- |
| [support/](support/) | Shared test providers and fixtures for agent integration tests. |
| [live/](live/) | Live provider coverage belongs here instead of deterministic tests. |

# Hints

Use the test-modification skill when modifying tests.
