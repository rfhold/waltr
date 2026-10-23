//! Startup-only XDG configuration, native shortcuts, and owned hold-to-Routine commands.
//! No UI framework, credential store, server discovery, or global async runtime is required.
//! The host pins a credential-aware transport and cancels its generation before profile/access changes.
//!
//! ```no_run
//! use std::{rc::Rc, sync::Arc};
//! use waltr_client::RoutineInteractionTransport;
//! use waltr_desktop::{actor::Actor, capture::Native, config, host::AudioAdmission, shortcut::Router, typing::NativeFactory};
//! use tokio_util::sync::CancellationToken;
//!
//! # async fn example<T: RoutineInteractionTransport>(transport: T, valid: Rc<dyn Fn() -> bool>) {
//! let config = config::load_ambient().expect("valid startup config");
//! let bindings: Vec<_> = config.keybinds.iter().map(|b| (b.id.clone(), b.key.clone())).collect();
//! let (router, mut events) = Router::channel();
//! // valid synchronously checks the pinned profile, access, and credential revision.
//! let (actor, status) = Actor::with_effects(config, transport, Arc::new(Native),
//!     AudioAdmission::default(), Rc::new(NativeFactory), valid).unwrap();
//! let generation = CancellationToken::new();
//! // macOS: construct Registration on the main thread and retain it through the runloop.
//! # #[cfg(target_os = "macos")]
//! let _registration = waltr_desktop::shortcut::macos::Registration::new(&bindings, router.clone()).unwrap();
//! // Drive actor.run(&mut events, &generation) as an owned local future alongside the UI loop.
//! // Wayland: also drive shortcut::wayland::run(&bindings, router, &backend_stop).
//! // On disposal: cancel generation, await actor, cancel/await backend, then drop registration.
//! let owned = actor.run(&mut events, &generation);
//! generation.cancel();
//! let _terminal = owned.await;
//! # drop((status, events, router, bindings));
//! # }
//! ```
//!
//! Do not drop active actor/hold futures as shutdown. Cancel and await them to acknowledge native
//! startup/teardown joins and command kill-and-wait. OS device calls have no portable hard deadline.
//! Local command side effects cannot be undone. No operation automatically retries.

pub mod actor;
mod bounded;
pub mod capture;
pub mod command;
pub mod config;
pub mod host;
pub mod interaction;
pub mod shortcut;
pub mod typing;
