# Test Containers

Container-backed tests validate behavior that depends on external storage or services.

# Scope

Use containers only when in-memory or filesystem-backed tests cannot exercise the required behavior.

# Expectations

Keep container-backed tests explicit about Docker requirements.

Prefer deterministic local fixtures for behavior that does not require an external service.

Avoid hiding container startup behind unrelated test helpers.
