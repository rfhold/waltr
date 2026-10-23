//! Main-runloop Carbon registration, macOS 11+. No media keys or accessibility fallback.
//! This crate must own global-hotkey's process-wide event handler before any other user.
use super::{Effective, Error, Router};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState, hotkey::HotKey};
use std::{
    collections::BTreeMap,
    marker::PhantomData,
    rc::Rc,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
static RESERVED: AtomicBool = AtomicBool::new(false);
static DISPATCH: OnceLock<(Router, BTreeMap<u32, String>)> = OnceLock::new();

/// Main-thread-only registrar; retain it while the main runloop executes.
/// Drop unregisters all keys on that same thread and invalidates its generation.
pub struct Registration {
    manager: GlobalHotKeyManager,
    keys: Vec<HotKey>,
    active: bool,
    _local: PhantomData<Rc<()>>,
}
impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("keys", &self.keys.len())
            .finish_non_exhaustive()
    }
}
/// Normalize strict portable chords to global-hotkey's physical Code syntax.
/// # Errors
/// Rejects invalid chords rather than accepting backend parser extensions.
pub fn hotkey(key: &str) -> Result<HotKey, Error> {
    crate::config::validate_chord(key).map_err(|_| Error::Invalid)?;
    let parts: Vec<_> = key.split('+').collect();
    let mut out = parts[..parts.len() - 1].join("+");
    if !out.is_empty() {
        out.push('+');
    }
    let key = parts[parts.len() - 1];
    if key.len() == 1 {
        if key.as_bytes()[0].is_ascii_alphabetic() {
            out.push_str("Key");
            out.push_str(&key.to_ascii_uppercase());
        } else {
            out.push_str("Digit");
            out.push_str(key);
        }
    } else {
        out.push_str(match key {
            "Left" => "ArrowLeft",
            "Right" => "ArrowRight",
            "Up" => "ArrowUp",
            "Down" => "ArrowDown",
            other => other,
        });
    }
    out.parse().map_err(|_| Error::Invalid)
}
impl Registration {
    /// Register once per process on the main thread. No callback ever uses the library's unbounded receiver.
    /// # Errors
    /// Returns explicit main-thread, dispatcher-ownership, parser, or native registration errors.
    pub fn new(bindings: &[(String, String)], router: Router) -> Result<Self, Error> {
        let _main = objc2::MainThreadMarker::new().ok_or(Error::MainThread)?;
        if bindings.len() > 64 {
            return Err(Error::Invalid);
        }
        if RESERVED.swap(true, Ordering::AcqRel) {
            return Err(Error::Busy);
        }
        GlobalHotKeyEvent::set_event_handler(Some(|event: GlobalHotKeyEvent| {
            if let Some((router, ids)) = DISPATCH.get()
                && let Some(id) = ids.get(&event.id)
            {
                router.edge("mac", "mac", id, event.state == HotKeyState::Pressed);
            }
        }));
        let manager = GlobalHotKeyManager::new().map_err(|_| Error::Backend)?;
        let mut registration = Self {
            manager,
            keys: Vec::new(),
            active: false,
            _local: PhantomData,
        };
        let mut ids = BTreeMap::new();
        for (id, chord) in bindings {
            let key = hotkey(chord)?;
            if ids.insert(key.id(), id.clone()).is_some() {
                return Err(Error::Invalid);
            }
            registration
                .manager
                .register(key)
                .map_err(|_| Error::Backend)?;
            registration.keys.push(key);
        }
        router.effective(
            bindings
                .iter()
                .map(|(id, key)| Effective {
                    id: id.clone(),
                    trigger: key.clone(),
                })
                .collect(),
        )?;
        DISPATCH.set((router, ids)).map_err(|_| Error::Busy)?;
        registration.active = true;
        Ok(registration)
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        let _main =
            objc2::MainThreadMarker::new().expect("registration must drop on the main thread");
        let _result = self.manager.unregister_all(&self.keys);
        if !self.active {
            return;
        }
        if let Some((router, _ids)) = DISPATCH.get() {
            router.fail(Error::Backend);
        }
    }
}
