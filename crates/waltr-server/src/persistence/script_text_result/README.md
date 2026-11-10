# Durable business text evidence

The production host uses this adapter for explicit selected business text, not automatic task history, intermediate output, or binary media.
Immutable filesystem or S3 objects own the UTF-8 payloads.
Waltr SQL owns descriptors, invocation ownership, quota counters, and object references, not text payload bytes.

## Bounds and ownership

- Each result contains at most 1 MiB of UTF-8 octets.
- Each invocation retains at most 32 immutable names and 8 MiB of canonical JSON envelope charges.
- The envelope includes text, escaped name, descriptor, optional helper reference, and its own decimal charge.
- The charge converges to the exact serialized byte count; the counter never allocates escaped text.
- Sources write into a caller-owned buffer of at most 64 KiB.
- Whole-source UTF-8 validation accepts split code points and rejects invalid or incomplete sequences before object or SQL publication.
- The actual stored invocation principal authorizes publication before source access.
- A process-wide 64 MiB credit pool reserves 8 MiB per publisher before content allocation.
- Object reads reserve 1 MiB plus 64 KiB of process credit before payload access.
- Each returned chunk retains a separate 64 KiB credit until Drop.
- Read offsets count raw octets, not characters. A chunk boundary can split a code point.
- Source Drop releases local ownership. It does not claim an asynchronous producer join.

Publication reads the bounded source and persists a verified immutable object before its SQL transaction starts.
The transaction locks the actual invocation row, rechecks principal and status, charges quota, and inserts the descriptor and object reference atomically.
An unsuccessful SQL publication can leave an unreferenced object; no orphan collector exists.
Published text survives later invocation failure.
Cancellation before the final check aborts SQL publication. After that check, the adapter awaits commit and reports its outcome.
A commit error returns `Uncertain`; callers inspect authorized descriptors instead of automatic replay.

List and read operations authorize the actual invocation principal before descriptor or payload access.
Reads check SQL descriptor consistency, object size and digest, and whole-payload UTF-8 before they return a bounded chunk.

## Focused test procedures

```sh
cargo test -p waltr-server --locked --no-default-features --features test --lib text_result_
cargo test -p waltr-server --locked --no-default-features --features test --lib text_result_ --release
cargo check -p waltr-server --locked --no-default-features --features test
```

`text_result_sqlite_additive_contract` applies migrations 1–9, seeds an outbound OAuth sentinel, then applies migrations 10 and 16.
It checks exact aggregate quota, concurrent publication, immutable names, authorization, UTF-8, chunks, and later-failure retention.
The prior JavaScript invocation fixture uses the current admission and start APIs after those migrations.
Before-and-after snapshots compare the loaded invocation representation across text publications, not legacy task-DSL history or raw SQL/event bytes.
The test also preserves a Waltr-owned outbound OAuth credential row and a separate invocation principal.
It creates no fake session, mask, or config tables.

`text_result_sqlite_watchdog_transaction_and_credit_evidence` creates an empty private SQLite file and starts one child test process.
The parent enforces a 90-second watchdog, kills and reaps on failure, and deletes only its own fixture paths.
The child accepts only the private empty-file layout; it does not read configured database URLs.
The parent and child share `TEXT_RESULT_PRIVATE_CHILD_FILE`, the `text-result-evidence-` directory prefix, and the `TEXT_RESULT_EVIDENCE_COMPLETE` marker.
The child runs these evidence cases sequentially:

| Case | Assertion |
| --- | --- |
| SQL lock gate | A real publisher holds its UPDATE lock; a separate connection returns SQLite BUSY. |
| Terminal lock waiter | An already-checked-out connection submits the invocation lock statement on a witness invocation while the publisher holds SQLite's writer lock. |
| Publish versus terminal | Concurrent terminalization completes after publication; the business result remains visible. |
| Terminal first | Completed terminalization rejects publication before any source read. |
| Pre-commit cancellation | A gate after INSERT precedes the final cancellation check; cancellation leaves no result or quota charge. |
| Commit wins | Cancellation after the final check does not falsely report rollback; the real SQL commit succeeds. |
| Lost acknowledgment | A private hook returns `Uncertain` after real commit; list/read recover exactly one named result without source replay. |
| Insert fault | An actual SQLite trigger aborts INSERT after quota UPDATE; rollback removes the quota mutation. |
| Resource faults | Source panic, failed source, incomplete UTF-8, pending future Drop, and open-transaction Drop return all process credit. |
| Reader ownership | A 64 KiB chunk retains the only available credit; Drop permits exact reclamation. |
| Separate pools | Two independent SQLite pools synchronize source EOF; quota CAS admits one escaped 6 MiB envelope, not two. |

The hooks compile only under `cfg(test)`.
Their registry permits four bounded entries; RAII removal releases any paused gate.
The child process isolates global-credit assertions from other tests.
SQL/source order uses explicit gates and barriers. The parent polls elapsed time only for its watchdog.

## Database boundaries and PostgreSQL guard

Agent sessions and masks belong to the agent session store, not these Waltr SQL tables.
The resource catalog uses file-backed configuration, not a Waltr SQL config table.
Hosted MCP OAuth belongs to the separate MCP database/schema owner.
These tests do not claim preservation of those separate stores.
See [persistence ownership](../../../../../docs/architecture/persistence.md) and [configuration ownership](../../../../../docs/architecture/config.md).

The ignored PostgreSQL test reads `SCRIPT_TEXT_RESULT_POSTGRES_URL` and requires a newly created disposable database without any production backing.
Its guard requires `127.0.0.1`, an explicit port, user `script_text_result`, database `script_text_result_ephemeral`, and no query or fragment overrides.
`CREATE SCHEMA waltr` must fail if that schema exists; the test never resets a target.
Those checks prove only the target guard and absence of the Waltr schema, not database freshness.
The caller must establish freshness separately, such as through a new disposable container.
The transaction/fault gates exercise SQLite only.
The PostgreSQL fixture uses the same current JavaScript invocation APIs and object-reference migration as the SQLite contract fixture.
The quota race uses two pools inside one isolated child, not two concurrent writer processes.
