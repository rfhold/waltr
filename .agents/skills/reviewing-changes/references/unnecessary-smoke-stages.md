# Unnecessary smoke stages

## Invariant

Add a smoke stage only when it tests a distinct risk and its result changes a decision. Otherwise, verify the requested workflow directly.

## Why it matters

Extra stages add setup, cleanup, and delay. Temporary resources can also outlive the check and confuse operators.

## What to inspect

- Compare each proposed stage with the evidence from `docs/testing.md`, CI, and the requested end-to-end workflow.
- Flag temporary bindings, profiles, or Routines that exercise the same path as the intended configuration without reducing risk.
- Keep a bounded live check when it tests a dependency that local tests cannot cover, such as the deployed proxy or OAuth issuer.
- If a live check needs separate authorization, do not make it a prerequisite for unrelated local setup.

## Suggested check

For each stage, name the distinct failure it detects, the evidence it produces, and the decision that evidence controls. Drop the stage if those answers duplicate another check.

## Finding format

`[unnecessary-smoke-stage] <location>: <stage> duplicates <evidence> and adds <cost>. Verify <required behavior> through <direct workflow>.`
