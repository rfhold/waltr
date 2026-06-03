# Shell Tool Plan

## Goal

Add an opt-in provider-facing `shell` tool to `waltr-agent` that can execute Bash/POSIX shell commands while using tree-sitter-backed command parsing to produce permission claims before execution.

The shell tool should follow the same safety model as filesystem tools:

- It is never enabled by default on `AgentBuilder`.
- Callers opt in explicitly with a builder method.
- Permission claims are extracted before execution.
- Denied contextual permissions are returned as recoverable tool results.
- The live permission test should cover both filesystem and shell permission behavior.

## Non-Goals

- Do not support PowerShell or cmd in the first version.
- Do not parse arbitrary shell behavior perfectly.
- Do not infer filesystem access for dynamic paths such as variables or command substitutions.
- Do not expose tree-sitter types in the public API.
- Do not make shell support part of the base dependency graph unless explicitly enabled.

## Cargo Feature

Shell support should be behind a Cargo feature because tree-sitter and the Bash grammar are large dependencies and require native grammar compilation.

Suggested dependency shape:

```toml
[features]
shell = ["dep:tree-sitter", "dep:tree-sitter-bash"]

[dependencies]
tree-sitter = { version = "0.25", optional = true }
tree-sitter-bash = { version = "0.25", optional = true }
```

Do not include `shell` in default features initially. This keeps `waltr-agent` lightweight for consumers that only need provider streaming, agent loops, or filesystem tools.

Feature-gated exports should make unsupported use obvious. For example:

```rust
#[cfg(feature = "shell")]
pub mod shell;
```

If a public builder method is feature-gated, code that calls it without enabling the feature should fail at compile time rather than silently registering no tool.

## Public Integration

Add one opt-in builder method:

```rust
AgentBuilder::with_shell_tool(config)
```

The tool name exposed to providers must be:

```text
shell
```

Policy examples should also use `shell`, not `bash`:

```yaml
permissions:
  shell:
    command:
      "*": ask
      git:
        effect: deny
        message: do not interact with git
      rg: allow
```

Internal permission resources can continue to use `PermissionResource::ShellCommand` because that describes the resource type, not the provider-facing tool name.

## Tool Schema

The provider-facing schema should be close to opencode's shell tool while keeping the initial implementation small:

- `command`: required string.
- `description`: required short string describing why the command is being run.
- `timeout`: optional positive integer seconds.
- `workdir`: optional string path.

The first version should require `command` and accept `description`, even if `description` is only used for model ergonomics and transcript readability.

## Config

Add a feature-gated shell config type:

```rust
pub struct ShellConfig {
    root: PathBuf,
    default_workdir: PathBuf,
    default_timeout: Duration,
    max_output_bytes: usize,
}
```

Recommended defaults:

- `root`: caller supplied.
- `default_workdir`: `root`.
- `default_timeout`: 30 seconds.
- `max_output_bytes`: 20 KiB.

All path normalization should mirror filesystem tools where practical:

- Relative `workdir` values resolve under `root`.
- Absolute `workdir` values are preserved and checked by permissions.
- Execution should use the resolved `workdir` only after permission approval.

## Parser Design

Use native Rust tree-sitter crates:

- `tree-sitter`
- `tree-sitter-bash`

Create an internal parser wrapper, for example:

```rust
struct ShellCommandParser {
    parser: tree_sitter::Parser,
}
```

The parser module should return our own data structures:

```rust
struct ShellScan {
    commands: Vec<ParsedShellCommand>,
    path_claims: Vec<PermissionClaim>,
    always_patterns: Vec<PermissionPattern>,
}

struct ParsedShellCommand {
    source: String,
    tokens: Vec<String>,
    is_cwd_command: bool,
}
```

Do not leak `tree_sitter::Node`, `Tree`, or grammar details outside the shell module.

## Parser Behavior

Follow opencode's behavior where it is useful:

- Parse the full shell command before execution.
- Traverse every `command` node, including nested commands.
- Extract command tokens from Bash grammar nodes such as `command_name`, `command_name_expr`, `command_elements`, `word`, `string`, `raw_string`, and `concatenation`.
- Skip separators and redirection nodes when building tokens.
- If a command is wrapped in a redirected statement, use the redirected statement source for the shell command permission so `echo hi > file.txt` includes the redirect.
- Unquote simple single-quoted and double-quoted strings.
- Expand `~` to the current user's home directory where reliable.
- Resolve relative path arguments against the resolved workdir.

Dynamic path expressions should not produce filesystem path claims. Treat these as unknown rather than guessing:

- `$VAR`
- `${VAR}`
- `$(...)`
- backtick substitution
- process substitution
- path arguments with dynamic prefixes

## Permission Claims

For each parsed command, add:

```rust
PermissionClaim {
    operation: PermissionOperation::Execute,
    resource: PermissionResource::ShellCommand(source),
    summary,
}
```

Do not add a shell command execute claim for pure cwd commands:

- `cd`
- `chdir`
- `pushd`
- `popd`

Cwd commands should instead produce working-directory or filesystem directory claims when they target a directory outside the configured root.

For commands with reliable file path arguments, add filesystem claims:

- `Read`: `cat`, `less`, `more`, `head`, `tail`, `grep`, `rg`, `ls`, `find`.
- `Write`: `touch`, `mkdir`, `cp`, `mv`, redirect output with `>` or `>>`.
- `Delete`: `rm`, `rmdir`.

When a command has multiple affected paths, emit multiple claims. This preserves the generic permission design requirement that a single tool call can contain multiple commands and multiple resources.

## Command Patterns

Add a small Bash arity helper for reusable `AllowAlways` decisions.

Initial examples:

- `ls -la` -> `ls *`
- `git status --short` -> `git status *`
- `cargo test -p waltr-agent` -> `cargo test *`
- `npm run dev` -> `npm run dev *`

The implementation can start with a curated map for common commands:

- `git`: 2 tokens by default, selected subcommands 3 tokens where useful.
- `cargo`: 2 tokens, with `cargo add` and `cargo run` as 3-token patterns if needed.
- `npm`: 2 tokens, with `npm run` as 3 tokens.
- `docker`: 2 tokens, with `docker compose` as 3 tokens.
- `kubectl`: 2 tokens.

If no arity entry matches, use the first token as the prefix.

These patterns should map into existing reusable permission patterns, likely `PermissionResourcePattern::ShellCommandPrefix`.

## Execution

Execution should happen only after all permission checks pass.

Use `tokio::process::Command` and execute through Bash:

```text
/bin/bash -lc <command>
```

Initial behavior:

- Set current directory to the resolved workdir.
- Apply timeout.
- Capture stdout and stderr.
- Return combined output with exit status.
- Truncate output beyond `max_output_bytes`.
- Return nonzero exits as successful tool output, not agent-loop errors.
- Return process launch failures as recoverable tool output where possible.

Output shape can be simple and deterministic:

```text
Exit code: 0

stdout...
stderr...
```

## Policy Documentation Updates

Update existing permission docs that mention future `bash` policy keys. The provider-facing tool is now `shell`.

Target shape:

```yaml
permissions:
  shell:
    command:
      "*": ask
      rg: allow
      git:
        effect: deny
        message: do not interact with git
    directory:
      "../*": ask
      "../../*": deny
```

The typed Rust policy can continue to use `PermissionResource::ShellCommand` and `PermissionCommandPattern`.

## Deterministic Tests

Add shell tests comparable to filesystem tool tests. These should not require live providers.

Parser and claim extraction tests:

- `echo hello` produces one shell execute claim.
- `echo one && echo two` produces two shell execute claims.
- `echo $(cat /etc/passwd)` detects the nested `cat` command.
- `echo hi > output.txt` uses redirected command source and produces a write claim.
- `cat /etc/passwd` produces an external filesystem read claim.
- `rm /tmp/file` produces a delete claim.
- `cd ..` produces a directory or working-directory claim but no shell execute claim.
- `cat "$FILE"` does not infer a filesystem path claim.
- `git status --short` produces an allow-always prefix compatible with `git status *`.

Execution tests:

- `shell` runs `printf waltr-shell-ok`.
- `workdir` changes where execution happens.
- timeout terminates a long command.
- output truncation works.
- nonzero exit is returned as tool output rather than an agent-loop error.

Permission tests:

- Whole-tool `shell: deny` hides the tool.
- `shell.command.git` deny returns the configured denial message as a recoverable tool result.
- Multiple command claims in one shell call are all checked.
- A denied external path claim prevents execution.
- `AllowAlways` for a shell command prefix is reused within the run.

## Live Tests

Expand `crates/waltr-agent/tests/agent/permissions.rs` so the live permission test includes shell permissions in addition to filesystem permissions.

The live permission test should validate:

- The model can use filesystem and shell tools in the same agent loop.
- A safe shell command such as `rg`, `grep`, `ls`, or `printf` can be allowed.
- A denied command such as `git status` returns the custom denial message if attempted.
- Shell permission denials are recoverable tool results.
- Shell `AllowAlways` patterns can be approved and reused.

Add a separate ignored live shell functionality test similar to filesystem live tests:

```rust
#[ignore = "requires live provider API keys"]
async fn openai_responses_websocket_runs_shell_agent_loop()
```

Expected behavior:

- Prompt asks the model to use `shell` to print `waltr-shell-ok`.
- Transcript includes a `shell` tool call.
- Tool output contains `waltr-shell-ok`.
- Final answer contains `waltr-shell-ok`.

Provider-parametrized live helpers can be reused if they already support selecting Anthropic, OpenAI Chat Completions, OpenAI Responses, and OpenAI Responses WebSocket.

## Implementation Order

1. Add `shell` Cargo feature and optional tree-sitter dependencies.
2. Add `tools::shell` module behind `#[cfg(feature = "shell")]`.
3. Implement parser wrapper and deterministic parser tests.
4. Implement permission claim extraction and deterministic permission tests.
5. Add `AgentBuilder::with_shell_tool(config)` behind the `shell` feature.
6. Implement process execution with timeout and truncation.
7. Add deterministic shell execution tests.
8. Update permission docs from `bash` examples to `shell` examples.
9. Expand live permission test with shell permissions.
10. Add ignored live shell agent-loop functionality test.

## Open Questions

- Should v1 hardcode `/bin/bash`, or should `ShellConfig` accept a shell path immediately?
- Should command path inference include `find` and `grep` flags in v1, or only simple positional paths?
- Should output combine stdout and stderr in stream order, or is stdout then stderr sufficient for v1?

Recommendation for v1:

- Hardcode `/bin/bash -lc` unless config-driven shell selection is immediately needed.
- Support simple positional path inference first.
- Return stdout and stderr in a deterministic combined output format rather than preserving exact interleaving.
