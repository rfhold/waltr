#![allow(dead_code)]

use std::collections::BTreeMap;
use std::time::Duration;

use waltr_client::{ConfigChange, ConfigView, ProviderKindView, ProviderView};

pub fn assert_config_matches(actual: &ConfigView, expected: &ConfigView) {
    assert_config_matches_without_etag(actual, expected);
    assert_eq!(actual.etag, expected.etag);
}

pub fn assert_config_matches_without_etag(actual: &ConfigView, expected: &ConfigView) {
    assert_eq!(actual.providers, expected.providers);
}

pub fn assert_watch_event(event: &ConfigChange, initial_etag: &str, patched: &ConfigView) {
    assert_eq!(event.previous_etag, initial_etag);
    assert_eq!(event.etag, patched.etag);
    assert_eq!(event.changed_keys, vec!["providers"]);
    assert!(event.error.is_empty());
    assert_config_matches(
        event
            .config
            .as_ref()
            .expect("watch event should include config"),
        patched,
    );
}

pub fn assert_initial_watch_event(event: &ConfigChange, initial: &ConfigView) {
    assert!(event.previous_etag.is_empty());
    assert_eq!(event.etag, initial.etag);
    assert!(event.changed_keys.is_empty());
    assert!(event.error.is_empty());
    assert_config_matches(
        event
            .config
            .as_ref()
            .expect("initial watch event should include config"),
        initial,
    );
}

pub fn config_with_provider(name: &str) -> ConfigView {
    ConfigView {
        providers: BTreeMap::from([(name.to_owned(), provider_view(name))]),
        mcp_servers: BTreeMap::new(),
        etag: String::new(),
    }
}

pub fn provider_view(name: &str) -> ProviderView {
    ProviderView {
        kind: ProviderKindView::Anthropic,
        name: name.to_owned(),
        url: String::new(),
        headers: BTreeMap::new(),
        models: BTreeMap::new(),
    }
}

pub async fn no_sleep(_delay_ms: u64) {}

#[allow(clippy::future_not_send)]
pub async fn next_snapshot_until(
    stream: &mut waltr_client::ClientStateStream,
    mut matches: impl FnMut(&waltr_client::ClientSnapshot) -> bool,
) -> waltr_client::ClientSnapshot {
    let mut last_snapshot = None;
    for _ in 0..32 {
        let snapshot = tokio::time::timeout(Duration::from_secs(2), stream.next(no_sleep))
            .await
            .expect("shared state stream should make progress");
        if matches(&snapshot) {
            return snapshot;
        }
        last_snapshot = Some(snapshot);
    }

    panic!("shared state stream did not reach expected snapshot; last snapshot: {last_snapshot:?}")
}
