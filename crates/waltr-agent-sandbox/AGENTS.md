# Boundaries

This crate owns workspace file operations, patching, skill file access, and optional shell execution. `src/wire.rs` implements the private worker side of the streaming RPC. The agent owns provider/session/prompt/policy and evaluates worker-supplied claims; the server owns the private gateway. Standalone uses the worker executable/image over a local Unix domain socket; embedded hosts run the worker in-process over duplex. See [agent architecture](../../docs/architecture/agent.md#private-workspace-boundary) and [deployment](../../docs/deployment.md#workspace-worker-deployment).

# Contracts

Keep provider-facing tool execution behind prepared, single-use tickets tied to the invocation and arguments. Recheck paths and claims before executing; worker context operations are read-only. Retain in-flight work and bounded unsent replies across stream reconnects of the same living worker; process death loses this state. Never retry an uncertain Execute automatically or add host-side execution fallback.

The `shell` feature gates worker shell execution; Android builds omit it. Standalone workers use `WALTR_SANDBOX_SOCKET=/run/waltr-sandbox/gateway.sock` and `WALTR_SANDBOX_WORKSPACE_ROOT=/workspace`, with no public listener. Do not mount the server's `/data`, secret projections, or host-owned config in the worker workspace. A separate process/mount is not a network boundary inside a shared Kubernetes pod.

# Hints

Use the agent-modification skill for workspace tool behavior, the protobuf-generation skill for changes to the private proto, and the test-modification skill when editing tests.
