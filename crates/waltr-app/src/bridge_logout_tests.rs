use super::*;
use crate::native_media::tests::raw_download_tests::Case;
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct Effects {
    received: AtomicUsize,
    completed: AtomicUsize,
    access_stops: AtomicUsize,
    deletes: AtomicUsize,
    shared_logouts: AtomicUsize,
    published_download: AtomicUsize,
}
struct Control {
    gate: Arc<tokio::sync::Semaphore>,
    credentials: NativeCredentialProvider,
    authorized_closes: Arc<AtomicUsize>,
    credits: Arc<tokio::sync::Semaphore>,
}
async fn fixture(failed_close: bool) -> (Arc<RuntimeBridge>, Arc<Effects>, Control) {
    let bridge = Arc::new(RuntimeBridge::new(EmbeddedServerController::default()));
    bridge.select(RuntimeTarget {
        id: "logout-test".into(),
        endpoint: Some("http://127.0.0.1:0".into()),
        ..RuntimeTarget::default()
    });
    bridge.active_generation.store(1, Ordering::Release);
    let current = bridge.active_generation.clone();
    let (requests, mut receiver) = mpsc::channel(CHANNEL_CAPACITY);
    let (stop, mut stopped) = watch::channel(false);
    let (platform, _) = watch::channel(PlatformStartup::Ready);
    let (ready, ready_rx) = oneshot::channel();
    let effects = Arc::new(Effects::default());
    let observed = effects.clone();
    let thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&runtime, async move {
            let case = Case::authenticated().await;
            case.fail_close(failed_close);
            let pending = case.begin();
            case.entered().await;
            let credentials = case.credentials();
            let audio = crate::dictation::Host::new(case.audio());
            let mut audio_tasks = tokio::task::JoinSet::new();
            ready.send(Control { gate: case.gate.clone(), credentials: credentials.clone(), authorized_closes: case.authorized_closes(), credits: case.credits() }).ok().unwrap();
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    request = receiver.recv() => {
                        let Some(HostRequest::LogoutAccess { reply }) = request else { panic!("public logout request expected") };
                        observed.received.fetch_add(1, Ordering::SeqCst);
                        // Exercise the production gate through the public bridge request channel;
                        // replace only platform/auth effects, never the cleanup owners or RPCs.
                        let result = complete_logout(&case.host, (&audio, &mut audio_tasks, 1), || current.load(Ordering::Acquire) == 1, async || {
                            assert_eq!(case.tasks(), 0, "actual driver handles must be joined before credentials change");
                            observed.access_stops.fetch_add(1, Ordering::SeqCst);
                            let _lease = credentials.clear_bearer_token();
                            observed.deletes.fetch_add(1, Ordering::SeqCst);
                            observed.shared_logouts.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        }).await;
                        let _sent = reply.send(result);
                        observed.completed.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
            if pending.await.unwrap().is_ok() { observed.published_download.fetch_add(1, Ordering::SeqCst); }
            case.shutdown(failed_close).await;
        });
    });
    *bridge.running.lock().unwrap() = Some(RunningGeneration {
        generation: 1,
        requests,
        stop,
        platform,
        recovery: Arc::new(RwLock::new(RecoveryState::new(1))),
        thread,
    });
    (bridge, effects, ready_rx.await.unwrap())
}
async fn count(counter: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while counter.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn preserved(effects: &Effects, control: &Control, revision: u64) {
    assert_eq!(control.credentials.credential_revision(), revision);
    assert_eq!(effects.access_stops.load(Ordering::SeqCst), 0);
    assert_eq!(effects.deletes.load(Ordering::SeqCst), 0);
    assert_eq!(effects.shared_logouts.load(Ordering::SeqCst), 0);
}
async fn logout(bridge: &RuntimeBridge) -> Result<(), String> {
    bridge
        .access_action(1, waltr_client::NativeAccessAction::Logout)
        .await
}
async fn late_close_finished(control: &Control) {
    count(&control.authorized_closes, 1).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while control.credits.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn logout_barrier_keeps_credentials_for_late_reader_then_explicit_retry_joins_and_deletes_once()
 {
    let (bridge, effects, control) = fixture(false).await;
    let revision = control.credentials.credential_revision();
    assert_eq!(
        logout(&bridge).await.unwrap_err(),
        "Native media cleanup could not be verified"
    );
    preserved(&effects, &control, revision);
    assert!(
        logout(&bridge).await.is_err(),
        "a second drain must not forget uncertainty"
    );
    preserved(&effects, &control, revision);
    control.gate.add_permits(1);
    late_close_finished(&control).await;
    preserved(&effects, &control, revision);
    logout(&bridge).await.unwrap();
    assert_eq!(effects.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(effects.access_stops.load(Ordering::SeqCst), 1);
    assert_eq!(effects.shared_logouts.load(Ordering::SeqCst), 1);
    assert_ne!(control.credentials.credential_revision(), revision);
    bridge.stop();
    assert_eq!(effects.published_download.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn logout_barrier_dropped_caller_and_stale_generation_cannot_clear_credentials() {
    for stale in [false, true] {
        let (bridge, effects, control) = fixture(false).await;
        let revision = control.credentials.credential_revision();
        assert!(
            bridge
                .access_action(99, waltr_client::NativeAccessAction::Logout)
                .await
                .is_err()
        );
        assert_eq!(effects.received.load(Ordering::SeqCst), 0);
        let running = bridge.clone();
        let caller = tokio::spawn(async move { logout(&running).await });
        count(&effects.received, 1).await;
        if stale {
            bridge.active_generation.store(2, Ordering::Release);
        }
        caller.abort();
        let _cancelled = caller.await;
        count(&effects.completed, 1).await;
        preserved(&effects, &control, revision);
        control.gate.add_permits(1);
        late_close_finished(&control).await;
        preserved(&effects, &control, revision);
        if stale {
            assert!(logout(&bridge).await.is_err());
            preserved(&effects, &control, revision);
            bridge.active_generation.store(1, Ordering::Release);
        }
        logout(&bridge).await.unwrap();
        assert_eq!(effects.deletes.load(Ordering::SeqCst), 1);
        bridge.stop();
        assert_eq!(effects.published_download.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn logout_barrier_uncertain_close_never_deletes_or_reports_success() {
    let (bridge, effects, control) = fixture(true).await;
    let revision = control.credentials.credential_revision();
    assert!(logout(&bridge).await.is_err());
    control.gate.add_permits(1);
    count(&control.authorized_closes, 1).await;
    for _ in 0..2 {
        assert!(logout(&bridge).await.is_err());
    }
    preserved(&effects, &control, revision);
    bridge.stop();
    assert_eq!(effects.published_download.load(Ordering::SeqCst), 0);
}
