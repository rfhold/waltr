# Obsessive Legacy Migration Handling

Keep one supported contract unless a deployed consumer, persisted data, or an approved staged rollout needs another. A draft, branch, or local test fixture is not a shipped contract.

Every extra mode adds a configuration choice and a behavior to test. Migration wording can make operators look for a step they do not need.

## Review

- Trace feature flags and opt-ins through startup, deployment, tests, and documentation. Name the rollout or operator choice each setting serves.
- Check protocol fallbacks, negotiation fields, and reservations against release artifacts and supported version skew. Preserve field reservations for shipped fields.
- Check for legacy wording in errors, comments, and guides. Describe the current contract unless operators must take a migration step.
- Keep real access and compatibility boundaries. Removing an opt-in does not remove authentication. Lack of a production deployment alone does not rule out other supported consumers.

## Examples

- `WALTR_WORKSPACE_PEERS` made operators opt in to hosted workspace registration after configuring OIDC. Remove the extra switch and test registration on the normal OIDC deployment. Leave the route unavailable without OIDC.
- The draft sandbox `Hello.supports_working_directory` field negotiated a capability all supported workers provide. It never shipped to a supported consumer. Remove the field, its draft reservation, and fallback handling. Keep worker-side directory validation.

Inspect the server composition, deployment examples, proto source, and generated contract. Verify the supported path without the removed switch or negotiation. Check the relevant authentication and routing tests.

Report the extra mode or wording, the evidence about affected consumers, and the specific code, configuration, or docs to remove.
