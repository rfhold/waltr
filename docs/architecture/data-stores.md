# Data stores

`waltr-datastore` owns storage and transactions. `waltr-server` owns the catalog, authorization, MCP surface, and Code Mode invocation lifecycle. The server includes data stores on standalone, desktop, and Android targets without a feature flag.

## Definitions and ownership

Put definitions in `.waltr/resources/data-stores/`. The server selects the storage implementation, not the definition.

```yaml
apiVersion: waltr.dev/v1alpha1
kind: DataStore
metadata: {name: reports}
spec:
  type: object
  access: {scope: principal}
```

```yaml
apiVersion: waltr.dev/v1alpha1
kind: DataStore
metadata: {name: research}
spec:
  type: relational
  access: {scope: shared}
  collections:
    projects:
      fields:
        title: {type: string, required: true}
        status: {type: string, indexed: true}
    notes:
      fields:
        body: {type: string, required: true}
  relations:
    project_notes: {from: projects, to: notes, cardinality: one-to-many}
```

A principal store has one partition per user. A shared store has one partition for authorized callers. Waltr records the submitting user as `created_by` and `last_edited_by`; edits preserve a known creator. An anonymous caller has no principal partition and cannot mutate a store. An embedded server uses a stable local profile identity.

A removed definition hides its data but does not delete it. The same name, type, and scope reopen that namespace. A rename does not move files or rows. Do not change a schema incompatibly without a data migration. Resource edits retain ordinary catalog ETags; data items have no revision counters.

## External writers

A fresh list, read, or query uses the backend directly. External writes need no import or restart. A transaction keeps its database snapshot.

- Filesystem content is under `<root>/object/<store>/<partition>/<key>`. S3 uses the same layout under the configured prefix. Use ordinary files and portable relative UTF-8 keys. Do not use symlinks or write into the receipt namespace.
- The partition is `shared` or `principal-` followed by the lowercase SHA-256 hex of the trusted actor key. Choose the partition explicitly for an administrative external write. The MCP API never accepts a partition key.
- PostgreSQL uses the `waltr_datastore` schema. SQLite uses a separate `datastore.sqlite3` file. The physical contracts are the [PostgreSQL](../../crates/waltr-datastore/migrations/postgres/0001_initial.sql) and [SQLite](../../crates/waltr-datastore/migrations/sqlite/0001_initial.sql) migrations.
- Write business fields in `records(namespace, collection, id, fields)`, with namespace `relational/<store>/<partition>`. Use `links` with the declared relation, endpoint collections and IDs, and cardinality. Do not write actor columns, field-index state, schema metadata, or commit receipts. Record deletion removes incident links, not the opposite records.
- Once Waltr registers the definition, an external writer can insert the first link without a prior Waltr write. Both endpoints must exist in the same partition.
- External edits can leave authorship unknown. Object write receipts distinguish verified current authorship from the last known Waltr write. Neither a matching digest nor an old receipt proves the identity of an external writer.

## MCP

Read `waltr://data-stores` to discover authorized stores. Resource templates identify object descriptors and content, collection schemas, and records. Use the canonical URIs returned by the server for nested keys.

Hosted MCP requires the exact `datastore:<tool>:<action>` OAuth scope for each mutation. For example, `datastore:destroy:object.delete` authorizes object deletion, not record deletion. The same check applies to public Code Mode. Read-only callers can query, read resources, and download authorized content.

Hosted sign-in defaults to object create and replace, record create and update, and relation link, in addition to the endpoint scopes. Consent lists these permissions. Clients can request fewer scopes explicitly. Delete and upload scopes are not defaults. Current grants do not gain scopes on refresh; sign in again to authorize the new defaults.

| Tool | Actions |
| --- | --- |
| `query` | `object.list`, `object.download_chunk`, `record.find`, `relation.list` |
| `create` | `object.create`, `object.upload`, `object.download`, `record.create`, `relation.link` |
| `execute` | `object.replace`, `object.upload_chunk`, `record.update` |
| `destroy` | `object.delete`, `object.close_transfer`, `record.delete`, `relation.unlink` |

An action takes an `input` object with `store`. Object create and replace take `key` and either inline `content_base64` or an upload `transfer`. For larger content, open `object.upload` with `key` and `size`, then send `object.upload_chunk` with contiguous byte offsets. Create or replace consumes the completed upload. For downloads, open `object.download` with `key` and use `object.download_chunk` until `eof`. Transfer handles expire after 30 seconds and never grant access to another user or store.

Records use logical collection names, IDs, and typed fields. Record queries accept equality filters in `where`, typed predicates in `filters_by`, and an optional sort. Pass the returned `next_cursor` back with the same query. Later pages outside a transaction read fresh data.

For example, call `query` with these arguments to find active projects:

```json
{
  "action": "record.find",
  "input": {
    "store": "research",
    "collection": "projects",
    "where": {"status": "active"},
    "limit": 10
  }
}
```

## Code Mode

`code.execute` accepts `transaction: "relational"` for one native serializable transaction. Ordinary first-party datastore calls and record reads share it. The mode forbids object effects, external tools, and helpers. Failed mutations prevent commit, even if JavaScript catches the error.

Without the selector, datastore writes commit independently. No tool argument, resource URI, or MCP session ID creates transaction authority. Public Code Mode still requires its own grant and does not proxy upstream tools.

Waltr commits a receipt with relational data. It uses that receipt to reconcile a lost commit acknowledgment and never replays source to resolve uncertainty.
After a restart, a receipt can confirm the commit even if no Routine result survives. The operation reports commit confirmation and the invocation remains uncertain.

## Deployment

Embedded servers use filesystem and SQLite with app-private paths. Standalone servers default to the filesystem and SQLite in the Waltr data directory. `WALTR_DATASTORE_OBJECT_ROOT` overrides the filesystem root. `WALTR_DATASTORE_DATABASE_URL` selects an absolute `sqlite:` path or a PostgreSQL URL. `WALTR_DATASTORE_OBJECT_STORE=s3` selects S3 and requires `WALTR_DATASTORE_S3_ENDPOINT`, `WALTR_DATASTORE_S3_REGION`, `WALTR_DATASTORE_S3_ACCESS_KEY_ID`, `WALTR_DATASTORE_S3_SECRET_ACCESS_KEY`, `WALTR_DATASTORE_S3_BUCKET`, `WALTR_DATASTORE_S3_PREFIX`, and `WALTR_DATASTORE_S3_FORCE_PATH_STYLE`.

Keep the filesystem root and SQLite file outside the agent workspace. The server rejects overlapping paths at startup.

S3 datastore replacement normally uses `If-Match`. For gateways that reject conditional PUTs,
set `WALTR_DATASTORE_S3_ALLOW_STALE_OVERWRITE=true` on the standalone server and restart it.
This skips `If-Match` for replacement content and updates to its existing receipt.
Replacement still checks that the object exists first. A concurrent write can be lost,
or a concurrent delete can be undone. Create-only writes and deletes keep their conditions.
Unset the variable or set it to `false` to restore conditional replacement.

Back up SQL data and object bytes with their receipts. Ship server and catalog reader binaries before publishing a `DataStore` definition to them. Do not share active definitions with older binaries that cannot parse the new kind.
