//! In-memory Streamable HTTP session store contract tests.

mod streamable_http_store_support;

use std::{sync::Arc, time::Duration};

use mcp::{
    McpPrincipalId,
    server::{InMemoryStreamableHttpSessionStore, StreamableHttpEventId},
};
use streamable_http_store_support::{
    assert_global_admission_ignores_retained_expired,
    assert_principal_admission_ignores_retained_expired, assert_store_contract,
    global_expiry_admission_config, principal_expiry_admission_config, test_config,
};

#[tokio::test]
async fn in_memory_store_passes_shared_contract() {
    let store = Arc::new(InMemoryStreamableHttpSessionStore::new(test_config()).unwrap());
    assert_store_contract(store.clone(), store).await;
}

#[tokio::test]
async fn in_memory_admission_counts_only_unexpired_sessions() {
    let global = InMemoryStreamableHttpSessionStore::new(global_expiry_admission_config()).unwrap();
    assert_global_admission_ignores_retained_expired(&global).await;

    let principal =
        InMemoryStreamableHttpSessionStore::new(principal_expiry_admission_config()).unwrap();
    assert_principal_admission_ignores_retained_expired(&principal).await;
}

#[test]
fn in_memory_store_rejects_invalid_config() {
    let invalid: [fn(&mut mcp::server::StreamableHttpSessionStoreConfig); 13] = [
        |config| config.session_ttl = Duration::ZERO,
        |config| config.max_sessions = 0,
        |config| config.max_sessions_per_principal = 0,
        |config| config.max_events_per_session = 0,
        |config| config.max_pending_bytes_per_session = 0,
        |config| config.max_event_bytes = 0,
        |config| config.delivery_batch_size = 0,
        |config| config.cleanup_batch_size = 0,
        |config| config.delivery_lease = Duration::ZERO,
        |config| config.initialization_claim_lease = Duration::ZERO,
        |config| config.session_ttl = Duration::from_millis(i64::MAX as u64 + 1),
        |config| config.delivery_lease = Duration::from_millis(i64::MAX as u64 + 1),
        |config| {
            config.initialization_claim_lease = Duration::from_millis(i64::MAX as u64 + 1);
        },
    ];

    for make_invalid in invalid {
        let mut config = test_config();
        make_invalid(&mut config);
        assert!(matches!(
            InMemoryStreamableHttpSessionStore::new(config),
            Err(mcp::server::StreamableHttpSessionStoreError::InvalidData)
        ));
    }
}

#[test]
fn principal_and_event_identifiers_are_strictly_validated() {
    let defaults = mcp::server::StreamableHttpSessionStoreConfig::default();
    assert_eq!(defaults.session_ttl, std::time::Duration::from_hours(24));
    assert_eq!(defaults.max_sessions, 10_000);
    assert_eq!(defaults.max_sessions_per_principal, 32);
    assert_eq!(defaults.max_events_per_session, 1_024);
    assert_eq!(defaults.max_pending_bytes_per_session, 8 * 1024 * 1024);
    assert_eq!(defaults.max_event_bytes, 1024 * 1024);
    assert_eq!(defaults.delivery_batch_size, 128);
    assert_eq!(defaults.cleanup_batch_size, 128);
    assert_eq!(defaults.delivery_lease, std::time::Duration::from_secs(30));
    assert_eq!(
        defaults.initialization_claim_lease,
        std::time::Duration::from_mins(1)
    );

    assert!(McpPrincipalId::new("").is_err());
    assert!(McpPrincipalId::new("x".repeat(257)).is_err());
    assert_eq!(
        McpPrincipalId::new("stable-subject").unwrap().as_str(),
        "stable-subject"
    );

    let id: StreamableHttpEventId = "stream:9223372036854775807".parse().unwrap();
    assert_eq!(id.to_string(), "stream:9223372036854775807");
    for invalid in [
        "",
        "stream",
        ":1",
        "stream:",
        "a:b:1",
        "stream:9223372036854775808",
        "stream:01",
    ] {
        assert!(
            invalid.parse::<StreamableHttpEventId>().is_err(),
            "{invalid}"
        );
    }
    assert!(
        format!("{}:1", "x".repeat(159))
            .parse::<StreamableHttpEventId>()
            .is_err()
    );
}
