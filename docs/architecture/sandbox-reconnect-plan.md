# Sandbox Reconnect Implementation Plan

## Purpose

Make the process-local sandbox usable from every agent entry point, including
`@tool(...)` templates, without requiring a durable template dispatch fence.
This document records the agreed implementation work; `agent.md` remains the
canonical description of the behavior actually shipped.

## Agreed Semantics

- The worker and gateway use a bounded, process-memory request queue. The
  worker survives loss of its RPC stream, reconnects automatically, and sends
  outstanding preparation, execution, and workspace replies without a host
  status query or renegotiation protocol. Completed replies may be sent again;
  the host correlates by request ID and ignores duplicates and late replies.
- Each request has a worker execution timeout and a host delivery deadline.
  When no reply arrives by the host deadline, the caller receives a timeout.
  Reconnection never automatically resubmits an Execute whose delivery is
  uncertain. Neither side waits indefinitely for a dead stream or worker.
- The worker must keep in-flight execution and unsent replies in memory across
  *stream* disconnects. It must not cancel a running operation just because the
  stream closed. A cancellation explicitly requested by the host remains a
  cancellation. On process death the queue is lost; no disk-backed worker queue,
  network listener, or OS isolation is added.
- `@tool(...)` uses the normal tool middleware, permission checks, worker
  Prepare claims and single-use Execute ticket when sandbox-backed. On success
  the renderer includes the tool result; on error/timeout it renders a tool
  error. No template-specific journal fence or automatic replay suppression is
  added: a subsequent render is a new invocation and can repeat a side effect.
- After server restart, do not reconstruct the worker queue or renegotiate
  outstanding requests. Treat incomplete remote work as unknown and use the
  ordinary session interruption/partial-turn resume behavior. Do not
  automatically resubmit an uncertain provider tool Execute.

## Responsibilities and Work Packages

### A. Worker and Local Transport

Owner: `crates/waltr-agent-sandbox/src/wire.rs`, worker-local tests, and
`crates/waltr-server/src/sandbox.rs` plus its local transport tests.

1. Retain worker state (sandbox tickets, in-flight tasks, reply buffer and
   worker identity) outside a single Connect stream; reconnect with bounded
   backoff on transient stream termination. Keep the server's private duplex
   service running and readiness accurate during reconnect and shutdown.
2. Queue bounded replies keyed by invocation/request ID, including Prepared,
   Completed, and WorkspaceResponse; flush the outstanding replies on each
   connection, and send new replies immediately while connected. Retain them
   until their request deadline, with bounded capacity and cleanup. Duplicate
   retransmissions must never execute a tool a second time.
3. Enforce worker-side request timeouts (shell keeps its command timeout; other
   operations have explicit bounds); do not cancel active jobs on stream loss.
   Cancel must still invalidate unused tickets and signal active work.
4. Test stream loss while execution is active, worker reconnect with automatic
   outstanding replies, late/duplicate reply behavior, timeout cleanup, and
   server startup/shutdown/readiness.

### B. Gateway and Private Contract

Owner: `crates/waltr-agent/src/sandbox_gateway.rs`, its unit tests,
`proto/waltr/sandbox/v1/sandbox_service.proto` and the corresponding generated
Rust (only if needed for request timeout fields).

1. Keep a bounded pending-request map across stream disconnects; do not clear
   it simply because its current epoch ended. Correlate replies after a new
   Hello by stable ID and kind, and ignore duplicates, late replies, and stale
   stream messages. Preserve one active worker per workspace and shutdown.
2. Requests made while disconnected wait for reconnection (subject to their
   deadline and the pending cap). Never resend an Execute already handed to a
   connection; if its reply is not delivered before the deadline, time out.
   A Prepare or workspace request queued *before* any connection may be sent
   when connected. Respect backpressure instead of immediate full-channel
   failures; all waits must be bounded.
3. Distinguish prepare, workspace, and execution deadlines. Shell execution's
   delivery allowance must exceed its configured command timeout. Preserve
   single-use tickets and permission approval. An unused ticket can be
   cancelled after reconnect only if it still belongs to the same worker;
   connection epochs alone must not invalidate tickets on stream reconnect.
4. Test queued-before-connect, mid-request disconnect/reply after reconnect,
   missing reply timeout, worker replacement, duplicate/late replies, pending
   capacity, shutdown and cancellation. No polling/reconciliation handshake.

### C. Templates and Session Lifecycle

Owner: `crates/waltr-agent/src/template.rs`, agent loop/session lifecycle only
where necessary, and agent integration tests.

1. Remove the sandbox-template hard rejection. Before calling a sandbox-backed
   tool, follow normal whole-tool and contextual permission evaluation,
   middleware-transformed arguments, worker claims/Prepare, Ask/deny ticket
   cleanup, and Execute using the approved ticket. Preserve ordinary template
   permission suspension and completed-result rendering/cache behavior.
2. Render worker errors and gateway timeouts as template tool errors; do not
   add a template dispatch fence or use a cached failure to suppress an
   explicitly later render. Preserve nested, runtime-prompt, and user-prompt
   rendering paths and non-sandbox tool behavior.
3. Keep ordinary provider-facing tool resume from re-executing uncertain work
   automatically. After process restart, integrate incomplete calls with the
   existing partial-turn interruption/resume flow rather than creating a
   separate worker-reconciliation lifecycle.
4. Replace unsupported-template tests with allow/ask/deny, result/error,
   disconnect/timeout, and rerender tests; check a later render may call
   again. Cover partial-turn resume without duplicate automatic Execute.

### D. Integration and Documentation

Owner: coordinating agent. Integrate interfaces across packages, revise
`docs/architecture/agent.md` and any affected guidance to describe the final
tested contract, and update the old reconnect/disconnect tests. Do not erase
unrelated working-tree changes.

## Sequencing and Verification

1. Agree on request deadline fields and worker identity contract between A and
   B before compiling cross-crate code. Implement A, B, and C in separate files;
   C may initially compile against the previous gateway API.
2. Integrate and run `buf lint` and `buf generate` only if the proto changes;
   check generated-file diff for unrelated plugin churn.
3. Run `cargo fmt --all -- --check`, `cargo test -p waltr-agent-sandbox
   --features shell`, `cargo test -p waltr-agent --features shell --lib`,
   `cargo test -p waltr-agent --features shell --test agent`, targeted
   `cargo test -p waltr-server` sandbox/embedded tests, and feature-off checks.
4. Test the end-to-end disconnect and deadline paths with actual private
   duplex transport. Check `git diff --check`. Document pre-existing failures
   separately rather than attributing them to this change.

## Explicit Tradeoff

The transport does not silently retry an Execute with an uncertain outcome.
The user has explicitly chosen different semantics for rendering: rerendering
an `@tool(...)` expression is a new request, even if a previous rendering
timed out after its side effect occurred. Callers must not mistake the timeout
for proof that a write or command did not happen.
