# Timeline

The agent timeline is the public, durable event stream produced by `crates/waltr-agent/`. It describes assistant text, provider thinking, tool calls, tool progress, tool results, external requests, and session status without depending on server, client, TUI, or web layers.

## Owning Layer

`crates/waltr-agent/` owns the timeline model and session-store contract:

| Type | Purpose |
| --- | --- |
| `AgentTimelineEvent` | Cursor-assigned event persisted in a session timeline. |
| `PendingTimelineEvent` | Event before a store assigns its durable cursor. |
| `TimelineCursor` | Monotonic per-session cursor assigned at append time. |
| `TimelineQuery` | Bounded timeline range query with direction, cursor, limit, and optional channel filtering. |
| `TimelineItemText` | Reconstructed text-like content for one timeline item. |
| `BoxAgentTimelineStream` | Host-facing live stream of cursor-assigned timeline events. |

Host layers may expose, transform, or fan out timeline events, but transport shape and UI rendering are outside the agent crate.

## Channels

Timeline channels are string-backed display categories. Built-in channel constants are exported by the agent crate.

| Channel | Content |
| --- | --- |
| `assistant_text` | Assistant response text deltas. |
| `thinking` | Provider reasoning or thinking text deltas. |
| `tool_call` | Provider-neutral tool call preview. |
| `tool_progress_info` | Informational tool progress. |
| `tool_progress_error` | Error-oriented tool progress. |
| `tool_result` | Provider-neutral tool result. |
| `external_request` | Host input request such as permission or question handling. |

Some status events are channel-less. Channel-filtered queries include only events whose `channel` is present in the requested channel set.

## Event Semantics

| Area | Contract |
| --- | --- |
| Cursor assignment | Every appended timeline event receives a stable, monotonic `TimelineCursor` within its session. |
| Assistant text | Provider text deltas emit `ContentDelta` events on `assistant_text`. Deltas for one assistant turn use a stable item id. |
| Thinking | Provider thinking deltas emit `ContentDelta` events on `thinking`. Deltas for one thinking item use a stable item id. |
| Tool call preview | A provider tool call emits an `ItemStarted` event on `tool_call` before local tool execution starts. |
| Tool progress | Tool progress emits timeline events while the tool runs. Progress events set `parent_id` to the tool call item id. |
| Tool result | Tool completion emits an `ItemFinished` event on `tool_result` with `parent_id` set to the tool call item id. |
| External request | Host-required input emits opened and resolved external request events on `external_request`. |
| Session status | Runtime status changes may emit channel-less status events. |

The timeline is an immutable event log. Consumers that need rendered messages or item snapshots should derive them from events outside the agent crate.

## Querying

`SessionStore::timeline` reads durable timeline ranges with `TimelineQuery`.

| Query Field | Behavior |
| --- | --- |
| `direction: Forward` | Reads events after `cursor`, or from the start when `cursor` is absent. |
| `direction: Backward` | Reads events before `cursor`, or from the end when `cursor` is absent. |
| `cursor` | Exclusive range boundary. |
| `limit` | Maximum returned event count. `0` returns no events. |
| `channels` | Optional channel filter. `None` includes all events. |

Returned events are always in persisted chronological order. Backward queries select the newest matching events before the boundary, then return that selected page oldest-to-newest.

`SessionStore::timeline_after` and `SessionStore::timeline_before` remain convenience wrappers around `TimelineQuery`.

## Text Retrieval

`SessionStore::timeline_item_text` reconstructs text-like content for one `TimelineItemId` from persisted events.

| Content Variant | Behavior |
| --- | --- |
| `TextDelta` | Appends the delta to the reconstructed text. |
| `TextSnapshot` | Replaces the reconstructed text with the snapshot. |
| `ContentRef` | Returns `TimelineContentUnavailable` until out-of-line content lookup exists. |
| Other variants | Do not contribute text. |

The method distinguishes a missing item from an existing item with no text-like content. Existing non-text items return empty reconstructed text with byte count `0`.

## Boundaries

- The agent crate owns durable event shape, cursor ordering, range queries, and item text reconstruction.
- The agent crate does not own server subscriptions, multi-client fanout, transport backpressure, UI capping policy, scroll behavior, or rendered message projections.
- Storage implementations may optimize query execution later without changing the public `SessionStore` contract.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage lives in:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent/src/agent/session/storage/memory.rs](../../crates/waltr-agent/src/agent/session/storage/memory.rs) | Store query, cursor ordering, channel filtering, and text reconstruction semantics. |
| [../../crates/waltr-agent/tests/agent/loop.rs](../../crates/waltr-agent/tests/agent/loop.rs) | Live loop event ordering, assistant/thinking deltas, tool progress parenting, and durable text reconstruction. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/src/timeline.rs](../../crates/waltr-agent/src/timeline.rs) | Public timeline types. |
| [../../crates/waltr-agent/src/agent/event.rs](../../crates/waltr-agent/src/agent/event.rs) | Timeline stream type. |
| [../../crates/waltr-agent/src/agent/loop.rs](../../crates/waltr-agent/src/agent/loop.rs) | Runtime timeline emission. |
| [../../crates/waltr-agent/src/agent/tool.rs](../../crates/waltr-agent/src/agent/tool.rs) | Tool progress sink. |
| [../../crates/waltr-agent/src/agent/session/storage/mod.rs](../../crates/waltr-agent/src/agent/session/storage/mod.rs) | `SessionStore` timeline contract. |
| [storage.md](storage.md) | Durable session and storage behavior. |
