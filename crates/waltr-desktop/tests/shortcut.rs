//! Synthetic routing only; constructors never register real OS shortcuts.
use waltr_desktop::shortcut::{Effective, Error, IngressEpoch, Router, portal_trigger};

#[tokio::test]
async fn ingress_stamps_survive_queueing_and_release_keeps_its_press_epoch() {
    let epoch = IngressEpoch::default();
    epoch.bind(10);
    epoch.set_enabled(10, true);
    let (router, mut receiver) = Router::channel_with_epoch(epoch.clone());
    router
        .effective(vec![Effective {
            id: "id".into(),
            trigger: "Space".into(),
        }])
        .unwrap();
    router.edge("s", "s", "id", true);
    epoch.bind(20);
    epoch.set_enabled(10, true); // Obsolete runtime cannot reactivate ingress.
    router.edge("s", "s", "id", false);
    let press = receiver.next_epoch().await.unwrap();
    let release = receiver.next_epoch().await.unwrap();
    assert_eq!(press.epoch, release.epoch);
    assert!(
        epoch
            .admit(press.epoch, || panic!("queued A press replayed"))
            .is_none()
    );
    epoch.set_enabled(20, true);
    router.edge("s", "s", "id", true);
    let fresh = receiver.next_epoch().await.unwrap();
    assert_ne!(fresh.epoch, press.epoch);
    assert_eq!(epoch.admit(fresh.epoch, || 42), Some(42));
    epoch.set_enabled(20, false);
    router.edge("s", "s", "id", false);
    assert_eq!(receiver.next_epoch().await.unwrap().epoch, fresh.epoch);
    router.edge("s", "s", "id", true);
    router.edge("s", "s", "id", false);
    epoch.set_enabled(20, true);
    let denied = receiver.next_epoch().await.unwrap();
    assert!(
        epoch
            .admit(denied.epoch, || panic!("denied press replayed"))
            .is_none()
    );
    assert_eq!(denied.epoch, receiver.next_epoch().await.unwrap().epoch);
}

#[tokio::test]
async fn admission_and_epoch_replacement_have_an_ordered_barrier() {
    let epoch = IngressEpoch::default();
    epoch.bind(1);
    epoch.set_enabled(1, true);
    let (router, mut receiver) = Router::channel_with_epoch(epoch.clone());
    router
        .effective(vec![Effective {
            id: "id".into(),
            trigger: "Space".into(),
        }])
        .unwrap();
    router.edge("s", "s", "id", true);
    router.edge("s", "s", "id", false);
    let stamp = receiver.next_epoch().await.unwrap().epoch;
    receiver.next_epoch().await.unwrap();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release_tx, release) = std::sync::mpsc::channel();
    let admitting = epoch.clone();
    let worker = std::thread::spawn(move || {
        admitting.admit(stamp, || {
            entered_tx.send(()).unwrap();
            release.recv().unwrap();
        })
    });
    entered.recv().unwrap();
    let (attempt_tx, attempt) = std::sync::mpsc::channel();
    let replacing = epoch.clone();
    let replacement = std::thread::spawn(move || {
        attempt_tx.send(()).unwrap();
        replacing.bind(2);
        replacing.set_enabled(2, true);
    });
    attempt.recv().unwrap();
    // Callbacks remain nonblocking while host replacement waits on prior admission.
    router.edge("s", "s", "id", true);
    release_tx.send(()).unwrap();
    assert_eq!(worker.join().unwrap(), Some(()));
    replacement.join().unwrap();
    let queued = receiver.next_epoch().await.unwrap();
    assert_eq!(queued.epoch, stamp);
    assert!(
        epoch
            .admit(queued.epoch, || panic!("barrier edge replayed"))
            .is_none()
    );
}

#[tokio::test]
async fn filters_sessions_ids_repeats_and_tracks_actual_overrides() {
    let (router, mut receiver) = Router::channel();
    router
        .effective(vec![Effective {
            id: "dictate".into(),
            trigger: "User override".into(),
        }])
        .unwrap();
    router.edge("owned", "foreign", "dictate", true);
    router.edge("owned", "owned", "unregistered", true);
    router.edge("owned", "owned", "dictate", true);
    router.edge("owned", "owned", "dictate", true);
    router.edge("owned", "owned", "dictate", false);
    assert!(receiver.next().await.unwrap().pressed);
    assert!(!receiver.next().await.unwrap().pressed);
    assert_eq!(receiver.effective()[0].trigger, "User override");
    router.effective(vec![]).unwrap();
    assert!(receiver.effective().is_empty());
}
#[tokio::test]
async fn full_queue_cancels_generation_before_any_stale_edge() {
    let (router, mut receiver) = Router::channel();
    router
        .effective(vec![Effective {
            id: "dictate".into(),
            trigger: "Space".into(),
        }])
        .unwrap();
    let failure = receiver.failure_token();
    for _index in 0..33 {
        router.edge("s", "s", "dictate", true);
        router.edge("s", "s", "dictate", false);
    }
    assert!(failure.is_cancelled());
    assert_eq!(receiver.next().await, Err(Error::Overflow));
}
#[test]
fn strict_backend_grammars() {
    assert_eq!(portal_trigger("Ctrl+Alt+Space").unwrap(), "CTRL+ALT+space");
    assert_eq!(portal_trigger("Super+PageUp").unwrap(), "LOGO+Prior");
    for bad in [
        "Alt+Ctrl+A",
        "Ctrl+Ctrl+A",
        "Ctrl+MediaPlay",
        "Shift+",
        "F25",
        "",
    ] {
        assert!(portal_trigger(bad).is_err());
    }
    #[cfg(target_os = "macos")]
    for chord in [
        "Ctrl+Alt+Space",
        "Super+Left",
        "Shift+A",
        "F24",
        "1",
        "Backspace",
    ] {
        assert!(
            waltr_desktop::shortcut::macos::hotkey(chord).is_ok(),
            "{chord}"
        );
    }
}
#[test]
fn native_subset_rejected_before_any_factory_access() {
    use waltr_desktop::{
        capture::native_config,
        config::{Audio, DeviceSelector},
    };
    for audio in [
        Audio {
            device: DeviceSelector::Id("stable".into()),
            sample_rate_hz: 24000,
            channels: 1,
        },
        Audio {
            device: DeviceSelector::Default,
            sample_rate_hz: 96000,
            channels: 1,
        },
        Audio {
            device: DeviceSelector::Default,
            sample_rate_hz: 24000,
            channels: 2,
        },
    ] {
        assert!(native_config(&audio).is_err());
    }
}
#[test]
fn native_actual_format_preflight_includes_host_queue_and_inflight_credits() {
    use waltr_desktop::{
        capture::native_config,
        config::{Audio, DeviceSelector},
    };
    let config = native_config(&Audio {
        device: DeviceSelector::Default,
        sample_rate_hz: 24_000,
        channels: 1,
    })
    .unwrap();
    assert_eq!(config.limits.raw_queue_items, 8);
    assert_eq!(
        config
            .callback_queue_items(waltr_audio_io::InputFormat {
                rate: 48_000,
                channels: 1
            })
            .unwrap(),
        3
    );
    assert_eq!(config.limits.preaccept_external_chunks, Some(10));
    let budget = config
        .audio_budget(
            waltr_audio_io::InputFormat {
                rate: 48_000,
                channels: 1,
            },
            10,
        )
        .unwrap();
    assert_eq!(budget.external, 7_500); // 12,000 queue bytes plus two 1,500-byte frames.
    assert_eq!(budget.native_output, 3_750);
    assert_eq!(budget.total, 22_926);
    for (rate, channels) in [(8_000, 1), (48_000, 1), (192_000, 32)] {
        let input = waltr_audio_io::InputFormat { rate, channels };
        let budget = config.audio_budget(input, 10).unwrap();
        let result = waltr_audio_io::Converter::new(config.clone(), input);
        if rate == 8_000 {
            assert!(budget.total > 24_000);
            assert_eq!(
                result.unwrap_err(),
                waltr_audio_io::Error::PreacceptBudgetExceeded
            );
        } else {
            assert!(budget.total <= 24_000);
            assert!(result.is_ok());
        }
    }
}
#[test]
fn backend_failure_and_rebind_while_down_invalidate_work() {
    let (router, receiver) = Router::channel();
    router
        .effective(vec![Effective {
            id: "id".into(),
            trigger: "Space".into(),
        }])
        .unwrap();
    router.edge("s", "s", "id", true);
    assert_eq!(router.effective(vec![]), Err(Error::Backend));
    assert!(receiver.failure_token().is_cancelled());
}
