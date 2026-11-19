# Unnecessary UI gating

## Invariant

Keep a supported destination visible when its data is empty. Gate the action that lacks a prerequisite, not the place where users learn what they need.

## Why it matters

A hidden destination makes an empty state look like missing support. It also removes the instructions that help a user set up the feature.

## What to inspect

- In `crates/waltr-web/src/routes/config/ConfigRoute.tsx`, check whether tab visibility depends on the length of a resource list.
- Compare the equivalent TUI flow. Separate empty data, loading, errors, and lack of support.
- Preserve access and capability checks that protect real boundaries. An empty list alone is not an access or capability check.

## Suggested test

Open Config with no workspace bindings. Assert that the tab remains visible and its empty state explains where to add a binding. Verify that a binding-specific authorization action appears only when its prerequisites hold.

## Finding format

`[unnecessary-ui-gating] <location>: <condition> hides <destination> even though <feature> is supported. Keep <destination> visible and gate only <action>.`
