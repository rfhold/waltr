---
name: resource-administration
description: Read and edit Waltr YAML resources with guarded revisions. Use for requested configuration and agent-definition changes.
---

# Resource Administration

Read [the resource contract](references/resources.md) before a mutation.

1. Discover entries through the `waltr://resources` MCP resource.
2. Read the entry URI for exact source and revision metadata.
3. Preserve unrelated settings in ordered text edits.
4. Call tool `edit` directly with the entry URI, `expected_revision`, and `edits`.
5. Check the returned resource.

Use tool `create`, action `resource.create`, for an absent persisted slot with complete YAML and its guarded revision.
Creation can establish a bundled override, but cannot overwrite a persisted file.
An edit to bundled effective source also establishes a persisted override.

Use tool `destroy`, action `resource.destroy`, only for a requested removal of a persisted override.
Destruction restores the bundled fallback if present; catalog validation can reject the removal.

On validation or stale-revision errors, read the entry again.
Do not retry a stale mutation unchanged.
Do not edit credentials or unrelated resources without explicit user direction.
