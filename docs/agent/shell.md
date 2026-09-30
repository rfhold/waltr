# Shell Tool

The shell tool gives an agent policy-controlled access to local shell command execution. The agent owns the schema and permissions; `waltr-agent-sandbox` executes commands through the private worker. Both crates require the `shell` feature and the host must enable the tool through `AgentBuilder`. See [the private workspace boundary](../architecture/agent.md#private-workspace-boundary).

The standalone server and embedded desktop app retain this feature. Their worker runs in the server process without operating-system isolation. Android is compiled without it.

Every host loads the same ordinary catalog. A no-shell build accepts, exposes, and resolves shell-bearing definitions, but runtime construction does not install the shell tool or feature-derived shell tool permissions.

## Entry Points

| API | Purpose |
| --- | --- |
| Cargo feature `shell` | Enables agent shell schemas and worker-side tree-sitter bash scanning/execution. |
| `AgentBuilder::with_shell_tool` | Adds the provider-facing `shell` tool. |
| `AgentBuilder::with_shell_system_prompt` | Adds shell usage guidance to the system prompt. |
| `ShellConfig` | Configures root, default workdir, timeout, and output limits; overrides are sent to the worker. |
| `ShellInput` | Provider-facing command input. |
| `ShellScan` and `ParsedShellCommand` | Agent-side parser types for command hints; worker supplies claims for runtime decisions. |

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

See [../testing.md](../testing.md) for canonical test commands. Agent shell tests require `--features shell`; server shell builds enable both agent and worker features.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent/src/tools/shell/mod.rs](../../crates/waltr-agent/src/tools/shell/mod.rs) | Agent-side shell schema and parser unit coverage. |
| [../../crates/waltr-agent-sandbox/src/shell.rs](../../crates/waltr-agent-sandbox/src/shell.rs) | Worker-side shell parsing, claims, and execution. |
| [../../crates/waltr-agent/tests/agent/permissions.rs](../../crates/waltr-agent/tests/agent/permissions.rs) | Deterministic permission behavior with shell claims when the feature is enabled. |
| [../../crates/waltr-agent/tests/live/agent/shell.rs](../../crates/waltr-agent/tests/live/agent/shell.rs) | Ignored live-provider shell coverage. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/Cargo.toml](../../crates/waltr-agent/Cargo.toml) | `shell` feature declaration. |
| [../../crates/waltr-agent/src/tools/shell/config.rs](../../crates/waltr-agent/src/tools/shell/config.rs) | Shell configuration and input types. |
| [../../crates/waltr-agent-sandbox/src/wire.rs](../../crates/waltr-agent-sandbox/src/wire.rs) | Worker RPC defaults including configured shell overrides. |
