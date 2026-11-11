//! Shortcut output adapter using the shared retained CPAL owner.
use super::{Error, Guard, LocalFuture, OutputSink, playback};
use playback::EffectFence;
use std::{cell::RefCell, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use waltr_client::{
    RoutineDirection, RoutineMedia,
    interaction::{InteractionEvent, wire},
};

#[derive(Clone)]
pub(crate) struct Fence {
    pub host: Arc<crate::native_audio::Host>,
    pub generation: u64,
    pub generation_cancel: CancellationToken,
    pub cancel: CancellationToken,
    pub revoked: CancellationToken,
    pub credential: Option<(waltr_client::NativeCredentialProvider, u64)>,
}
impl EffectFence for Fence {
    fn valid(&self) -> bool {
        let state = self.host.lock();
        state.generation == self.generation
            && state.allowed
            && !self.generation_cancel.is_cancelled()
            && !self.cancel.is_cancelled()
            && !self.revoked.is_cancelled()
            && self
                .credential
                .as_ref()
                .is_none_or(|(provider, revision)| provider.credential_revision() == *revision)
    }
    fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }
}
pub(crate) struct Sink {
    port: String,
    fence: Fence,
    stream: RefCell<Option<String>>,
    closed: std::cell::Cell<bool>,
    owner: Mutex<Option<playback::Owner>>,
}
impl Sink {
    pub fn new(port: String, fence: Fence) -> Self {
        Self {
            port,
            fence,
            stream: RefCell::new(None),
            closed: std::cell::Cell::new(false),
            owner: Mutex::new(None),
        }
    }
}
impl OutputSink for Sink {
    fn revoke(&self) {
        self.fence.revoked.cancel();
    }
    fn supports(&self, port: &waltr_client::RoutinePort) -> bool {
        port.name == self.port
            && port.direction == RoutineDirection::Output
            && port.media == RoutineMedia::Audio
            && port.required
            && port.formats == ["pcm16le-24000-mono"]
    }
    fn write<'a>(
        &'a self,
        frame: waltr_desktop::media::Frame<'a>,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            guard.check().map_err(|_| Error::Cancelled)?;
            if !self.fence.valid() {
                return Err(Error::Cancelled);
            }
            let waltr_desktop::media::Frame::Stream(event) = frame else {
                return Err(Error::Unsupported);
            };
            match event {
                InteractionEvent::Open(open) if open.port == self.port => {
                    let Some(wire::stream_open::Format::Pcm16le(format)) = open.format else {
                        return Err(Error::Unsupported);
                    };
                    if format.sample_rate != 24000
                        || format.channels != 1
                        || self.stream.borrow().is_some()
                    {
                        return Err(Error::Unsupported);
                    }
                    let mut owner = self.owner.lock().await;
                    guard.check().map_err(|_| Error::Cancelled)?;
                    *owner = Some(playback::Owner::start(format, self.fence.clone())?);
                    *self.stream.borrow_mut() = Some(open.stream_id.clone());
                }
                InteractionEvent::Chunk(chunk)
                    if self.stream.borrow().as_ref() == Some(&chunk.stream_id) =>
                {
                    let Some(wire::stream_chunk::Payload::Audio(bytes)) = &chunk.payload else {
                        return Err(Error::Unsupported);
                    };
                    if self.closed.get() {
                        return Err(Error::Interaction);
                    }
                    let sender = self
                        .owner
                        .lock()
                        .await
                        .as_ref()
                        .ok_or(Error::Interaction)?
                        .sender();
                    playback::send(&sender, bytes.clone(), &self.fence).await?;
                }
                InteractionEvent::Close(close)
                    if self.stream.borrow().as_ref() == Some(&close.stream_id) =>
                {
                    let mut owner = self.owner.lock().await;
                    owner
                        .as_mut()
                        .ok_or(Error::Interaction)?
                        .finish(guard.check().is_ok() && self.fence.valid())
                        .await?;
                    owner.take();
                    self.closed.set(true);
                }
                _ => {}
            }
            Ok(())
        })
    }
    fn finish<'a>(
        &'a self,
        success: bool,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            if !success || guard.check().is_err() {
                self.revoke();
            }
            let mut owner = self.owner.lock().await;
            if let Some(owner) = owner.as_mut() {
                owner.finish(false).await?;
            }
            owner.take();
            if success
                && ((self.stream.borrow().is_some() && !self.closed.get())
                    || guard.check().is_err()
                    || !self.fence.valid())
            {
                return Err(Error::Interaction);
            }
            Ok(())
        })
    }
}
