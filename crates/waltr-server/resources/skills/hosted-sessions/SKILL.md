---
name: hosted-sessions
description: Inspect and control hosted Waltr sessions, submit prompts, and wait for conditions in the stable session projection.
---

# Hosted Sessions

Use the `sessions` tool. Read [the session contract](references/sessions.md)
for inputs, projections, and wait behavior.

1. Call `list` to discover durable sessions or `get` with an exact known id.
2. For new work, call `create` with a required nonempty prompt and an optional
   exact startup agent name. Record the returned session and run ids.
3. For existing work, call `send` with the session id and a nonempty prompt.
   The prompt can queue behind an active run. Inspect the returned disposition.
4. Call `wait` with a jq condition on the `get` projection, then inspect its
   reason and session state. A stopped session is not necessarily successful.

Use `interrupt` only to stop requested work; it also cancels queued runs.
Use `wipe` only with explicit authority to delete every hosted session and
after ensuring they are idle. It requires `confirm: true` and is not a
single-session deletion operation.

If creation reports that a session was created but prompt submission failed,
inspect that session instead of creating another blindly.
