//! Portal-consented, append-only EIS text delivery. No keyboard/keysym fallback.
//! The UTF-8 interface has no backward-text deletion API; revisions stay unsupported.
use super::{Error, Guard, LocalFuture, Session, native::chunk_end};
use ashpd::desktop::{
    CreateSessionOptions, PersistMode, Session as PortalSession,
    remote_desktop::{
        ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
    },
};
use futures_util::StreamExt;
use reis::{
    ei,
    event::{Connection, Device, DeviceCapability, EiEvent, EiEventConverter},
};
use std::{cell::RefCell, os::unix::net::UnixStream, rc::Rc, time::Duration};
use tokio::{sync::Notify, task::JoinHandle};

const SETUP: Duration = Duration::from_secs(8);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Waiting,
    Ready,
    Dead,
}
impl Phase {
    fn resumed(self) -> Self {
        if self == Self::Dead {
            self
        } else {
            Self::Ready
        }
    }
}
const fn eligible(virtual_device: bool, text_capability: bool, text_interface: bool) -> bool {
    virtual_device && text_capability && text_interface
}
#[derive(Default)]
struct Live {
    phase: Phase,
    device: Option<Device>,
}
impl Live {
    const fn fail(&mut self) {
        self.phase = Phase::Dead;
    }
}
type Creating = LocalFuture<'static, Result<Rc<PortalSession<RemoteDesktop>>, Error>>;
#[derive(Default)]
pub(super) struct NativeSession {
    bus: Option<ashpd::zbus::Connection>,
    portal: Option<Rc<RemoteDesktop>>,
    creating: Option<Creating>,
    session: Option<Rc<PortalSession<RemoteDesktop>>>,
    context: Option<ei::Context>,
    connection: Option<Connection>,
    live: Rc<RefCell<Live>>,
    changed: Rc<Notify>,
    events: Option<JoinHandle<()>>,
    preflight: Option<tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<()>>>,
    closed: Option<JoinHandle<()>>,
    emulating: bool,
    poisoned: bool,
}
#[allow(
    clippy::future_not_send,
    reason = "sessions and guards are LocalSet-owned"
)]
impl NativeSession {
    async fn portal_setup(&mut self, guard: &Guard<'_>) -> Result<(), Error> {
        guard.check()?;
        self.bus = Some(
            ashpd::zbus::Connection::session()
                .await
                .map_err(|_| Error::Unsupported)?,
        );
        guard.check()?;
        let portal = Rc::new(
            RemoteDesktop::with_connection(self.bus.as_ref().ok_or(Error::Backend)?.clone())
                .await
                .map_err(|_| Error::Unsupported)?,
        );
        if portal.version() < 2 {
            return Err(Error::Unsupported);
        }
        self.portal = Some(portal.clone());
        guard.check()?;
        self.creating = Some(Box::pin(async move {
            portal
                .create_session(CreateSessionOptions::default())
                .await
                .map(Rc::new)
                .map_err(|_| Error::Backend)
        }));
        let created = self.creating.as_mut().ok_or(Error::Backend)?.await;
        self.creating = None;
        let session = created?;
        self.session = Some(session.clone());
        guard.check()?;
        let (ready, subscribed) = tokio::sync::oneshot::channel();
        let live = self.live.clone();
        let changed = self.changed.clone();
        self.closed = Some(tokio::task::spawn_local(async move {
            if let Ok(mut signals) = session.receive_closed().await {
                let _ = ready.send(());
                drop(signals.next().await);
            }
            live.borrow_mut().fail();
            changed.notify_one();
        }));
        subscribed.await.map_err(|_| Error::Backend)?;
        guard.check()?;
        let portal = self.portal.as_ref().ok_or(Error::Backend)?;
        let session = self.session.as_ref().ok_or(Error::Backend)?;
        portal
            .select_devices(
                session,
                SelectDevicesOptions::default()
                    .set_devices(ashpd::enumflags2::BitFlags::from(DeviceType::Keyboard))
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .map_err(|_| Error::Backend)?
            .response()
            .map_err(|_| Error::Unsupported)?;
        guard.check()?;
        let granted = portal
            .start(session, None, StartOptions::default())
            .await
            .map_err(|_| Error::Backend)?
            .response()
            .map_err(|_| Error::Unsupported)?;
        if !granted.devices().contains(DeviceType::Keyboard) {
            return Err(Error::Unsupported);
        }
        guard.check()?;
        let fd = portal
            .connect_to_eis(session, ConnectToEISOptions::default())
            .await
            .map_err(|_| Error::Unsupported)?;
        // reis debug mode prints protocol arguments, including committed text.
        if std::env::var_os("REIS_DEBUG").is_some_and(|value| !value.is_empty()) {
            return Err(Error::Unsupported);
        }
        self.context = Some(ei::Context::new(UnixStream::from(fd)).map_err(|_| Error::Backend)?);
        Ok(())
    }
    async fn setup(&mut self, guard: &Guard<'_>) -> Result<(), Error> {
        self.portal_setup(guard).await?;
        guard.check()?;
        let context = self.context.as_ref().ok_or(Error::Backend)?;
        let mut events =
            reis::tokio::EiEventStream::new(context.clone()).map_err(|_| Error::Backend)?;
        let handshake = reis::tokio::ei_handshake(
            &mut events,
            "Waltr committed text",
            ei::handshake::ContextType::Sender,
        )
        .await
        .map_err(|_| Error::Backend)?;
        if !handshake.negotiated_interfaces.contains_key("ei_text") {
            return Err(Error::Unsupported);
        }
        let converter = EiEventConverter::new(context, handshake);
        self.connection = Some(converter.connection().clone());
        self.start_events(events, converter);
        loop {
            guard.check()?;
            match self.live.borrow().phase {
                Phase::Ready => return Ok(()),
                Phase::Dead => return Err(Error::Unsupported),
                Phase::Waiting => {}
            }
            self.changed.notified().await;
        }
    }
    fn start_events(
        &mut self,
        mut events: reis::tokio::EiEventStream,
        mut converter: EiEventConverter,
    ) {
        let connection = converter.connection().clone();
        let live = self.live.clone();
        let changed = self.changed.clone();
        let (requests, mut checks) =
            tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<()>>(1);
        self.preflight = Some(requests);
        self.events = Some(tokio::task::spawn_local(async move {
            loop {
                // Drain already-readable protocol events before acknowledging any next batch.
                let event = tokio::select! { biased;
                    event = events.next() => event,
                    request = checks.recv() => {
                        let Some(reply) = request else { break; };
                        let _ = reply.send(());
                        continue;
                    }
                };
                let Some(Ok(reis::PendingRequestResult::Request(event))) = event else {
                    break;
                };
                if converter.handle_event(event).is_err() {
                    break;
                }
                while let Some(event) = converter.next_event() {
                    let mut state = live.borrow_mut();
                    if state.phase == Phase::Dead {
                        return;
                    }
                    match event {
                        EiEvent::SeatAdded(event) => {
                            event.seat.bind_capabilities(DeviceCapability::Text.into());
                            if connection.flush().is_err() {
                                state.fail();
                            }
                        }
                        EiEvent::DeviceAdded(event) if state.device.is_none() => {
                            if eligible(
                                event.device.device_type() == ei::device::DeviceType::Virtual,
                                event.device.has_capability(DeviceCapability::Text),
                                event.device.interface::<ei::Text>().is_some(),
                            ) {
                                if event.device.device().version() >= 3 {
                                    event.device.device().ready();
                                    if connection.flush().is_err() {
                                        state.fail();
                                    }
                                }
                                state.device = Some(event.device);
                            }
                        }
                        EiEvent::DeviceResumed(event)
                            if state.device.as_ref() == Some(&event.device) =>
                        {
                            state.phase = state.phase.resumed();
                        }
                        EiEvent::DevicePaused(event)
                            if state.device.as_ref() == Some(&event.device) =>
                        {
                            state.fail();
                        }
                        EiEvent::DeviceRemoved(event)
                            if state.device.as_ref() == Some(&event.device) =>
                        {
                            state.fail();
                        }
                        EiEvent::SeatRemoved(event)
                            if state
                                .device
                                .as_ref()
                                .is_some_and(|d| d.seat() == &event.seat) =>
                        {
                            state.fail();
                        }
                        EiEvent::Disconnected(_) => state.fail(),
                        _ => {}
                    }
                    changed.notify_one();
                    if state.phase == Phase::Dead {
                        return;
                    }
                }
            }
            live.borrow_mut().fail();
            changed.notify_one();
        }));
    }
    fn poison(&mut self) {
        self.poisoned = true;
        self.live.borrow_mut().fail();
        if let Some(task) = &self.events {
            task.abort();
        }
    }
}
impl Session for NativeSession {
    fn open<'a>(&'a mut self, guard: &'a Guard<'_>) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            tokio::time::timeout(SETUP, self.setup(guard))
                .await
                .map_err(|_| Error::Unsupported)?
        })
    }
    fn write<'a>(
        &'a mut self,
        text: &'a str,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let mut rest = text;
            while !rest.is_empty() {
                let (reply, ready) = tokio::sync::oneshot::channel();
                let requests = self.preflight.as_ref().ok_or(Error::Backend)?;
                tokio::time::timeout(Duration::from_secs(1), async {
                    requests.send(reply).await.map_err(|_| Error::Backend)?;
                    ready.await.map_err(|_| Error::Backend)
                })
                .await
                .map_err(|_| Error::Backend)??;
                guard.check()?;
                let device = {
                    let live = self.live.borrow();
                    if live.phase != Phase::Ready {
                        return Err(Error::Backend);
                    }
                    live.device.clone().ok_or(Error::Backend)?
                };
                let interface = device.interface::<ei::Text>().ok_or(Error::Unsupported)?;
                let end = chunk_end(rest, 254, false);
                let batch = &rest[..end];
                if batch.contains('\0') {
                    return Err(Error::Control);
                }
                let connection = self.connection.as_ref().ok_or(Error::Backend)?;
                let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
                let micros = u64::try_from(now.tv_sec)
                    .map_err(|_| Error::Backend)?
                    .saturating_mul(1_000_000)
                    + u64::try_from(now.tv_nsec).map_err(|_| Error::Backend)? / 1000;
                guard.check()?;
                if !self.emulating {
                    device.device().start_emulating(connection.serial(), 1);
                    self.emulating = true;
                }
                interface.utf8(batch);
                device.device().frame(connection.serial(), micros);
                // No await between queueing and flush. On failure no helper may flush again.
                if let Err(error) = guard.check() {
                    self.poison();
                    return Err(error);
                }
                if connection.flush().is_err() {
                    self.poison();
                    return Err(Error::Backend);
                }
                rest = &rest[end..];
            }
            Ok(())
        })
    }
    fn finish<'a>(&'a mut self, guard: &'a Guard<'_>) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            guard.check()?;
            if self.live.borrow().phase == Phase::Ready {
                Ok(())
            } else {
                Err(Error::Backend)
            }
        })
    }
    fn close(&mut self) -> LocalFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            for task in [&mut self.events, &mut self.closed] {
                if let Some(task) = task.take() {
                    task.abort();
                    drop(task.await);
                }
            }
            self.preflight = None;
            let mut result = Ok(());
            if !self.poisoned
                && self.emulating
                && self.live.borrow().phase == Phase::Ready
                && let (Some(connection), Some(device)) =
                    (&self.connection, &self.live.borrow().device)
            {
                device.device().stop_emulating(connection.serial());
                if connection.flush().is_err() {
                    result = Err(Error::Backend);
                }
            }
            self.live.borrow_mut().fail();
            self.live.borrow_mut().device = None;
            self.connection = None;
            self.context = None;
            // Retain cancelled CreateSession work until cleanup can acquire and explicitly close it.
            if let Some(creating) = self.creating.take()
                && let Ok(Ok(session)) = tokio::time::timeout(SETUP, creating).await
            {
                self.session = Some(session);
            }
            if let Some(session) = self.session.take()
                && !matches!(
                    tokio::time::timeout(Duration::from_secs(2), session.close()).await,
                    Ok(Ok(()))
                )
            {
                result = Err(Error::Backend);
            }
            self.portal = None;
            if let Some(bus) = self.bus.take() {
                // Dedicated bus disconnect also destroys a portal session whose creation timed out.
                if !matches!(
                    tokio::time::timeout(Duration::from_secs(2), bus.close()).await,
                    Ok(Ok(()))
                ) {
                    result = Err(Error::Backend);
                }
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    #[test]
    fn only_virtual_text_devices_qualify_and_dead_never_resumes() {
        for virtual_device in [false, true] {
            for capability in [false, true] {
                for interface in [false, true] {
                    assert_eq!(
                        eligible(virtual_device, capability, interface),
                        virtual_device && capability && interface
                    );
                }
            }
        }
        assert_eq!(Phase::Waiting.resumed(), Phase::Ready);
        let mut live = Live::default();
        live.phase = live.phase.resumed();
        live.fail();
        assert_eq!(live.phase.resumed(), Phase::Dead);
    }
    #[tokio::test]
    async fn revocation_during_preflight_never_reaches_a_native_batch() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let generation = tokio_util::sync::CancellationToken::new();
        let guard = Guard {
            cancel: &cancel,
            generation: &generation,
            valid: &|| true,
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<()>>(1);
        let mut session = NativeSession {
            preflight: Some(tx),
            ..Default::default()
        };
        let (result, ()) = tokio::join!(session.write("🙂", &guard), async {
            let reply = rx.recv().await.unwrap();
            cancel.cancel();
            reply.send(()).unwrap();
        });
        assert_eq!(result, Err(Error::Cancelled));
        session.close().await.unwrap();
    }
    #[tokio::test]
    async fn close_retains_cancelled_startup_and_joins_owned_monitor() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let release = Rc::new(Notify::new());
                let started = Rc::new(Notify::new());
                let mut session = NativeSession::default();
                let release2 = release.clone();
                let started2 = started.clone();
                session.creating = Some(Box::pin(async move {
                    started2.notify_one();
                    release2.notified().await;
                    Err(Error::Backend)
                }));
                assert!(
                    session
                        .creating
                        .as_mut()
                        .unwrap()
                        .as_mut()
                        .now_or_never()
                        .is_none()
                );
                let (done, joined) = tokio::sync::oneshot::channel::<()>();
                session.closed = Some(tokio::task::spawn_local(async move {
                    let _done = done;
                    std::future::pending::<()>().await;
                }));
                tokio::join!(session.close(), async {
                    started.notified().await;
                    assert!(
                        tokio::time::timeout(Duration::from_secs(1), joined)
                            .await
                            .unwrap()
                            .is_err()
                    );
                    release.notify_one();
                })
                .0
                .unwrap();
                assert!(session.closed.is_none());
                assert!(session.creating.is_none());
                assert_eq!(session.live.borrow().phase, Phase::Dead);
            })
            .await;
    }
}
