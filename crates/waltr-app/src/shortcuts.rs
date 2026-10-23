//! Process-owned registration; runtime selection only changes the edge destination.
use crate::bridge::RuntimeBridge;
use waltr_desktop::{
    config,
    host::HoldControl,
    shortcut::{self, Router},
};
use std::sync::{Arc, Mutex, Weak};
use tokio_util::sync::CancellationToken;

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
    let mut active: Option<(String, u64, HoldControl, CancellationToken)> = None;
    loop {
        let edge = tokio::select! { biased;
            () = stop.cancelled() => break,
            edge = receiver.next_epoch() => match edge { Ok(edge) => edge, Err(_) => break },
        };
        dispatch_edge(bridge.upgrade().as_deref(), &bindings, &mut active, edge);
    }
    if let Some((_, _, control, token)) = active {
        token.cancel();
        control.cancel();
    }
}

type Active = Option<(String, u64, HoldControl, CancellationToken)>;
pub fn dispatch_edge(
    bridge: Option<&RuntimeBridge>,
    bindings: &[Arc<config::Keybind>],
    active: &mut Active,
    edge: shortcut::EpochEvent,
) {
    let epoch = edge.epoch;
    let edge = edge.event;
    if !edge.pressed {
        if let Some((id, pressed_epoch, control, _token)) = &active
            && id == &edge.id
            && *pressed_epoch == epoch
        {
            control.release();
        }
        return;
    }
    if let (Some(bridge), Some(binding)) = (bridge, bindings.iter().find(|b| b.id == edge.id))
        && let Some((control, token)) = bridge.shortcut_press(epoch, binding.clone())
    {
        *active = Some((edge.id, epoch, control, token));
    }
}
