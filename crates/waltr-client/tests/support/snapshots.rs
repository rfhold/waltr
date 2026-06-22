#![allow(dead_code, clippy::future_not_send)]

use std::collections::BTreeMap;
use std::time::Duration;

use futures_channel::oneshot;
use waltr_client::{
    ClientCommand, ClientCommandRequest, ClientError, ClientSnapshot, ClientStateStream,
    ClientStateStreamEvent, ConfigChange, ConfigView, ProviderKindView, ProviderView,
};

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
        runtime_defaults: None,
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
pub async fn dispatch_stream_command(
    stream: &mut ClientStateStream,
    command: ClientCommand,
) -> Result<ClientSnapshot, ClientError> {
    for _ in 0..32 {
        let (sender, receiver) = oneshot::channel();
        let event = stream
            .next_or_dispatch(async {
                Some(ClientCommandRequest::with_result(command.clone(), sender))
            })
            .await;

        let ClientStateStreamEvent::Snapshot(snapshot) = event else {
            panic!("shared state stream closed while dispatching command");
        };

        if let Ok(result) = receiver.await {
            if let Ok(result_snapshot) = &result {
                assert_eq!(snapshot.as_ref(), result_snapshot);
            }
            return result;
        }
    }

    panic!("shared state stream did not accept command before retry limit");
}

pub async fn next_snapshot_until(
    stream: &mut ClientStateStream,
    mut matches: impl FnMut(&ClientSnapshot) -> bool,
) -> ClientSnapshot {
    let mut last_snapshot = None;
    for _ in 0..32 {
        let snapshot = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("shared state stream should make progress");
        if matches(&snapshot) {
            return snapshot;
        }
        last_snapshot = Some(snapshot);
    }

    panic!("shared state stream did not reach expected snapshot; last snapshot: {last_snapshot:?}")
}
