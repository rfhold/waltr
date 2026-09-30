---
name: reviewing-changes
description: Helps an agent review Waltr commits, pull requests, diffs, or feature work for drift from primitive patterns in state, requests, lifecycle, boundaries, and tests.
---

# Reviewing Changes

Review changes against Waltr's primitive patterns before judging style. Prefer concrete findings tied to drift in ownership, lifecycle, contracts, or test layering.

## When To Use

Use this skill when reviewing recent commits, PRs, diffs, feature branches, or completed feature work across Waltr crates, docs, generated contracts, or tests.

Do not use this skill to implement fixes unless the user asks for changes after the review.

## Review Workflow

1. Identify the changed paths and the affected layer: agent, server, client, web, TUI, proto, tests, or docs.
2. Load only the reference rows that match the changed paths or suspected drift.
3. Compare the change to the invariant, not to personal preference.
4. Check tests at the same layer as the behavior, plus cross-layer tests when a contract or lifecycle crosses a boundary.
5. Report findings first, ordered by severity, with file and line references when available.
6. Include residual risks or missing validation when no findings are discovered.

## Review Focus

| Item | What to look for | Reference file path |
| --- | --- | --- |
| Layer ownership | Behavior moved into a crate that should only consume the owning layer. | `references/layer-ownership.md` |
| Construction inputs | Growing `*_with_*` variants, positional forwarding, or placeholder defaults hide required construction capabilities. | `references/construction-inputs.md` |
| Shared state lifecycle | Server data duplicated in UI state or mutated outside the client reducer/effect model. | `references/shared-state-lifecycle.md` |
| Request identity | Operations missing stable IDs, stale result guards, or keyed completion state. | `references/request-identity.md` |
| Watch lifecycle | Watches opening too early, leaking after interest ends, or reconnecting outside readiness rules. | `references/watch-lifecycle.md` |
| Server stream lifecycle | Initial snapshots, update ordering, cancellation, or stream error semantics drifting from server contracts. | `references/server-stream-lifecycle.md` |
| Agent session replay chronology | Replay, persistence, suspension, or resume logic changing event order or durable records. | `references/agent-session-replay-chronology.md` |
| Tool suspension lifecycle | Tool permission, suspension, resume input, or result handling bypassing agent loop semantics. | `references/tool-suspension-lifecycle.md` |
| Provider usage semantics | Provider adapters leaking provider-specific details into neutral types or misrepresenting stream events. | `references/provider-usage-semantics.md` |
| Transport contracts | Proto, gRPC, gRPC-web, WASM, or Rust transport changes breaking status, ETag, or message semantics. | `references/transport-contracts.md` |
| UI operation lifecycle | UI forms, dialogs, tabs, transitions, or operation feedback losing pending/success/error behavior. | `references/ui-operation-lifecycle.md` |
| UI parity | Web and TUI shared flows diverging in labels, empty states, errors, or available actions. | `references/ui-parity.md` |
| Test layering | Tests asserted at the wrong layer, duplicate lower-layer coverage, or omit contract/lifecycle cases. | `references/test-layering.md` |
| Skill maintenance | A new invariant appears but is not captured as reusable review guidance. | `references/skill-maintenance.md` |

## Maintaining This Skill

When a review identifies a new pattern invariant, add one focused reference under `references/`, add a row to the table, and keep `SKILL.md` concise. The new reference should state the invariant, why it matters, source files to inspect, common drift signs, suggested tests, and finding format.

Keep references one level deep, use forward-slash paths, and do not add scripts unless deterministic automation is clearly needed.
