//! Config integration test aggregator.

mod support;

// Full application fixtures retain the process-global Routine blob store.
macro_rules! isolate_config_test {
    () => {
        if !waltr_server::test_support::isolated_process_test(
            std::thread::current()
                .name()
                .expect("named Cargo test thread"),
        ) {
            return;
        }
    };
}

#[path = "config/read.rs"]
mod read;

#[path = "config/watch.rs"]
mod watch;

#[path = "config/providers.rs"]
mod providers;

#[path = "config/stale_etag.rs"]
mod stale_etag;

#[path = "config/source.rs"]
mod source;

#[path = "config/reset.rs"]
mod reset;

#[allow(
    clippy::future_not_send,
    reason = "the client state stream intentionally runs on a local task set"
)]
async fn config_patch_result(
    stream: &mut waltr_client::ClientStateStream,
    started: &waltr_client::ClientSnapshot,
    expected_status: &str,
) -> waltr_client::ClientSnapshot {
    let key = started
        .config
        .patch
        .key
        .as_ref()
        .expect("patch operation key");
    // A real catalog write may rebuild and validate the catalog twice before
    // emitting a snapshot. The isolated test's watchdog bounds the entire wait.
    for _ in 0..32 {
        let snapshot = stream.next().await;
        if let Some(waltr_client::ClientOperationView::ConfigPatch {
            status: status @ ("ready" | "failed"),
            error,
            ..
        }) = snapshot.operations.current.get(key)
        {
            assert_eq!(*status, expected_status, "config patch {key}: {error:?}");
            return snapshot;
        }
    }
    panic!(
        "config patch {key} did not complete: {:?}",
        stream.snapshot().config.patch
    );
}
