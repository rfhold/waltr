# Shell Tool

The server owns shell schemas and policy; `waltr-agent-sandbox` executes commands through the private worker.
Both crates require `shell`; `waltr-agent` has no shell feature. `HostRuntimeBuilder` installs the tool.
See [the private workspace boundary](../architecture/agent.md#private-workspace-boundary).

Standalone containers use a separate worker. Embedded desktop uses a process-local worker without OS isolation. Android has no shell feature.

Every host loads the same ordinary catalog. A no-shell build accepts, exposes, and resolves shell-bearing definitions, but runtime construction does not install the shell tool or feature-derived shell tool permissions.

## Entry Points

| API | Purpose |
| --- | --- |
| Server Cargo feature `shell` | Enables server schemas and the worker shell feature. |
| `HostRuntimeBuilder::with_shell_tool` | Adds the `shell` capability. |
| `HostRuntimeBuilder::with_shell_system_prompt` | Adds shell prompt guidance. |
| `ShellConfig` | Configures root, default workdir, timeout, and output limits; overrides are sent to the worker. |
| `ShellInput` | Provider-facing command input. |
| `ShellScan` and `ParsedShellCommand` | Server parser types for command hints; worker supplies claims for runtime decisions. |

## Execution Behavior

The provider sees a single tool named `shell`. Approved calls execute in the workspace worker after worker preparation, host permission evaluation, and a durable dispatch fence. Standalone containers run it in the separate worker image; embedded hosts run it in-process. The worker wraps the command with signal cleanup and invokes Bash with `-lc`:

```text
/bin/bash -lc <wrapped command>
```

Current behavior:

- Standard input is null.
- Standard output streams as generic `tool_progress_info` timeline deltas.
- Standard error streams as generic `tool_progress_error` timeline deltas.
- Standard output and standard error are still captured into one final provider-facing tool result.
- The default timeout is 30 seconds.
- The default maximum output is 20 KiB.
- Timed-out processes are killed and reported with timeout metadata.
- Nonzero exit status is returned as normal tool output, not as a Rust error.

Without an explicit `workdir`, commands use the configured default workdir (the root unless overridden; a relative default is rooted there). An explicit relative `workdir` resolves from the root, **not** from that default; an explicit absolute `workdir` is normalized by the worker. The worker uses configured timeout and output-limit overrides for execution.

## Parsing And Permission Claims

The worker's shell feature uses tree-sitter bash to inspect commands before execution. The parser finds simple and nested commands, then permission helpers derive claims for:

- Shell command execution.
- Directory changes from commands such as `cd` and `pushd`.
- Known file operations for common command names.
- Redirection writes through `>` and `>>`.

Some shell constructs cannot be safely inferred. Dynamic paths containing shell variables, command substitutions, backticks, or process substitution are not converted into precise filesystem claims.

## Safety Boundaries

The worker is a local command runner, not an OS sandbox. Private RPC separates tool ownership but does not reduce operating-system authority.

- There is no chroot, container, seccomp, or PTY sandbox.
- The embedded desktop worker runs shell commands with the app process's operating-system authority.
- Android exposes no shell tool. Shell-bearing resource source remains valid and inspectable, but shell feature selectors produce no runtime tool permission rules.
- Interactive command handling is not implemented.
- Shell output progress is display-only timeline data; provider replay still receives the final tool result.
- Environment configuration is not exposed through `ShellConfig` today.
- A dispatched call with unknown outcome suspends awaiting tool results; it is not automatically re-executed on restart.

Hosts should configure explicit `PermissionPolicy` rules for shell use. The parser improves permission decisions, but it cannot prove all side effects of arbitrary shell code.

## Tests

See [../testing.md](../testing.md). Server host shell tests require the server `shell` feature, which also enables worker shell support.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-server/src/agent_host/tools/shell/mod.rs](../../crates/waltr-server/src/agent_host/tools/shell/mod.rs) | Server shell schema and parser. |
| [../../crates/waltr-agent-sandbox/src/shell.rs](../../crates/waltr-agent-sandbox/src/shell.rs) | Worker-side shell parsing, claims, and execution. |
| [../../crates/waltr-server/tests/agent_host/permissions.rs](../../crates/waltr-server/tests/agent_host/permissions.rs) | Shell policy cases. |
| [../../crates/waltr-server/tests/agent_host/live/shell.rs](../../crates/waltr-server/tests/agent_host/live/shell.rs) | Live-provider shell cases. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-server/Cargo.toml](../../crates/waltr-server/Cargo.toml) | Server `shell` feature. |
| [../../crates/waltr-server/src/agent_host/tools/shell/config.rs](../../crates/waltr-server/src/agent_host/tools/shell/config.rs) | Shell configuration and input types. |
| [../../crates/waltr-agent-sandbox/src/wire.rs](../../crates/waltr-agent-sandbox/src/wire.rs) | Worker RPC defaults including configured shell overrides. |
