# Index

| Path | Info |
| --- | --- |
| [README.md](README.md) | Public Rust contracts, JavaScript ABI, budgets, and ownership. |
| [PROVENANCE.md](PROVENANCE.md) | Interpreter source, compiler, artifact, and rebuild evidence. |
| [src/engine.rs](src/engine.rs) | Admission, execution ownership, async terminal notification, and thread reaper. |
| [src/host.rs](src/host.rs) | Dispatcher boundary, opaque callbacks, and invocation resource identities. |
| [src/backend/](src/backend/) | Sole private QuickJS-NG/Wasmi backend. |
| [wasm/reactor.c](wasm/reactor.c) | Private C bridge and descriptor-only value transport. |
| [wasm/quickjs-budget.patch](wasm/quickjs-budget.patch) | WASI C-stack enforcement and native budget hooks declared in [budgets.h](wasm/budgets.h). |
| [tests/](tests/) | Fake-host contracts, disposable watchdog, and consumer substitution. |

# Boundaries

This crate owns the interpreter, not server policy, resources, or integrations.
Operation names carry no authority.
The dispatcher owns authorization and actual resources.
No native fallback, JIT, server dependency, or ambient guest I/O belongs here.

# Contracts

Keep interpreter types private.
Validate source without evaluation.
Keep host futures cooperative, owned, and free from detached tasks.
Destroy guest memory and host futures before host cleanup.
Publish terminal reports only after the reaper joins the worker.
Keep cancellation control independent from execution ownership; control-handle drop does not cancel.
Keep the deadline default at 30 seconds and its hard cap at 90 minutes; longer deadlines never replenish cumulative fuel or reset resource quotas.
Retain admission reservations after failed host cleanup.

# Hints

Use the repository test-modification skill for test changes.
Use the documenting skill for documentation changes.
Rebuild interpreter artifacts only through [scripts/rebuild.sh](scripts/rebuild.sh).
Keep the artifact checksum and provenance current after bridge, header, or QuickJS patch changes.
