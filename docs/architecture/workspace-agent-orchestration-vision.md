# Workspace And Agent Orchestration Vision

This vision document captures long-term direction for Waltr workspace context, subagent orchestration, sidecar agents, and event-driven workflows. It is intentionally future-facing and does not describe only shipped behavior.

## Purpose

Waltr should grow from a single active agent session into an orchestration environment where a primary session can delegate work, maintain workspace-specific operating context, and process ongoing event streams without losing focus.

The long-term goal is to make agent work more durable, contextual, and parallel while keeping the main user session understandable and controllable.

## Design Principles

- Main sessions should stay responsive while delegated work continues elsewhere.
- Workspace context should be explicit, inspectable, and tied to a filesystem directory.
- Sidecar agents should support focused auxiliary jobs without becoming the main actor.
- Event processing should preserve semantic meaning and avoid drowning workspaces in noise.
- Context injection should support multiple transport forms, including markdown, tool calls, MCP resources, and expanded file reads.

## Subagents

Subagents are delegated agents created from a primary session or workspace workflow. They should be able to handle focused tasks without requiring the main session to carry every detail in its active context.

Subagents may be used for exploratory research, implementation support, validation, summarization, or task-specific workflows. A subagent should have a clear scope, explicit inputs, and a result contract that can be returned to the main session or written into workspace state.

## Background Async Subagents

Background async subagents should allow the main session to continue while delegated work runs independently. The main session should not block on long-running exploration, validation, or summarization unless the user asks to wait.

A background subagent needs durable status, progress events, cancellation, and a final result. The main session should be able to reference running background work, poll or receive completion updates, and incorporate the result when useful.

## Sidecar Agents

Sidecar agents are auxiliary agents attached to an active session, workspace, or event cursor. They should run focused support roles around the primary work rather than owning the primary task.

Potential sidecar uses include:

- Summarizing an active session for continuity or handoff.
- Watching an active session to validate compliance with policies, instructions, or quality bars.
- Producing task-specific summaries for documentation, tickets, changelogs, or external tracking systems.
- Updating workspace context, loaded skills, relevant files, or prompt fragments when the workspace changes.

Sidecars should be explicit about their authority. A sidecar may recommend updates, produce summaries, or flag compliance issues, but the system needs clear rules for when a sidecar can mutate workspace state or influence the main prompt.

## Workspaces

A workspace is a durable operating context tied to a filesystem directory. It should have its own local guidance, loaded capabilities, and read context so agents can work with project-specific assumptions without relying on global configuration alone.

Workspace-owned context may include:

- An `AGENTS.md` file or equivalent local instruction source.
- Defined loaded skills.
- Available tools and MCP servers.
- Permission policy.
- Filesystem read boundaries.
- A read-context markdown file that references other files, resources, and contextual inputs.

The read-context markdown file should act as an inspectable manifest for workspace context. It can reference local files, MCP resources, or other context sources without forcing every item to be expanded all the time.

Context injection options could include:

- Fully expanding referenced reads into a user message.
- Injecting multiple user messages and tool-call results in an ordered sequence.
- Mixing markdown context with MCP tool calls and MCP resources.
- Preserving references when expansion is unnecessary or too expensive.

## Workspace Agent Mode

Workspace agent mode should be a specialized mode that operates through subagents and sidecars rather than acting as a single monolithic session.

In this mode, the workspace agent should coordinate support agents that can update default context, skills, relevant file sets, and system prompt fragments when needed. The primary role of the workspace agent is orchestration: it decides what support work is needed, delegates it, and incorporates results into durable workspace state.

Workspace agent mode should make context maintenance an ongoing process instead of a one-time prompt assembly step.

## Event Ingest

Waltr should support event ingest for semantic events represented in markdown. Events may include references to MCP resources, local files, external records, or other structured context.

The event ingest layer should preserve enough semantic information for later processing while keeping raw event payloads inspectable. Markdown is useful as the human-readable carrier, but event references should remain machine-actionable where possible.

## Workspace Event Cursors

Workspace event cursors are long-running processors for event streams. A cursor should reference a workspace, carry a prompt, and support batch processing of matching events.

Each cursor should manage its position in the event stream and define how events are filtered, grouped, and processed. Cursors should be able to use sidecar agents to refine event filters, summarize batches, and remove noise before work reaches the main workspace context.

Potential cursor responsibilities include:

- Tracking unread or unprocessed semantic events for a workspace.
- Applying a prompt to batches of matching events.
- Using sidecars to classify relevance and suppress noise.
- Producing summaries, tickets, documentation updates, or workspace context changes.
- Maintaining durable cursor state so processing can resume after interruption.

## Open Questions

- What authority should sidecar agents have to modify workspace state automatically?
- How should background subagent progress and completion be surfaced to users and host layers?
- What is the durable schema for workspace context, event cursors, and semantic event references?
- Which context injection forms should be part of the public contract versus internal implementation details?
- How should permission policies compose across global config, workspace config, sidecars, and subagents?
