# Snapshot disposition

The user approved a separate test-only repair of the 34 expectations below and the MCP test's selection sequence. These changes are not Routine feature goldens. Product renderers, backend dimensions, dependencies, fixtures, and deadlines remain unchanged.

## Evidence and scope

The old expectations come from committed snapshots. Current output comes from full debug/release app and server-backed runs with `INSTA_UPDATE=no`. The renderer files below remain byte-identical to `HEAD`. The MCP test now explicitly selects and asserts the added server before edit and delete.

- [render helper](support/render.rs): an 80 × 24 `TestBackend`; the inference editor uses an explicit 100-column backend.
- [shell footer](../src/ui.rs): right-aligned `Waltr`, two spaces, and `ready`.
- [access renderer](../src/screens/access.rs): existing fixed-width title and wrapped authorization URL.
- [landing renderer](../src/screens/landing.rs): existing title and centered body.
- [splash renderer](../src/screens/splash.rs): existing centered ASCII art.
- [MCP server test](server-backed/config/mcp.rs): creates `remote` before the saved configuration snapshot; the repair targets that server for edit and delete.

For the footer/title rows, the committed ASCII expectation is one column wider than its actual backend: 81 versus 80, or 101 versus 100. Such a row cannot be the output of the current fixed-width renderer. The centered splash rows have unchanged text and an old/new starting column of 21/22. The MCP expectation places `waltr` before `remote`; current ordered configuration rendering places `remote` first and selects index zero. No Routine route is open in any of these cases. This is source/expectation evidence, not a claim that a pristine historical checkout was built against the new frozen client API.

Codes used in the per-file ledger:

| Code | Old expectation → current rendering | Disposition |
| --- | --- | --- |
| F | `Waltr  ready` one column right; ASCII row 81 → 80 | Approved fixed-width expectation repair; renderer unchanged. |
| F100 | Same footer, row 101 → 100 | Approved 100-column expectation repair. |
| A | `Waltr sign in` row 81 → 80 | Approved access title expectation repair. |
| AU | A plus authorization URL wrap: first line ends `redacte` → `redact`, next line `d` → `ed` | Approved access expectation repair; exact URL bytes unchanged. |
| S | Seven unchanged ASCII-art rows start at column 21 → 22 | Approved centered-art expectation repair; source unchanged. |
| L | Title/body/footer ASCII rows 81 → 80; centered `Ready to configure Waltr` shifts left one column | Approved landing expectation repair; source unchanged. |
| M | Selected `waltr` with local OAuth details → selected `remote`, `https://mcp.example/`, status unavailable; F also applies | Approved ordered MCP expectation repair. Explicit target selection and exact server-map assertions now pass through edit and delete in both profiles. |

## Per-file ledger

Paths in this table are relative to `tests/`.

| Snapshot | Classification |
| --- | --- |
| `app/snapshots/app__waltr_access_awaiting_callback.snap` | AU |
| `app/snapshots/app__config_mcp_auth_status_details.snap` | F |
| `app/snapshots/app__config_mcp_create_form_renders_snapshot.snap` | F |
| `app/snapshots/app__config_provider_delete_confirmation_renders_snapshot.snap` | F |
| `app/snapshots/app__config_provider_auth_reset_confirmation.snap` | F |
| `app/snapshots/app__config_provider_auth_status_actions.snap` | F |
| `app/snapshots/app__config_provider_create_form_renders_snapshot.snap` | F |
| `app/snapshots/app__config_explicit_oauth_attempt.snap` | F |
| `app/snapshots/app__app_launches_and_stabilizes_to_snapshot.snap` | S |
| `app/snapshots/app__config_provider_patch_error_renders_snapshot.snap` | F |
| `app/snapshots/app__escape_first_press_arms_exit_notice.snap` | S |
| `app/snapshots/app__ctrl_c_first_press_arms_exit_notice.snap` | S |
| `app/snapshots/app__config_reset_success.snap` | F |
| `app/snapshots/app__config_provider_oauth_fields.snap` | F |
| `app/snapshots/app__config_reset_failure.snap` | F |
| `app/snapshots/app__config_reset_pending.snap` | F |
| `app/snapshots/app__ready_landing_renders_app_shell_and_palette.snap` | L |
| `app/snapshots/app__config_reset_confirmation.snap` | F |
| `app/snapshots/app__session_data_success.snap` | F |
| `app/snapshots/app__session_data_pending.snap` | F |
| `app/snapshots/app__session_data_confirmation.snap` | F |
| `app/snapshots/app__sessions_create_error_renders_snapshot.snap` | F |
| `app/snapshots/app__sessions_delete_confirmation_renders_snapshot.snap` | F |
| `app/snapshots/app__sessions_delete_error_renders_snapshot.snap` | F |
| `app/snapshots/app__sessions_delete_pending_renders_snapshot.snap` | F |
| `app/snapshots/app__sessions_create_pending_renders_snapshot.snap` | F |
| `app/snapshots/app__sessions_loading_renders_snapshot.snap` | F |
| `app/snapshots/app__sessions_inference_editor_renders_layered_snapshot.snap` | F100 |
| `app/snapshots/app__waltr_access_login_required.snap` | A |
| `app/snapshots/app__unknown_leader_command_renders_notice.snap` | S |
| `server-backed/snapshots/server_backed__sessions__sessions_empty_real_server.snap` | F |
| `server-backed/config/snapshots/server_backed__config__providers__config_providers_empty_real_server.snap` | F |
| `server-backed/config/snapshots/server_backed__config__mcp__config_mcp_saved_real_server.snap` | M |
| `server-backed/config/snapshots/server_backed__config__inference_defaults__config_defaults_seeded_real_server.snap` | F |

## First repair verification and stop condition

All 34 repaired snapshot assertions match in debug and release. At the end of this first repair pass, four complete tests still failed after those assertions. Their new failures fell outside that approval. No unlisted snapshot or session-flow code changed. No snapshot auto-update ran, and no `.snap.new` file exists.

The MCP test uses `Up` and the public selected index to assert `remote` before edit and delete. It also asserts the form target, update-command target, delete prompt, and delete-command target. The edit changes the URL to `https://mcp.example/edited`. Exact server-map equality proves that the edit preserves `waltr` and that delete restores the original map.

Final commands and results (`cargo` commands use `--locked -p waltr-tui`; all tests use `INSTA_UPDATE=no`):

| Command suffix | Debug | Release |
| --- | --- | --- |
| `test --test app` | 190 passed, 1 failed, 0 ignored | 190 passed, 1 failed, 0 ignored |
| `test --test server_backed` | 10 passed, 3 failed, 1 ignored | 10 passed, 3 failed, 1 ignored |
| `test --lib` | 57 passed, 0 failed, 0 ignored | 57 passed, 0 failed, 0 ignored |
| `check` | Passed | Not requested |
| `check --features bundled-server` | Passed | Not requested |

The library runs include all 14 native v2 TCP RPC tests. Both app runs pass `routine_detail_end_and_down_reach_receipts_after_long_operation_history`. Both server-backed runs pass `client_stream_creates_edits_and_deletes_mcp_server_with_real_server` through all new assertions.

Final focused MCP reruns each pass: 1 passed, 0 failed, 13 filtered out. The touched Rust file passes `rustfmt --edition 2024 --check`. The TUI diff passes `git diff --check`. A path audit confirms exactly 34 approved snapshot changes; every row matches its 80/100-column backend. The provider-model test remains byte-identical to `HEAD`.

### Newly exposed, unauthorized failures

Paths below are relative to `tests/`. Both profiles produce each failure:

| Test and failure location | Evidence | Disposition |
| --- | --- | --- |
| `session_data_pending_locks_input_and_failure_is_retryable`, `app/session_data.rs:94` | Unlisted `app/snapshots/app__session_data_failure.snap`: footer row 81 → 80. | Stop; snapshot untouched. The approved pending snapshot now matches before this assertion. |
| `client_stream_creates_edits_and_deletes_provider_with_real_server`, `server-backed/config/providers.rs:51` | Unlisted `server-backed/config/snapshots/server_backed__config__providers__config_mcp_empty_real_server.snap`: selected `waltr` row and footer 81 → 80. | Stop; snapshot and test untouched. Later assertions remain unverified. |
| `inference_defaults_snapshot_and_save_use_real_server`, `server-backed/config/inference_defaults.rs:54` | Unlisted `server-backed/config/snapshots/server_backed__config__inference_defaults__config_defaults_saved_real_server.snap`: footer 81 → 80. | Stop; snapshot and test untouched. Later assertions remain unverified. |
| `client_stream_creates_opens_and_deletes_session_with_real_server`, `server-backed/sessions.rs:85–92` | After successful create, the 32-snapshot helper rejects the wait for the created session in `sessions.current`. The last snapshot has a ready create receipt, an empty current list, and detail/history watches that report `loading`. | Stop; no timeout, poll-budget, test, or source change. The later detail snapshot and delete assertions remain unverified. |

At this checkpoint, the Routine real-server watch test was ignored pending the R5 route cutover. Neither these repairs nor the native fake-TCP fixtures proved production v2 routing. The later production-composition milestone removes that ignore and executes the real-server test; independent review and coordinator verification remain necessary.

The prior Clippy result still has two baseline `too_many_arguments` denials in `context.rs` and `timeline.rs`. This pass does not waive them or claim strict warning cleanliness.

## Approved five-snapshot follow-up

The user separately approved five exact expectation repairs and a test-only investigation/correction of the session observation failure. Product code, renderers, fixtures, command order, deadlines, and the 32-snapshot bound remain unchanged. This pass preserves every byte of the prior 34 snapshots and the MCP lifecycle test.

### Exact expectation changes

Paths are relative to `tests/`. Widths below count decoded JSON string content, not quote delimiters. Every changed row is ASCII. All text, URLs, and wrap boundaries remain unchanged; only surplus spaces change.

| Snapshot | Old → new decoded content | Runtime disposition |
| --- | --- | --- |
| `app/snapshots/app__session_data_failure.snap` | Footer: 81 → 80 columns; `Waltr  ready` shifts left one space. | Matches; the complete failure/retry test passes in both profiles. |
| `server-backed/config/snapshots/server_backed__config__providers__config_mcp_empty_real_server.snap` | Footer: 81 → 80 columns. Selected `waltr` URL row: 81 → 80 columns, with one less trailing space. | Matches; the complete provider create/edit/delete test passes in both profiles. |
| `server-backed/config/snapshots/server_backed__config__inference_defaults__config_defaults_saved_real_server.snap` | Footer: 81 → 80 columns; `Waltr  ready` shifts left one space. | Matches; the complete defaults save test passes in both profiles. |
| `server-backed/config/snapshots/server_backed__config__providers__config_defaults_empty_real_server.snap` | Footer: 81 → 80 columns; `Waltr  ready` shifts left one space. | Newly reached; matches within the complete provider lifecycle test in both profiles. |
| `server-backed/snapshots/server_backed__sessions__sessions_detail_real_server.snap` | Footer: 81 → 80 columns; `Waltr  ready` shifts left one space. | Static width/content repair only. The session observation failure still prevents this assertion and later delete assertions. |

### Proven session blocker

Temporary diagnostics captured every outer snapshot, follow-up command, and consumed follow-up snapshot. The diagnostics are absent from the final candidate. Two focused native real-server runs produced the same sequence:

1. The session list watch receives its initial empty list.
2. `CreateSession` completes with a ready receipt and the exact created ID; the app opens that detail route.
3. The app emits timeline-watch registration, then session Routine-history-watch registration, for that same ID.
4. Both consumed registration snapshots still contain an empty session list. Neither carries the missing list observation.
5. The session Routine-history watch reports `failed` with `unimplemented: `, then `loading`, repeatedly through the 32-snapshot budget.
6. Throughout that loop, the timeline watch stays `loading`, and `sessions.current` stays empty.

The shared-client source explains this sequence:

- `src/coordination/runtime.rs:1768–1781` pops queued effects before it polls active watches. The most recent Routine watch effect precedes the earlier timeline effect.
- `src/coordination/runtime.rs:2719–2724` converts the failed session Routine watch open into `SessionInvocationWatchOpenFailed`.
- `src/coordination/routine.rs:989–1024` schedules `SleepSessionInvocationReconnect` after that failure.
- `src/executor.rs:947–954` uses the test's `no_sleep` callback and emits the reconnect event.
- The reconnect handler at `src/coordination/routine.rs:360–362` queues the watch open again. The next queued effect again precedes active-watch observations.

Those source paths are relative to `crates/waltr-client/`. At this checkpoint, the fixture lacked the production v2 route. The unsupported watch/reconnect effect cycle, not a lost follow-up list snapshot, blocked this session lifecycle. This evidence does not establish a pristine historical baseline result.

`apply_snapshot` does not expose consumed follow-up snapshots to its caller's predicate. That structural observation alone does not prove it caused this failure. This pass therefore makes no speculative helper change or false-cause regression. The final helper and session test retain their pre-pass bytes. A product/runtime or route change exceeds this test-only authority; the session flow remains blocked for coordinator disposition.

### Final verification

All test commands use `INSTA_UPDATE=no cargo test --locked -p waltr-tui`:

| Target | Debug | Release |
| --- | --- | --- |
| `--test app` | 191 passed, 0 failed, 0 ignored | 191 passed, 0 failed, 0 ignored |
| `--test server_backed` | 12 passed, 1 failed, 1 ignored | 12 passed, 1 failed, 1 ignored |
| `--lib` | 57 passed, 0 failed, 0 ignored | 57 passed, 0 failed, 0 ignored |

Both complete server-backed runs pass the provider lifecycle, defaults save, and unchanged exact-map MCP lifecycle assertions. The sole failure is the session create/detail/delete flow at its list-observation wait. Its detail snapshot and delete assertions remain unexecuted. All 14 native TCP RPC cases and the receipt-scroll regression still pass.

Both `cargo check --locked -p waltr-tui` and the same command with `--features bundled-server` pass. This pass adds no Rust code, test warnings, dependencies, ignores, sleeps, retries, or timeout changes. The prior Clippy denials remain unwaived; no strict-clean claim applies. The integration ignore recorded at this checkpoint is removed by the later production-composition milestone. Independent targeted review and coordinator verification remain necessary; this is not TUI/R3 acceptance.

The final audit verifies all 35 protected file digests, the exact five additional golden paths, and six decoded ASCII row changes of 81 → 80 columns. All other bytes in those five files match `HEAD`. No `.snap.new` file exists. `git diff --check -- crates/waltr-tui` passes. No final Rust edits remain from this pass, so touched-only rustfmt has no targets.
