# Feature Testing

Waltr intentionally repeats feature coverage across layers to preserve behavior at each boundary.

# Layered Coverage

Feature coverage should start at the most isolated layer that owns the behavior, then repeat through higher layers when the feature crosses those boundaries.

| Layer | Test Intent |
| --- | --- |
| Agent | Agent library behavior from a library consumer's perspective. |
| Server | Server contract and coordination behavior. |
| Client | Server plus shared client state-machine behavior. |
| Web | Browser UI behavior over client-owned state. |
| TUI | Terminal UI behavior over client-owned state. |

# Expectations

Add or update tests at each affected layer when a feature crosses that layer.

Do not remove higher-layer coverage just because lower-layer coverage exists.

Keep each layer's assertions focused on that layer's responsibility.

Use shared support for setup, but keep the test perspective layer-specific.
