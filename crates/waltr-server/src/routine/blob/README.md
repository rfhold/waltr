# Temporary blob foundation

[`blob.rs`](../blob.rs) defines the consumer-owned `BlobStore` and `Clock` ports.
[`filesystem.rs`](filesystem.rs) exposes the Unix filesystem adapter.
This module has no production routes, agent attachments, SQL records, or durable raw-media references.

## Authority and ownership

The host supplies authenticated principal and invocation identities.
Opaque IDs include a random process generation and random token.
The store checks both authentication and typed object state on every operation.
No public constructor or serializer grants object authority.
Display names never select filesystem names.
Ordinary files remain inert, regardless of their MIME type.
The module fetches no URLs and extracts no archives.

Uploads remain in quarantine until exact-length and incremental SHA-256 checks succeed.
An optional declared length reserves its complete byte budget before file creation.
Unknown-length uploads reserve each chunk before its write.
Sequence numbers start at zero; duplicates, gaps, and reordering invalidate the upload.
Consumers must not retry failed operations automatically.
Abandoned successful mutation replies revoke the object; abandoned read replies revoke the cursor rather than permit partial-consumption retries.

Output transfer changes consumption authority atomically and revokes prior invocation readers.
It preserves the original invocation, principal, and process charges.
The output lease expires 15 minutes after transfer, without renewal or read-based extension.
Invocation closure preserves only explicit output transfers.
Expiry immediately denies access; filesystem deletion can complete later.

## Filesystem requirements

The host supplies an empty directory with mode `0700` and the effective user's ownership.
The adapter permits one active store per process so independent stores cannot bypass process quotas.
An exclusive nonblocking directory lock rejects concurrent use of the same root.
A nonempty root fails construction; operators handle crash orphans explicitly.
The adapter never recursively deletes or adopts prior namespaces.

The adapter walks each absolute path component with descriptor-relative `openat` and `NOFOLLOW`.
It checks ancestor ownership and rejects group or other write permission.
It rejects workspace overlap through directory device/inode identities.
It creates one random private namespace and random UUID files with exclusive creation.
Namespace permissions are `0700`; file permissions are `0600`.
All later operations use retained directory or file descriptors, not revalidated path strings.
Cleanup unlinks only exact names that this instance created.

These guarantees require local Unix filesystem semantics and functional advisory locks.
The adapter rejects other platforms explicitly.
It does not defend against a privileged actor or another hostile process with the same effective user.
The host must exclude the root from worker mounts and MCP/shell filesystem capabilities.
Workspace overlap checks alone do not restrict an unrestricted same-user shell.
The host must preserve that isolation after construction.

## Budgets and cleanup

| Resource | Limit |
| --- | --- |
| File / image encoded bytes | 64 MiB / 20 MiB |
| Invocation / principal / process bytes | 512 MiB / 1 GiB / 4 GiB |
| Invocation / principal / process objects | 128 / 1024 / 4096 |
| Readers per object / invocation | 4 / 32 |
| Read or write chunk | 64 KiB |
| Request admission / cleanup queue | 32 / 4096 |
| Cleanup workers | 2 |

Reservations include partial uploads, output leases, and failed deletion quarantine.
The store releases byte and object charges only after descriptor closure and verified unlink success.
Reader handles retain no external file descriptors; revocation releases their cursor credit.
The driver owns cleanup after cancellation, owner drop, or a dropped shutdown waiter.
Explicit shutdown cancels admission, drains cleanup, and joins the workers and driver.
Deletion failure reports `Cleanup` and retains service ownership and charges for a later shutdown or cleanup retry.
Last-store drop requests shutdown; its service retries failed deletion until success.
Callers that need completion evidence must await explicit shutdown before runtime teardown.

The driver uses a dedicated Tokio blocking task for trusted filesystem calls.
Cooperative waits observe cancellation and deadlines.
The adapter cannot impose a hard deadline on arbitrary kernel or filesystem waits.

## Image signatures

Image admission requires the exact MIME type for PNG, JPEG, or WebP.
Commit checks PNG's eight-byte signature, JPEG's two-byte SOI, or WebP's twelve-byte RIFF/WEBP signature.
Empty inputs, incomplete signatures, and format mismatches fail commit.
The store does not parse the body, decode pixels, or rewrite bytes.
A malformed body with an allowed signature can pass; providers own full decoding and ordinary encoding errors.
The internal image marker records only the checked format, not dimensions or trusted pixel counts.
Optional wire image metadata remains unset, and no decompression-bomb protection exists here.
Temporary blob IDs cannot enter durable agent sessions without a separate explicit durable-ownership policy.

[`tests.rs`](tests.rs) covers private filesystem behavior, integrity, authority, quotas, expiry, cancellation, and cleanup faults.

Fault tests pause inside the filesystem read, exhaust byte budgets with transferred outputs, saturate request admission, and contend on root locks from a separate process.
They also exercise last-store-drop retry and real Unix unlink permission failure.
The permission test reports `UNVERIFIED` if privilege or filesystem semantics bypass directory write permissions.
Test-only barriers have bounded waits and release guards; they do not change production path checks or limits.
