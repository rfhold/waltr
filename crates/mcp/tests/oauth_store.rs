//! In-memory OAuth authorization store conformance.

mod oauth_store_support;

use std::sync::Arc;

use mcp::oauth::InMemoryOAuthAuthorizationStore;

#[tokio::test]
async fn memory_store_passes_shared_contract() {
    let store = Arc::new(InMemoryOAuthAuthorizationStore::new(
        oauth_store_support::config(),
    ));
    oauth_store_support::assert_store_contract(store.clone(), store).await;
}
