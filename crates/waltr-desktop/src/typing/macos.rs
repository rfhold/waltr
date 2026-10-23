//! Best-effort Unicode `CGEvents`; AX access is checked without prompting.
use super::{Error, Guard, LocalFuture, Session, native::chunk_end};
use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventTapLocation},
    event_source::{CGEventSource, CGEventSourceStateID},
};
use macos_accessibility_client::accessibility::application_is_trusted;

#[derive(Default)]
pub(super) struct NativeSession {
    source: Option<CGEventSource>,
}
impl Session for NativeSession {
    fn open<'a>(&'a mut self, guard: &'a Guard<'_>) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            guard.check()?;
            if !application_is_trusted() {
                return Err(Error::Unsupported);
            }
            self.source = Some(
                CGEventSource::new(CGEventSourceStateID::Private).map_err(|()| Error::Backend)?,
            );
            Ok(())
        })
    }
    fn write<'a>(
        &'a mut self,
        text: &'a str,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let source = self.source.as_ref().ok_or(Error::Backend)?;
            let mut rest = text;
            while !rest.is_empty() {
                tokio::task::yield_now().await;
                let end = chunk_end(rest, 20, true);
                let batch = &rest[..end];
                let down = CGEvent::new_keyboard_event(source.clone(), 0, true)
                    .map_err(|()| Error::Backend)?;
                let up = CGEvent::new_keyboard_event(source.clone(), 0, false)
                    .map_err(|()| Error::Backend)?;
                for event in [&down, &up] {
                    event.set_flags(CGEventFlags::empty());
                    event.set_string(batch);
                }
                guard.check()?;
                if !application_is_trusted() {
                    return Err(Error::Unsupported);
                }
                guard.check()?;
                // The pair is synchronous: cancellation cannot strand our own down event.
                down.post(CGEventTapLocation::HID);
                up.post(CGEventTapLocation::HID);
                rest = &rest[end..];
            }
            Ok(())
        })
    }
    fn supports_backspace(&self) -> bool {
        true
    }
    fn backspace<'a>(
        &'a mut self,
        count: usize,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            if count > super::MAX_TEXT_BYTES {
                return Err(Error::Limit);
            }
            let source = self.source.as_ref().ok_or(Error::Backend)?;
            for _ in 0..count {
                tokio::task::yield_now().await;
                let down = CGEvent::new_keyboard_event(source.clone(), 51, true)
                    .map_err(|()| Error::Backend)?;
                let up = CGEvent::new_keyboard_event(source.clone(), 51, false)
                    .map_err(|()| Error::Backend)?;
                down.set_flags(CGEventFlags::empty());
                up.set_flags(CGEventFlags::empty());
                guard.check()?;
                if !application_is_trusted() {
                    return Err(Error::Unsupported);
                }
                guard.check()?;
                down.post(CGEventTapLocation::HID);
                up.post(CGEventTapLocation::HID);
            }
            Ok(())
        })
    }
    fn finish<'a>(&'a mut self, guard: &'a Guard<'_>) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move { guard.check() })
    }
    fn close(&mut self) -> LocalFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.source = None;
            Ok(())
        })
    }
}
