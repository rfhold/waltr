---
name: hosted-sessions
description: Inspect and control hosted Waltr sessions, submit prompts, and wait for conditions in the stable session projection.
---

# Hosted Sessions

Read [the session contract](references/sessions.md) for resources, tools, projections, and wait behavior.

1. Read resource `waltr://sessions` to discover durable sessions, then read the exact entry URI.
2. For new work, call tool `create`, action `session.create`, with a nonempty prompt and an optional exact agent name.
3. For current work, call tool `execute`, action `session.send`, with the session id and a nonempty prompt.
   The prompt can queue behind an active run. Inspect the returned disposition.
4. Call tool `query`, action `session.wait`, with a jq condition on the session projection, then inspect its
   reason and session state. A stopped session is not necessarily successful.

Use tool `execute`, action `session.interrupt`, only to stop requested work; it also cancels queued runs.
Use tool `destroy`, action `session.wipe`, only with explicit authority to delete every hosted session and
after ensuring they are idle. It requires `confirm: true` and is not a
single-session deletion operation.

If creation reports that a session was created but prompt submission failed,
inspect that session instead of creating another blindly.
