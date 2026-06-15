---
name: documenting
description: Helps an agent scaffold, draft, and finalize Waltr documentation when the user asks to create or refine repository docs.
---

# Documenting

Follow Waltr's scaffold, draft, finalize workflow with minimum assumptions and high quality.

## Workflow

1. Inspect the nearby docs, `AGENTS.md`, and code paths before editing.
2. Choose the right document type and template.
3. Draft only guidance that is supported by current code, tests, or existing docs.
4. Update all affected references, indexes, and local agent guidance.
5. Verify changed docs are internally consistent and referenced files exist.

## Document Types

| Location | Use For |
| --- | --- |
| `docs/architecture/` | System and layer boundaries, responsibilities, flows, invariants, and cross-crate contracts. Update `docs/architecture/README.md` when files change. |
| `docs/agent/` | Agent library features, provider integrations, tools, permissions, storage, skills, and MCP behavior. Update `docs/agent/README.md` when files change. |
| `docs/testing.md` | Test structure, commands, layering expectations, live-test setup, and container-backed test notes. Link here instead of duplicating test command lists. |
| `docs/templates/` | Reusable scaffolds for new documentation. Check `docs/templates/README.md` and the relevant template before creating a new doc. |
| `AGENTS.md` files | Local guidance for future agents: path indexes, boundaries, dependencies, contracts, and concise hints. Use `docs/templates/agents.md` for new local guidance files. |
| `CONTRIBUTING.md` | Concise contributor entry points for running, building, testing, and commit guidance. Prefer links to canonical docs over duplicated detail. |

## Templates

Use `docs/templates/architecture.md` for architecture docs, `docs/templates/feature.md` for feature docs, `docs/templates/runbook.md` for repeatable workflows, and `docs/templates/agents.md` for local `AGENTS.md` files.

Remove template comments and optional sections that do not add useful guidance. Templates are scaffolds, not required headings.

## Reference Maintenance

- Update README indexes when adding, moving, renaming, or deleting docs.
- Update relevant `AGENTS.md` path tables or hints when docs change local guidance, boundaries, or important file locations.
- Replace duplicated guidance with relative Markdown links to the canonical document.
- Keep references relative where possible and verify linked files exist.
- Search for stale references after path, heading, command, or behavior changes.
- If a doc includes commands, verify them or clearly state why verification was not run.

## Quality Bar

- Do not invent behavior. Cite current code/docs or mark uncertainty as an open question.
- Prefer concise tables for indexes and reference lists.
- Keep local guidance local; broad project policy belongs in canonical docs.
- Remove obsolete docs when their content is superseded, but update all references in the same change.
