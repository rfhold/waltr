# Server Live Tests

Server live provider-run tests belong in `crates/waltr-server/tests/live.rs` and `crates/waltr-server/tests/live/session.rs` and should be ignored by default.

Use them for server contract behavior that requires a real provider-backed session run: command RPCs, background run completion, runtime default resolution, and timeline history/watch behavior across reconnects. Keep assertions at the server boundary by using gRPC clients for mutations and reads.

Structure live provider coverage as separate ignored tests for each provider path, following `crates/waltr-agent/tests/live/` patterns. Each test should skip when its own configuration is unavailable instead of building a single provider matrix that fails unrelated cases.

Cover these provider paths when relevant:

- Direct OpenAI from `OPENAI_API_KEY`.
- Direct Anthropic from `ANTHROPIC_API_KEY`.
- Gateway OpenAI-format and Anthropic-format providers when `WALTR_AGENT_LIVE_GATEWAY=1` is set.

For now, server runtime construction treats OpenAI providers as chat completions. Keep this documented in `docs/testing.md` until config supports selecting other OpenAI APIs.

Run live server session tests with:

`cargo test -p waltr-server --test live -- --ignored`
