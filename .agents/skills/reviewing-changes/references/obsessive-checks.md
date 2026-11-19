# Obsessive checks

## Invariant

Start with what the caller needs to observe. Name the failure a check prevents, which layer already handles it, and what the extra check costs. A possible race or malformed input alone does not establish a need for another mechanism.

## Patterns to inspect

| Pattern | Technical review question |
| --- | --- |
| Serialize concurrent reads and writes | Does the reader require one consistent revision? A file search that promises best-effort results need not hold a workspace lock while another agent edits. |
| Snapshot and compare directories | Does the operation need an exact tree revision or just one file? A before/after tree comparison scans unrelated entries and can reject a valid result because a build wrote a log. |
| Repeat trust checks | Which identity, resource, or authority changes at this boundary? If an admitted caller passes the same bound handle within one operation, a second identity lookup needs a distinct purpose. |
| Duplicate provider validation | If Waltr forwards opaque image bytes, what does a local decode establish that the provider's decoder does not? Keep checks Waltr needs for its own transport or allocations separate from image-format validation. |
| Stop a session on an operation error | If one read times out, can the caller receive that tool error and continue? Distinguish an ordinary failure from loss of an acknowledgement for a side effect. Avoid automatic replay without treating every error as a session failure. |
| Add a transaction at the wrong layer | Which writes must commit together, and who observes a partial result? One atomic database statement rarely needs a caller-owned transaction. A transaction held across a network call cannot roll back the remote effect. |

## Worked example: workspace paths

Suppose a machine exports `~` and two sessions start in `~/repos/a` and `~/repos/b`. Each relative read needs the directory from its own session. Neither read requires the other repository to stay unchanged.

Compare the predicates: “does this session directory resolve inside the export?” validates the requested directory. “Does the export contain any Waltr storage?” tests installation layout instead. In the remote-workspace design, storage under `~` is an accepted layout. Applying the second predicate at registration instead of [standalone](../../../../crates/waltr-server/src/app.rs) or [embedded](../../../../crates/waltr-server/src/embedded.rs) startup still rejects the same installation.

## Suggested test

Use two fixture repositories with the same filename and different contents. Interleave session reads with an unrelated write. Assert each read returns its own repository's file. The test needs no assertion about mutexes, snapshot counts, or validator calls.

## Finding format

`[obsessive-checks] <location>: <mechanism> adds <specific cost or failure> to <operation>. The caller needs <observable guarantee>; <simpler behavior> provides it.`
