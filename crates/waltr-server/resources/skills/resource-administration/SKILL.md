---
name: resource-administration
description: Inspect, replace, or reset complete Waltr YAML resources with persisted-slot ETag protection. Use for requested configuration and agent-definition changes.
---

# Resource Administration

Use the `exec` tool for Waltr resource administration. Read
[the resource contract](references/resources.md) before editing.

1. Call `resources.inspect` with the canonical resource path. Omit the path to
   discover available entries.
2. Preserve unrelated settings and prepare one complete strict YAML resource.
3. Call `resources.replace` with the path, complete source, and the inspected
   persisted-slot `expected_etag`. Never send a patch, merge, or diff.
4. Inspect the returned resource and report the change.

Use `resources.reset` only when the user requests removal of the persisted
override. Supply its current persisted-slot ETag. Reset reveals the bundled
fallback if one exists; it does not write a replacement resource.

On validation or stale-ETag errors, inspect again and rebuild the requested
replacement. Do not retry a stale write unchanged. Do not edit credentials or
unrelated resources without explicit user direction.
