//! Process-owned registration; runtime selection only changes the edge destination.
use crate::bridge::RuntimeBridge;
use std::sync::{Arc, Mutex, Weak};
use tokio_util::sync::CancellationToken;
use waltr_desktop::{
    config,
    host::HoldControl,
    shortcut::{self, Router},
};

#[cfg(target_os = "macos")]
thread_local! {
    // Tauri setup and Exit run on the main runloop; never send this registrar to a worker.
    static REGISTRATION: std::cell::RefCell<Option<shortcut::macos::Registration>> = const { std::cell::RefCell::new(None) };
}
pub struct Owner {
    stop: CancellationToken,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}
impl Owner {
    pub fn install(bridge: &Arc<RuntimeBridge>) -> Result<Self, String> {
        let stop = CancellationToken::new();
        #[cfg(target_os = "android")]
        let config = config::Config::default();
        #[cfg(not(target_os = "android"))]
        let config = config::load_ambient().map_err(|_| "Invalid desktop client configuration")?;
        if config.keybinds.is_empty() {
            return Ok(Self {
                stop,
                worker: Mutex::new(None),
            });
        }
        let bindings: Vec<_> = config
            .keybinds
            .iter()
            .map(|b| (b.id.clone(), b.key.clone()))
            .collect();
        let (router, receiver) = Router::channel_with_epoch(bridge.shortcut_ingress());
        #[cfg(target_os = "macos")]
        {
            let registration = shortcut::macos::Registration::new(&bindings, router.clone())
                .map_err(|_| "Desktop shortcut registration failed")?;
            REGISTRATION.with(|slot| *slot.borrow_mut() = Some(registration));
        }
        let weak = Arc::downgrade(bridge);
        let stopping = stop.clone();
        let worker = std::thread::Builder::new()
            .name("waltr-shortcuts".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    router.fail(shortcut::Error::Backend);
                    return;
                };
                runtime.block_on(async move {
                    let edges = route(
                        receiver,
                        config.keybinds.into_iter().map(Arc::new).collect(),
                        weak,
                        &stopping,
                    );
                    #[cfg(target_os = "linux")]
                    {
                        let backend = shortcut::wayland::run(&bindings, router, &stopping);
                        tokio::pin!(backend, edges);
                        tokio::select! {
                            result = &mut backend => {
                        if result.is_err() { crate::observability::lifecycle_event("desktop.shortcuts", "backend", "unavailable"); }
                        stopping.cancel(); edges.await;
                    }
                            () = &mut edges => { stopping.cancel(); let _result = backend.await; }
                        }
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        drop((bindings, router));
                        edges.await;
                    }
                });
            })
            .map_err(|_| {
                #[cfg(target_os = "macos")]
                REGISTRATION.with(|slot| drop(slot.borrow_mut().take()));
                "Desktop shortcut worker could not start"
            })?;
        Ok(Self {
            stop,
            worker: Mutex::new(Some(worker)),
        })
    }
    pub fn shutdown(&self, bridge: &RuntimeBridge) {
        self.stop.cancel();
        #[cfg(target_os = "macos")]
        REGISTRATION.with(|slot| drop(slot.borrow_mut().take()));
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(worker) = worker {
            bridge.retire_shortcut_backend(worker);
        }
    }
}
async fn route(
    mut receiver: shortcut::Receiver,
    bindings: Vec<Arc<config::Keybind>>,
    bridge: Weak<RuntimeBridge>,
    stop: &CancellationToken,
) {
    let mut state = RouteState::default();
    loop {
        let edge = tokio::select! { biased;
            () = stop.cancelled() => break,
            edge = receiver.next_stamped_epoch() => match edge { Ok(edge) => edge, Err(_) => break },
        };
        state.dispatch(bridge.upgrade().as_deref(), &bindings, edge);
    }
    if let Some((_, _, control, token)) = state.active {
        token.cancel();
        control.cancel();
    }
}

type Active = Option<(String, u64, HoldControl, CancellationToken)>;

#[derive(Default)]
struct RouteState {
    active: Active,
    one_shot: Option<(config::Mode, shortcut::LifetimeStamp)>,
    discard_through: Option<shortcut::LifetimeStamp>,
}
impl RouteState {
    fn dispatch(
        &mut self,
        bridge: Option<&RuntimeBridge>,
        bindings: &[Arc<config::Keybind>],
        (edge, arrived_at): (shortcut::EpochEvent, shortcut::LifetimeStamp),
    ) {
        if edge.event.pressed {
            if let Some((mode, opened_by)) = self.one_shot
                && let Some((id, epoch, control, token)) = &self.active
            {
                let Some(released) = control.admission_released_at() else {
                    if mode == config::Mode::Toggle
                        && *id == edge.event.id
                        && *epoch == edge.epoch
                        && arrived_at > opened_by
                        && let Some(bridge) = bridge
                    {
                        let _ = bridge.shortcut_ingress().admit(*epoch, || {
                            !token.is_cancelled() && control.finish_toggle_capture()
                        });
                    }
                    return;
                };
                // Keep the cutoff even when another binding replaces the owner.
                self.discard_through = self.discard_through.max(Some(released));
            }
            if self
                .discard_through
                .is_some_and(|cutoff| arrived_at <= cutoff)
            {
                return;
            }
        }
        if let Some(mode) = route_edge(bridge, bindings, &mut self.active, edge) {
            self.one_shot = (mode != config::Mode::Hold).then_some((mode, arrived_at));
        }
    }
}

#[cfg(test)]
pub fn dispatch_edge(
    bridge: Option<&RuntimeBridge>,
    bindings: &[Arc<config::Keybind>],
    active: &mut Active,
    edge: shortcut::EpochEvent,
) {
    let _ = route_edge(bridge, bindings, active, edge);
}

fn route_edge(
    bridge: Option<&RuntimeBridge>,
    bindings: &[Arc<config::Keybind>],
    active: &mut Active,
    edge: shortcut::EpochEvent,
) -> Option<config::Mode> {
    let epoch = edge.epoch;
    let edge = edge.event;
    if !edge.pressed {
        if let Some((id, pressed_epoch, control, _)) = &active
            && id == &edge.id
            && *pressed_epoch == epoch
            && bindings
                .iter()
                .any(|b| b.id == edge.id && b.mode == config::Mode::Hold)
        {
            control.release();
        }
        return None;
    }
    if let Some((id, pressed_epoch, control, token)) = active.as_mut()
        && id == &edge.id
        && *pressed_epoch == epoch
        && !token.is_cancelled()
        && !control.is_finished()
    {
        if control.repeats_enabled()
            && let Some(next) =
                bridge.and_then(|bridge| bridge.shortcut_repeat(epoch, control, token))
        {
            *control = next;
        }
        // A live invocation that sealed admission must never turn a late press into a new RPC.
        return None;
    }
    if let (Some(bridge), Some(binding)) = (bridge, bindings.iter().find(|b| b.id == edge.id))
        && let Some((control, token)) = bridge.shortcut_press(epoch, binding.clone())
    {
        *active = Some((edge.id, epoch, control, token));
        return Some(binding.mode);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bridge::RuntimeBridge, server_controller::EmbeddedServerController};
    use shortcut::{Effective, EpochEvent, Event};

    #[tokio::test]
    async fn presses_queued_during_press_to_talk_do_not_replay_after_cleanup() {
        one_shot_busy_presses_do_not_replay("press-to-talk").await;
    }

    #[tokio::test]
    async fn toggle_routes_one_matching_stop_and_discards_busy_taps() {
        one_shot_busy_presses_do_not_replay("toggle").await;
    }

    async fn one_shot_busy_presses_do_not_replay(mode: &str) {
        let bridge = RuntimeBridge::new(EmbeddedServerController::default());
        let mut receive = bridge.synthetic_shortcut_generation(1);
        let mut snapshot = crate::native_media_test_support::snapshot("test");
        let output = &mut snapshot.routines.definitions.definitions[0].ports[1];
        output.media = waltr_client::RoutineMedia::Audio;
        output.formats = vec!["pcm16le-24000-mono".into()];
        bridge.synthetic_shortcut_catalog(&snapshot);
        let yaml = r#"version: 1
keybinds:
 - id: talk
   key: Ctrl+Alt+Space
   mode: press-to-talk
   routine: {name: test, trigger: dictate, input: audio, output: clean-text}
   audio: {device: default, sampleRateHz: 24000, channels: 1}
   playback: {}
 - id: other
   key: Ctrl+Alt+A
   mode: hold
   routine: {name: test, trigger: dictate, input: audio, output: clean-text}
   audio: {device: default, sampleRateHz: 24000, channels: 1}
   playback: {}
"#;
        let config = config::parse(yaml.replace("press-to-talk", mode).as_bytes()).unwrap();
        let bindings: Vec<_> = config.keybinds.into_iter().map(Arc::new).collect();
        let (router, mut receiver) = Router::channel_with_epoch(bridge.shortcut_ingress());
        router
            .effective(
                bindings
                    .iter()
                    .map(|b| Effective {
                        id: b.id.clone(),
                        trigger: b.key.clone(),
                    })
                    .collect(),
            )
            .unwrap();
        let mut route = RouteState::default();
        router.edge("s", "s", "talk", true);
        route.dispatch(
            Some(&bridge),
            &bindings,
            receiver.next_stamped_epoch().await.unwrap(),
        );
        let held = receive().expect("initial press owns admission");
        let epoch = route.active.as_ref().unwrap().1;
        if mode == "toggle" {
            let control = route.active.as_ref().unwrap().2.clone();
            assert!(!control.repeats_enabled());
            for (id, pressed) in [
                ("talk", false),
                ("other", true),
                ("other", false),
                ("talk", true),
            ] {
                router.edge("s", "s", id, pressed);
                let edge = receiver.next_stamped_epoch().await.unwrap();
                if id == "talk" && pressed {
                    let mut wrong = edge;
                    wrong.0.epoch += 2;
                    route.dispatch(Some(&bridge), &bindings, wrong);
                } else {
                    route.dispatch(Some(&bridge), &bindings, edge);
                }
            }
            assert!(
                control.finish_toggle_capture(),
                "key-up, other binding, and wrong epoch must leave the stop latch open"
            );
            assert!(receive().is_none());
        }
        for (id, pressed) in [
            ("talk", false),
            ("talk", true),
            ("talk", false),
            ("other", true),
            ("other", false),
        ] {
            router.edge("s", "s", id, pressed);
        }
        drop(held); // Real permit teardown, but no device work.
        let mut delayed = None;
        for _ in 0..5 {
            let edge = receiver.next_stamped_epoch().await.unwrap();
            assert_eq!(edge.0.epoch, epoch, "no access or generation change");
            if edge.0.event.pressed && edge.0.event.id == "other" {
                delayed = Some(edge.clone());
            }
            route.dispatch(Some(&bridge), &bindings, edge);
            assert!(receive().is_none(), "a busy tap must not replay");
        }
        router.edge("s", "s", "other", true);
        route.dispatch(
            Some(&bridge),
            &bindings,
            receiver.next_stamped_epoch().await.unwrap(),
        );
        let fresh = receive().expect("fresh press after cleanup starts normally");
        assert_eq!(route.active.as_ref().unwrap().0, "other");
        drop(fresh);
        // Also exercise a stale arrival dispatched after the speech owner has been replaced.
        route.dispatch(Some(&bridge), &bindings, delayed.unwrap());
        assert!(
            receive().is_none(),
            "replacement must not forget the cutoff"
        );
        router.edge("s", "s", "talk", true);
        route.dispatch(
            Some(&bridge),
            &bindings,
            receiver.next_stamped_epoch().await.unwrap(),
        );
        let fresh = receive().expect("fresh one-shot tap can also start again");
        if mode == "toggle" {
            let control = route.active.as_ref().unwrap().2.clone();
            for pressed in [false, true] {
                router.edge("s", "s", "talk", pressed);
                route.dispatch(
                    Some(&bridge),
                    &bindings,
                    receiver.next_stamped_epoch().await.unwrap(),
                );
            }
            assert!(
                !control.finish_toggle_capture(),
                "matching tap latched stop"
            );
            assert!(receive().is_none(), "stop did not enqueue another capture");
        }
        drop(fresh);
        bridge.stop();
    }

    #[tokio::test]
    async fn exhausted_repeated_capture_surfaces_audio_failure_without_replacing_owner() {
        for case in ["undismissed", "dismissed", "terminal-failure"] {
            let bridge = RuntimeBridge::new(EmbeddedServerController::default());
            let mut receive = bridge.synthetic_shortcut_generation(1);
            let mut snapshot = crate::native_media_test_support::snapshot("test");
            let definition = &mut snapshot.routines.definitions.definitions[0];
            definition.capabilities.push("media.inputs".into());
            definition.ports[1].media = waltr_client::RoutineMedia::Audio;
            definition.ports[1].formats = vec!["pcm16le-24000-mono".into()];
            bridge.synthetic_shortcut_catalog(&snapshot);
            let config = config::parse(
                br#"version: 1
keybinds:
 - id: test
   key: Ctrl+Alt+Space
   mode: hold
   routine: {name: test, trigger: dictate, input: audio, output: clean-text}
   audio: {device: default, sampleRateHz: 24000, channels: 1}
   playback: {}
"#,
            )
            .unwrap();
            let bindings: Vec<_> = config.keybinds.into_iter().map(Arc::new).collect();
            let (router, mut edges) = Router::channel_with_epoch(bridge.shortcut_ingress());
            router
                .effective(vec![Effective {
                    id: "test".into(),
                    trigger: "Space".into(),
                }])
                .unwrap();
            router.edge("s", "s", "test", true);
            let edge = edges.next_epoch().await.unwrap();
            let epoch = edge.epoch;
            let mut active = None;
            dispatch_edge(Some(&bridge), &bindings, &mut active, edge);
            let held = receive().unwrap();
            let failures = bridge.synthetic_shortcut_failures();
            for _ in 1..16 {
                router.edge("s", "s", "test", false);
                dispatch_edge(
                    Some(&bridge),
                    &bindings,
                    &mut active,
                    edges.next_epoch().await.unwrap(),
                );
                router.edge("s", "s", "test", true);
                dispatch_edge(
                    Some(&bridge),
                    &bindings,
                    &mut active,
                    edges.next_epoch().await.unwrap(),
                );
                assert!(failures.borrow().is_none());
            }
            router.edge("s", "s", "test", false);
            dispatch_edge(
                Some(&bridge),
                &bindings,
                &mut active,
                edges.next_epoch().await.unwrap(),
            );
            router.edge("s", "s", "test", true);
            dispatch_edge(
                Some(&bridge),
                &bindings,
                &mut active,
                edges.next_epoch().await.unwrap(),
            );
            assert_eq!(failures.borrow().unwrap().category, "audio_failed");
            assert!(failures.borrow().unwrap().diagnostic_id > 0);
            assert!(
                receive().is_none(),
                "rejection cannot enqueue a replacement interaction"
            );
            assert_eq!(active.as_ref().unwrap().1, epoch);
            assert!(
                bridge.shortcut_press(epoch, bindings[0].clone()).is_none(),
                "old owner retains admission after rejection"
            );
            let rejected = *failures.borrow();
            if case != "undismissed" {
                bridge
                    .dismiss_audio_failure(1, rejected.unwrap().diagnostic_id)
                    .unwrap();
                assert!(failures.borrow().is_none());
            }
            bridge.synthetic_shortcut_complete(if case == "terminal-failure" {
                Err(waltr_desktop::host::Error::Interaction)
            } else {
                Ok(())
            });
            match case {
                "undismissed" => assert_eq!(
                    *failures.borrow(),
                    rejected,
                    "terminal success preserves an undismissed rejection"
                ),
                "dismissed" => assert!(
                    failures.borrow().is_none(),
                    "terminal success cannot republish a dismissed rejection"
                ),
                _ => {
                    let terminal = failures.borrow().unwrap();
                    assert_eq!(terminal.category, "audio_failed");
                    assert_ne!(
                        terminal.diagnostic_id,
                        rejected.unwrap().diagnostic_id,
                        "real terminal failure has its own diagnostic identity"
                    );
                }
            }
            drop(held);
            bridge.stop();
        }
    }

    #[tokio::test]
    async fn shortcut_routes_only_matching_binding_epoch_and_mode() {
        for (opt_in, press_to_talk) in [(false, false), (true, false), (true, true)] {
            let bridge = RuntimeBridge::new(EmbeddedServerController::default());
            let mut receive = bridge.synthetic_shortcut_generation(1);
            let mut snapshot = crate::native_media_test_support::snapshot("test");
            let output = &mut snapshot.routines.definitions.definitions[0].ports[1];
            output.media = waltr_client::RoutineMedia::Audio;
            output.formats = vec!["pcm16le-24000-mono".into()];
            if opt_in {
                snapshot.routines.definitions.definitions[0]
                    .capabilities
                    .push("media.inputs".into());
            } else {
                snapshot.routines.definitions.definitions[0]
                    .capabilities
                    .push("media.inputs.extra".into());
            }
            bridge.synthetic_shortcut_catalog(&snapshot);
            let mut config = config::parse(
                br#"version: 1
keybinds:
 - id: test
   key: Ctrl+Alt+Space
   mode: hold
   routine: {name: test, trigger: dictate, input: audio, output: clean-text}
   audio: {device: default, sampleRateHz: 24000, channels: 1}
   playback: {}
 - id: other
   key: Ctrl+Alt+A
   mode: hold
   routine: {name: test, trigger: dictate, input: audio, output: clean-text}
   audio: {device: default, sampleRateHz: 24000, channels: 1}
   playback: {}
"#,
            )
            .unwrap();
            if press_to_talk {
                config.keybinds[0].mode = config::Mode::PressToTalk;
            }
            let bindings: Vec<_> = config.keybinds.drain(..).map(Arc::new).collect();
            let (router, mut receiver) = Router::channel_with_epoch(bridge.shortcut_ingress());
            router
                .effective(vec![
                    Effective {
                        id: "test".into(),
                        trigger: "Space".into(),
                    },
                    Effective {
                        id: "other".into(),
                        trigger: "A".into(),
                    },
                ])
                .unwrap();
            let mut active = None;
            router.edge("s", "s", "test", true);
            let first = receiver.next_epoch().await.unwrap();
            let epoch = first.epoch;
            dispatch_edge(Some(&bridge), &bindings, &mut active, first);
            let held = receive().expect("initial press enqueues one retained operation");
            assert_eq!(
                active.as_ref().unwrap().2.repeats_enabled(),
                opt_in && !press_to_talk
            );
            router.edge("s", "s", "test", false);
            dispatch_edge(
                Some(&bridge),
                &bindings,
                &mut active,
                receiver.next_epoch().await.unwrap(),
            );
            router.edge("s", "s", "test", true);
            dispatch_edge(
                Some(&bridge),
                &bindings,
                &mut active,
                receiver.next_epoch().await.unwrap(),
            );
            assert!(
                receive().is_none(),
                "repeat must not enqueue another RPC owner"
            );
            let control = active.as_ref().unwrap().2.clone();
            assert!(
                control.press_again().is_err(),
                "new capture remains held, or one-shot disallows repeats"
            );
            dispatch_edge(
                Some(&bridge),
                &bindings,
                &mut active,
                EpochEvent {
                    epoch: epoch + 2,
                    event: Event {
                        id: "test".into(),
                        pressed: false,
                    },
                },
            );
            assert!(
                control.press_again().is_err(),
                "wrong epoch key-up cannot release current capture"
            );
            router.edge("s", "s", "other", true);
            dispatch_edge(
                Some(&bridge),
                &bindings,
                &mut active,
                receiver.next_epoch().await.unwrap(),
            );
            assert!(receive().is_none());
            assert_eq!(active.as_ref().unwrap().0, "test");
            router.edge("s", "s", "test", false);
            dispatch_edge(
                Some(&bridge),
                &bindings,
                &mut active,
                receiver.next_epoch().await.unwrap(),
            );
            assert_eq!(
                control.press_again().is_ok(),
                opt_in && !press_to_talk,
                "only hold with the pinned exact capability enables repeated capture admission"
            );
            bridge.synthetic_shortcut_access(1, false);
            assert!(
                bridge
                    .shortcut_repeat(epoch, &control, &active.as_ref().unwrap().3)
                    .is_none()
            );
            bridge.synthetic_shortcut_access(1, true);
            assert!(
                bridge
                    .shortcut_repeat(epoch, &control, &active.as_ref().unwrap().3)
                    .is_none(),
                "renewed access cannot replay an old epoch"
            );
            router.edge("s", "s", "test", true);
            dispatch_edge(
                Some(&bridge),
                &bindings,
                &mut active,
                receiver.next_epoch().await.unwrap(),
            );
            assert!(
                receive().is_none(),
                "revocation retains admission until old owner cleanup"
            );
            router.edge("s", "s", "test", false);
            dispatch_edge(
                Some(&bridge),
                &bindings,
                &mut active,
                receiver.next_epoch().await.unwrap(),
            );
            drop(held); // Simulate terminal owner cleanup without touching native devices.
            router.edge("s", "s", "test", true);
            dispatch_edge(
                Some(&bridge),
                &bindings,
                &mut active,
                receiver.next_epoch().await.unwrap(),
            );
            let next = receive().expect("press after old owner cleanup starts normally");
            drop(next);
            bridge.stop();
        }
    }
}
