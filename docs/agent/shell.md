# Shell Tool

The shell tool gives an agent controlled access to local shell command execution. It is an optional `waltr-agent` feature and is not available unless the crate is built with the `shell` feature and the host enables the tool through `AgentBuilder`.

## Entry Points

| API | Purpose |
| --- | --- |
| Cargo feature `shell` | Enables shell modules and tree-sitter bash parsing dependencies. |
| `AgentBuilder::with_shell_tool` | Adds the provider-facing `shell` tool. |
| `AgentBuilder::with_shell_system_prompt` | Adds shell usage guidance to the system prompt. |
| `ShellConfig` | Configures root, timeout, and output limits. |
| `ShellInput` | Provider-facing command input. |
| `ShellScan` and `ParsedShellCommand` | Parser output used for permission claims and command hints. |

## Execution Behavior

The provider sees a single tool named `shell`. Tool calls execute through:

```text
/bin/bash -lc <command>
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

Relative working directories resolve from the configured root. Absolute working directories are normalized by the shell path helper.

## Parsing And Permission Claims

The shell feature uses tree-sitter bash to inspect commands before execution. The parser finds simple and nested commands, then permission helpers derive claims for:

- Shell command execution.
- Directory changes from commands such as `cd` and `pushd`.
- Known file operations for common command names.
- Redirection writes through `>` and `>>`.

Some shell constructs cannot be safely inferred. Dynamic paths containing shell variables, command substitutions, backticks, or process substitution are not converted into precise filesystem claims.

## Safety Boundaries

The shell tool is a local command runner, not a sandbox.

- There is no chroot, container, seccomp, or PTY sandbox.
- Interactive command handling is not implemented.
- Shell output progress is display-only timeline data; provider replay still receives the final tool result.
- Environment configuration is not exposed through `ShellConfig` today.
- Durable allow-once or allow-always approvals are not implemented in the agent loop.

Hosts should configure explicit `PermissionPolicy` rules for shell use. The parser improves permission decisions, but it cannot prove all side effects of arbitrary shell code.

## Tests

See [../testing.md](../testing.md) for canonical test commands. Shell tests require `--features shell`.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent/src/tools/shell/mod.rs](../../crates/waltr-agent/src/tools/shell/mod.rs) | Unit coverage for shell parsing, permissions, and execution. |
| [../../crates/waltr-agent/tests/agent/permissions.rs](../../crates/waltr-agent/tests/agent/permissions.rs) | Deterministic permission behavior with shell claims when the feature is enabled. |
| [../../crates/waltr-agent/tests/live/agent/shell.rs](../../crates/waltr-agent/tests/live/agent/shell.rs) | Ignored live-provider shell coverage. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/Cargo.toml](../../crates/waltr-agent/Cargo.toml) | `shell` feature declaration. |
| [../../crates/waltr-agent/src/tools/shell/config.rs](../../crates/waltr-agent/src/tools/shell/config.rs) | Shell configuration and input types. |
| [../../crates/waltr-agent/src/tools/shell/execute.rs](../../crates/waltr-agent/src/tools/shell/execute.rs) | Process execution, timeout, and output capture. |
| [../../crates/waltr-agent/src/tools/shell/formatting.rs](../../crates/waltr-agent/src/tools/shell/formatting.rs) | Tool result formatting. |
| [../../crates/waltr-agent/src/tools/shell/parser.rs](../../crates/waltr-agent/src/tools/shell/parser.rs) | Tree-sitter bash parsing. |
| [../../crates/waltr-agent/src/tools/shell/permissions.rs](../../crates/waltr-agent/src/tools/shell/permissions.rs) | Shell permission claim construction. |
| [../../crates/waltr-agent/src/tools/shell/path.rs](../../crates/waltr-agent/src/tools/shell/path.rs) | Working-directory normalization. |
