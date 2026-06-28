# Skill Maintenance

## Invariant

When a review reveals a reusable Waltr primitive invariant, this skill captures it as a short table row and one focused reference file.

## Why It Matters

The skill stays useful only if repeated review knowledge becomes navigable guidance without bloating `SKILL.md`.

## Source Files To Inspect

- `.agents/skills/reviewing-changes/SKILL.md`
- `.agents/skills/reviewing-changes/references/`
- `.agents/skills/agent-modification/SKILL.md`
- `.agents/skills/client-modification/SKILL.md`
- `.agents/skills/server-modification/SKILL.md`
- `.agents/skills/test-modification/SKILL.md`

## Common Drift Signs

- A finding category recurs but has no reference.
- A reference grows into multiple unrelated topics.
- `SKILL.md` duplicates long guidance from references.
- References point to nested paths or use platform-specific path separators.

## Suggested Tests

Run `skills-ref validate .agents/skills/reviewing-changes` when available and manually verify every table path exists.

## Finding Format

`[skill-maintenance] <path>:<line> should add or split <invariant> so future reviews can load focused guidance.`
