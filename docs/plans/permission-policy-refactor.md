# Permission Policy Refactor Plan

## Goal

Replace the current split permission configuration with one typed policy table that can express whole-tool, class, directory, and command permissions consistently across filesystem tools and future command tools.

The policy should be simple to reason about:

- Rules are grouped by tool selector.
- Exact tool selectors override global `*` selectors.
- Directory rules apply to any tool that emits filesystem or working-directory metadata, including future command tools.
- The default safe filesystem behavior remains: paths under the configured root are allowed and paths outside the root are denied unless a caller configures otherwise.
- Do not add a built-in `../../*` deny rule. Parent-directory patterns are caller policy, not default policy.

## Target Shape

The user-facing shape should map cleanly to this structure:

```yaml
permissions:
  "*":
    directory:
      "../*": ask
      "../../*": deny
    class:
      delete: deny
      write: ask
      read: allow

  read:
    directory:
      "*": allow

  shell:
    command:
      "*": ask
      git:
        effect: deny
        message: do not interact with git
      rg: allow

  delete: deny
```

This plan should first implement the typed Rust model and evaluator. YAML or serde parsing can be added later if persisted configuration is needed.

Rule values can be either shorthand levels (`allow`, `ask`, `deny`) or expanded objects with `effect` and optional `message`. Messages are used when a deny rule blocks a call.

## Semantics

### Tool Selectors

Top-level policy keys select tools:

- `*`: applies to every tool.
- exact tool name, such as `read`, `shell`, or `delete`: applies only to that tool.

An exact tool selector can map directly to `deny` as shorthand for `tool: deny`.

Exact tool selectors are more specific than `*` and override global rules.

Example:

```yaml
permissions:
  "*":
    directory:
      "../../*": deny
  read:
    directory:
      "*": allow
```

`read ../../secret` is allowed because the exact `read` tool selector is more specific than the global selector.

### Rule Categories

Each tool selector can define these categories:

- `tool`: whole-tool permission. `deny` means the tool is not surfaced to the model.
- `class`: operation-level permission derived from metadata, such as `read`, `write`, `delete`, `execute`, or `network`.
- `directory`: filesystem path or working-directory permission.
- `command`: shell command permission.

Each rule can use either a bare level or an expanded effect object:

```yaml
git:
  effect: deny
  message: do not interact with git
```

`message` is currently meaningful for `deny`; `allow` and `ask` may carry it in the typed model, but it is ignored by the current engine.

Directory rules apply to:

- `PermissionResource::FilesystemPath`
- `PermissionResource::FilesystemPathPrefix`
- `PermissionResource::WorkingDirectory`
- future command tool metadata that emits filesystem path claims

Command rules apply to:

- `PermissionResource::ShellCommand`

### Precedence

Permission evaluation should rank matches in this order:

1. Exact tool selector, exact or most-specific resource/class/command pattern.
2. Exact tool selector, wildcard pattern.
3. Global `*` selector, exact or most-specific resource/class/command pattern.
4. Global `*` selector, wildcard pattern.
5. Default fallback.

If two matching rules have the same specificity, `deny` wins over `ask`, and `ask` wins over `allow`.

Whole-tool `deny` for an exact tool selector hides that exact tool regardless of broader global rules.

## Default Policy

Default policy should preserve current safety without baking in user-specific parent-directory rules.

Filesystem defaults:

- Allow configured filesystem root paths for operations exposed by the enabled tools.
- Deny filesystem paths outside the configured root unless the caller configures another policy.
- Do not implicitly add `../*`, `../../*`, or any other parent-directory pattern.

Tool defaults:

- Tools are surfaced unless policy denies the tool.
- Filesystem tool profile still decides which filesystem mutation tool names exist for a model.
- Policy only decides whether exposed tools are allowed, asked, or denied.

Ask defaults:

- If a rule evaluates to `ask` and no permission handler is configured, deny with a clear recoverable tool error.

## Typed Rust Model

Add a typed policy model in `crates/waltr-agent/src/permission.rs`.

Suggested public types:

```rust
pub struct PermissionPolicy {
    rules: Vec<PermissionToolRule>,
    fallback: PermissionLevel,
}

pub struct PermissionToolRule {
    selector: PermissionToolSelector,
    tool: Option<PermissionLevel>,
    classes: Vec<PermissionClassRule>,
    directories: Vec<PermissionDirectoryRule>,
    commands: Vec<PermissionCommandRule>,
}

pub enum PermissionToolSelector {
    Any,
    Exact(String),
}

pub struct PermissionClassRule {
    class: PermissionOperation,
    level: PermissionLevel,
}

pub struct PermissionDirectoryRule {
    pattern: PermissionPathPattern,
    level: PermissionLevel,
}

pub struct PermissionCommandRule {
    pattern: PermissionCommandPattern,
    level: PermissionLevel,
}
```

Pattern types should support the minimal useful set first:

- exact path
- path prefix
- glob-like wildcard string for user-facing construction
- exact command
- command prefix or command name match
- wildcard `*`

Keep `PermissionPattern` for run-scoped `AllowAlways` approvals. It can remain lower-level and concrete.

## Evaluation API

Add an evaluation context:

```rust
pub struct PermissionCheckContext<'a> {
    pub tool_name: &'a str,
    pub claims: &'a [PermissionClaim],
}
```

Policy API:

```rust
impl PermissionPolicy {
    pub fn evaluate_tool(&self, tool_name: &str) -> PermissionLevel;
    pub fn evaluate_claim(&self, tool_name: &str, claim: &PermissionClaim) -> PermissionLevel;
}
```

`evaluate_tool` only considers whole-tool `tool` rules.

`evaluate_claim` considers class, directory, and command rules. A single claim can match multiple categories, such as an execute claim with a working directory. Use the same specificity ranking across all matched rules.

## Permission Engine Changes

Change the engine from seeded patterns plus a default contextual level to policy-driven evaluation.

Current shape:

```rust
PermissionEngine::new(patterns, handler)
check(call, claims, default_level)
```

Target shape:

```rust
PermissionEngine::new(policy, handler)
check(call, claims)
```

Flow:

1. Extract claims for the tool call.
2. Check run-scoped `AllowAlways` patterns first.
3. For unmatched claims, evaluate `PermissionPolicy` using the tool name and claim metadata.
4. If every unmatched claim evaluates to `allow`, execute the tool.
5. If any unmatched claim evaluates to `deny`, return a recoverable tool result and do not execute the tool.
6. If any unmatched claim evaluates to `ask`, call the permission handler with those ask claims.
7. `AllowOnce` permits this call only.
8. `AllowAlways` stores concrete `PermissionPattern` approvals and permits this call.
9. Handler `Deny(message)` returns a recoverable tool result and does not execute the tool.

If a tool call has multiple unmatched claims, deny should block execution before ask unless an exact-tool ask rule is more specific than a global deny for the same claim.

## Agent Integration

Move policy onto the agent and run builders:

```rust
AgentBuilder::with_permission_policy(policy)
AgentRunBuilder::with_permission_policy(policy)
```

Agent loop changes:

- Build `PermissionEngine` from the run policy and handler.
- Surface tools only when `policy.evaluate_tool(tool.name()) != PermissionLevel::Deny`.
- If `evaluate_tool` returns `Ask`, surface only when a permission handler exists; otherwise hide it.
- Execute contextual claim checks through `PermissionEngine::check(call, claims)`.

`AgentTool` should keep:

- provider-facing `ToolDefinition`
- handler
- claim extractor

Remove or stop using:

- `whole_tool`
- `contextual`
- `allowed_patterns`

## Filesystem Integration

Simplify `FilesystemConfig` back toward filesystem concerns:

- keep `root`
- keep `profile`
- keep claim extraction
- keep direct public methods root-scoped for direct API safety

Remove or replace:

- `external_permission`
- `read_permission`
- `write_permission`
- `delete_permission`

Filesystem tools should emit claims with normalized absolute paths. The policy decides whether those paths are allowed, asked, or denied.

Add policy helper constructors for filesystem defaults:

```rust
PermissionPolicy::filesystem_default(root)
PermissionPolicy::filesystem_external(root, PermissionEffect::ask())
```

`filesystem_default(root)` should allow root paths and deny external paths without using parent-directory glob rules.

`filesystem_external(root, PermissionEffect::ask())` or builder equivalents should be used in tests that need external access prompts.

## Tests

### Permission Policy Unit Tests

Add focused tests in `permission.rs` for:

- exact tool selector overrides global selector.
- exact tool `tool: deny` hides that tool.
- same-specificity conflict chooses deny over ask over allow.
- class `write: ask`, `read: allow`, `delete: deny` evaluates correctly.
- directory rules match filesystem paths and working directories.
- command rules match `shell` tool command resources.
- no built-in `../../*` deny rule exists in the default policy.

### Agent Integration Tests

Update `tests/agent/permissions.rs`:

- Replace `with_external_permission(PermissionLevel::Ask)` with a policy effect that allows root paths and asks for external filesystem paths.
- Replace `with_tool_permission(..., Deny)` with policy rules that deny write/delete classes or exact tool names.
- Keep deterministic behavior unchanged:
  - root read does not ask.
  - external read asks.
  - `AllowAlways` stores and reuses a concrete pattern.
  - deny returns a recoverable tool result.
  - multi-claim denial blocks execution.

### Live Test

Keep `live_permissions_agent_loop` behavior unchanged. Only migrate its setup to the new policy builder.

## Compatibility

Preferred approach: remove the old filesystem permission knobs if this API has no external compatibility requirement.

If compatibility is needed, temporarily keep old methods as thin policy helpers and mark them as transitional:

- `with_external_permission(level)` adds the equivalent external filesystem policy.
- `with_tool_permission(operation, level)` adds class or exact-tool policy.

Do not keep two independent permission systems long-term.

## Implementation Phases

### Phase 1: Policy Types And Evaluator

- Add typed policy types.
- Add precedence ranking.
- Add unit tests for exact-tool override, conflicts, class, directory, and command matching.
- Keep current engine behavior unchanged in this phase.

### Phase 2: Engine Policy Integration

- Change `PermissionEngine` to store `PermissionPolicy`.
- Keep run-scoped `AllowAlways` concrete patterns.
- Update `check` to evaluate policy per claim.
- Add tests for ask/deny/allow engine behavior through policy.

### Phase 3: Agent Tool Surfacing

- Add policy to `Agent` and `AgentRunBuilder`.
- Use policy for whole-tool surfacing.
- Remove `whole_tool`, `contextual`, and seeded allowed patterns from `AgentTool`.

### Phase 4: Filesystem Migration

- Remove filesystem-specific permission fields.
- Move root allow/external deny behavior into default policy helpers.
- Update filesystem agent tool construction to only attach claim extractors.

### Phase 5: Test Migration And Verification

- Migrate deterministic permission tests to policy configuration.
- Migrate live permission test configuration.
- Run `cargo fmt -p waltr-agent`.
- Run `cargo test -p waltr-agent --test agent`.
- Run `cargo clippy -p waltr-agent --all-targets`.
- Run `cargo test -p waltr-agent`.

## Open Questions

- Should exact tool `tool: ask` hide the tool when no handler exists, or surface it and deny at execution? Recommended: hide it when no handler exists.
- Should directory string patterns be interpreted relative to each tool root, or only against normalized paths with special support for `../*`-style user-facing patterns? Recommended: normalize claims and provide builder helpers first; defer persisted string pattern semantics until needed.
- Should class rules apply to synthetic whole-tool surfacing, or only to claims? Recommended: claims only; use `tool` rules for surfacing.
