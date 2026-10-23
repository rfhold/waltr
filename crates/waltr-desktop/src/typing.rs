//! Bounded live-hypothesis delivery. No clipboard, focus pinning, or retries.
use std::{future::Future, pin::Pin};
use tokio_util::sync::CancellationToken;
use unicode_segmentation::UnicodeSegmentation;

#[cfg(target_os = "macos")]
mod macos;
mod native;
#[cfg(target_os = "linux")]
mod wayland;
pub use native::NativeFactory;

/// Maximum cumulative inserted UTF-8 bytes plus deleted graphemes during one hold.
pub const MAX_TEXT_BYTES: usize = 128 * 1024;
/// Maximum UTF-8 bytes admitted to a backend write; no delivery queue is created.
pub const CHUNK_BYTES: usize = 1024;
/// Local-capable owned async work; implementations must not detach helper tasks.
pub type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
/// Fixed, content-free typing failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No supported or authorized backend exists.
    Unsupported,
    /// Host intent is no longer valid.
    Cancelled,
    /// A forbidden control character was present.
    Control,
    /// Sanitized text or cumulative insertion/deletion work exceeds the fixed bound.
    Limit,
    /// The committed source prefix changed.
    Revision,
    /// Backend startup, delivery, or cleanup failed.
    Backend,
}
/// Synchronous intent fence, passed through to every actual native batch.
pub struct Guard<'a> {
    /// Hold cancellation.
    pub cancel: &'a CancellationToken,
    /// Pinned access/profile generation.
    pub generation: &'a CancellationToken,
    /// Synchronous identity and credential revision validation.
    pub valid: &'a dyn Fn() -> bool,
}
impl std::fmt::Debug for Guard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guard").finish_non_exhaustive()
    }
}
impl Guard<'_> {
    /// Check immediately before every external effect, including after any await.
    /// # Errors
    /// Returns Cancelled if any host fence rejects the effect.
    pub fn check(&self) -> Result<(), Error> {
        if self.cancel.is_cancelled() || self.generation.is_cancelled() || !(self.valid)() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}
/// Creates an inert session synchronously; startup belongs in [`Session::open`].
/// Implementations may own Rc state and must never do blocking device work here.
pub trait Factory {
    /// Create a session without native effects or detached work.
    /// # Errors
    /// Returns a fixed unsupported/backend error.
    fn create(&self) -> Result<Box<dyn Session>, Error>;
}
/// One owned backend lifecycle.
///
/// All methods are cancellation-safe to drop: dropping a
/// pending method stops further effects, and `close` must join all previously started work.
/// No method may log text, retry a write, synthesize Enter/Tab, or use a clipboard.
/// Each actual OS batch MUST recheck `guard` immediately before its effect, even if
/// one write is split internally. Effects target the CURRENT focus, not the press focus.
pub trait Session {
    /// Initialize asynchronously without blocking the UI thread.
    fn open<'a>(&'a mut self, guard: &'a Guard<'_>) -> LocalFuture<'a, Result<(), Error>>;
    /// Deliver one bounded sanitized chunk exactly once; backpressure is awaited.
    fn write<'a>(
        &'a mut self,
        text: &'a str,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>>;
    /// Whether this session can delete previously delivered graphemes.
    fn supports_backspace(&self) -> bool {
        false
    }
    /// Delete a bounded number of graphemes, checking the guard before each native pair.
    /// Unsupported backends must fail without effects, never silently succeed.
    fn backspace<'a>(
        &'a mut self,
        _count: usize,
        _guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async { Err(Error::Unsupported) })
    }
    /// Flush successful delivery without adding any keys.
    fn finish<'a>(&'a mut self, guard: &'a Guard<'_>) -> LocalFuture<'a, Result<(), Error>>;
    /// Unconditionally release resources and join helpers, even after cancelled startup/write.
    /// This cleanup future must be awaited, never cancelled by the host.
    fn close(&mut self) -> LocalFuture<'_, Result<(), Error>>;
}

/// Incremental sanitizer and delivery state. Deliberately has no Debug implementation.
#[derive(Default)]
pub(crate) struct Delivery {
    visible: String,
    total: usize,
    opened: bool,
    poisoned: bool,
}
impl Delivery {
    #[allow(clippy::future_not_send, reason = "backend futures are local-capable")]
    pub(crate) async fn finish(
        &self,
        session: &mut dyn Session,
        guard: &Guard<'_>,
    ) -> Result<(), Error> {
        guard.check()?;
        if self.poisoned {
            return Err(Error::Backend);
        }
        if self.opened {
            session.finish(guard).await?;
        }
        Ok(())
    }
    #[allow(clippy::future_not_send, reason = "backend futures are local-capable")]
    pub(crate) async fn deliver(
        &mut self,
        prefix: &str,
        session: &mut dyn Session,
        guard: &Guard<'_>,
    ) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Backend);
        }
        let mut sanitized = String::new();
        let mut previous_cr = false;
        for ch in prefix.chars() {
            if ch == '\n' && previous_cr {
                previous_cr = false;
                continue;
            }
            previous_cr = ch == '\r';
            match ch {
                '\r' | '\n' | '\t' | '\u{2028}' | '\u{2029}' => sanitized.push(' '),
                ch if ch.is_control() => return Err(Error::Control),
                ch => sanitized.push(ch),
            }
            if sanitized.len() > MAX_TEXT_BYTES {
                return Err(Error::Limit);
            }
        }
        let common: usize = self
            .visible
            .graphemes(true)
            .zip(sanitized.graphemes(true))
            .take_while(|(old, new)| old == new)
            .map(|(same, _)| same.len())
            .sum();
        let deletions = self.visible[common..].graphemes(true).count();
        let replacement = &sanitized[common..];
        let work = replacement.len().saturating_add(deletions);
        if self.total.saturating_add(work) > MAX_TEXT_BYTES {
            return Err(Error::Limit);
        }
        if deletions > 0 && !session.supports_backspace() {
            return Err(Error::Unsupported);
        }
        // Validate the whole hypothesis and work budget before any destructive effect.
        guard.check()?;
        if work > 0 {
            // Any dropped or failed effect leaves unknown target state; never retry it.
            self.poisoned = true;
            if !self.opened {
                session.open(guard).await?;
                self.opened = true;
            }
            if deletions > 0 {
                guard.check()?;
                session.backspace(deletions, guard).await?;
            }
            let mut rest = replacement;
            while !rest.is_empty() {
                let mut end = rest.len().min(CHUNK_BYTES);
                while !rest.is_char_boundary(end) {
                    end -= 1;
                }
                guard.check()?;
                session.write(&rest[..end], guard).await?;
                rest = &rest[end..];
            }
        }
        self.total += work;
        self.visible = sanitized;
        self.poisoned = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        writes: Vec<String>,
        deletions: Vec<usize>,
        unsupported: bool,
        cancel_after_delete: bool,
        revoke: bool,
        revision: Option<std::rc::Rc<std::cell::Cell<bool>>>,
    }
    impl Session for Fake {
        fn open<'a>(&'a mut self, guard: &'a Guard<'_>) -> LocalFuture<'a, Result<(), Error>> {
            Box::pin(async move { guard.check() })
        }
        fn write<'a>(
            &'a mut self,
            text: &'a str,
            guard: &'a Guard<'_>,
        ) -> LocalFuture<'a, Result<(), Error>> {
            Box::pin(async move {
                if self.revoke {
                    guard.cancel.cancel();
                }
                if let Some(revision) = &self.revision {
                    revision.set(false);
                }
                guard.check()?;
                self.writes.push(text.into());
                Ok(())
            })
        }
        fn supports_backspace(&self) -> bool {
            !self.unsupported
        }
        fn backspace<'a>(
            &'a mut self,
            count: usize,
            guard: &'a Guard<'_>,
        ) -> LocalFuture<'a, Result<(), Error>> {
            Box::pin(async move {
                guard.check()?;
                self.deletions.push(count);
                if self.cancel_after_delete {
                    guard.cancel.cancel();
                    guard.check()?;
                }
                Ok(())
            })
        }
        fn finish<'a>(&'a mut self, guard: &'a Guard<'_>) -> LocalFuture<'a, Result<(), Error>> {
            Box::pin(async move { guard.check() })
        }
        fn close(&mut self) -> LocalFuture<'_, Result<(), Error>> {
            Box::pin(async { Ok(()) })
        }
    }
    #[tokio::test]
    async fn suffix_once_crlf_across_commits_and_unicode_chunks() {
        let cancel = CancellationToken::new();
        let guard = Guard {
            cancel: &cancel,
            generation: &cancel,
            valid: &|| true,
        };
        let mut fake = Fake::default();
        let mut delivery = Delivery::default();
        for prefix in ["你\r", "你\r", "你\r\n好\t\u{2028}\u{2029}"] {
            delivery.deliver(prefix, &mut fake, &guard).await.unwrap();
        }
        assert_eq!(fake.writes, ["你 ", "好   "]);
        let prefix = format!("你\r\n好\t\u{2028}\u{2029}{}", "界".repeat(1000));
        delivery.deliver(&prefix, &mut fake, &guard).await.unwrap();
        assert!(fake.writes.iter().all(|s| s.len() <= CHUNK_BYTES));
        delivery
            .deliver("changed", &mut fake, &guard)
            .await
            .unwrap();
        assert_eq!(fake.deletions, [1006]);
    }
    #[tokio::test]
    async fn revisions_use_whole_graphemes_and_deduplicate_normalized_text() {
        let token = CancellationToken::new();
        let guard = Guard {
            cancel: &token,
            generation: &token,
            valid: &|| true,
        };
        let mut fake = Fake::default();
        let mut delivery = Delivery::default();
        for text in ["a👩‍💻e\u{301}\r", "a👩‍💻e\u{301}\r\n", "a👩‍💻e\u{301}\n"]
        {
            delivery.deliver(text, &mut fake, &guard).await.unwrap();
        }
        assert_eq!(fake.writes.len(), 1);
        delivery.deliver("a👩‍💻é", &mut fake, &guard).await.unwrap();
        assert_eq!(fake.deletions, [2]);
        delivery.deliver("a", &mut fake, &guard).await.unwrap();
        assert_eq!(fake.deletions, [2, 2]);
        delivery.deliver("", &mut fake, &guard).await.unwrap();
        delivery.deliver("", &mut fake, &guard).await.unwrap();
        assert_eq!(fake.deletions, [2, 2, 1]);
        assert_eq!(delivery.visible, "");
        delivery.deliver("e", &mut fake, &guard).await.unwrap();
        delivery
            .deliver("e\u{301}", &mut fake, &guard)
            .await
            .unwrap();
        assert_eq!(fake.deletions, [2, 2, 1, 1]);
        assert_eq!(fake.writes.last().unwrap(), "e\u{301}");
    }

    #[tokio::test]
    async fn unsupported_revisions_preflight_and_partial_deletions_never_retry() {
        let token = CancellationToken::new();
        let guard = Guard {
            cancel: &token,
            generation: &token,
            valid: &|| true,
        };
        let mut fake = Fake {
            unsupported: true,
            ..Default::default()
        };
        let mut delivery = Delivery::default();
        delivery.deliver("old", &mut fake, &guard).await.unwrap();
        assert_eq!(
            delivery.deliver("new", &mut fake, &guard).await,
            Err(Error::Unsupported)
        );
        assert!(fake.deletions.is_empty());
        assert_eq!(fake.writes, ["old"]);
        fake.unsupported = false;
        fake.cancel_after_delete = true;
        assert_eq!(
            delivery.deliver("new", &mut fake, &guard).await,
            Err(Error::Cancelled)
        );
        assert_eq!(fake.writes, ["old"]);
        assert_eq!(
            delivery.deliver("new", &mut fake, &guard).await,
            Err(Error::Backend)
        );
        assert_eq!(fake.deletions, [3]);
    }

    #[tokio::test]
    async fn invalid_replacement_and_cumulative_deletion_budget_precede_deletion() {
        let token = CancellationToken::new();
        let guard = Guard {
            cancel: &token,
            generation: &token,
            valid: &|| true,
        };
        let mut fake = Fake::default();
        let mut delivery = Delivery::default();
        delivery.deliver("safe", &mut fake, &guard).await.unwrap();
        for text in ["bad\0".to_owned(), "x".repeat(MAX_TEXT_BYTES + 1)] {
            assert!(delivery.deliver(&text, &mut fake, &guard).await.is_err());
            assert!(fake.deletions.is_empty());
        }
        delivery.total = MAX_TEXT_BYTES;
        assert_eq!(
            delivery.deliver("", &mut fake, &guard).await,
            Err(Error::Limit)
        );
        assert!(fake.deletions.is_empty());
        delivery.total = 4;
        token.cancel();
        assert_eq!(
            delivery.deliver("", &mut fake, &guard).await,
            Err(Error::Cancelled)
        );
        assert!(fake.deletions.is_empty());
    }

    #[tokio::test]
    async fn controls_and_total_bound_reject_before_suffix_effects() {
        let cancel = CancellationToken::new();
        let guard = Guard {
            cancel: &cancel,
            generation: &cancel,
            valid: &|| true,
        };
        for control in [
            '\0', '\u{b}', '\u{c}', '\u{1b}', '\u{7f}', '\u{85}', '\u{9f}',
        ] {
            let mut fake = Fake::default();
            assert_eq!(
                Delivery::default()
                    .deliver(&format!("safe{control}"), &mut fake, &guard)
                    .await,
                Err(Error::Control)
            );
            assert!(fake.writes.is_empty());
        }
        let mut fake = Fake::default();
        let mut delivery = Delivery::default();
        let text = "x".repeat(MAX_TEXT_BYTES);
        delivery.deliver(&text, &mut fake, &guard).await.unwrap();
        assert_eq!(fake.writes.concat(), text);
        let count = fake.writes.len();
        assert_eq!(
            delivery.deliver(&(text + "x"), &mut fake, &guard).await,
            Err(Error::Limit)
        );
        assert_eq!(fake.writes.len(), count);
    }
    #[tokio::test]
    async fn credential_revision_rechecked_inside_backend_before_effect() {
        let token = CancellationToken::new();
        let revision = std::rc::Rc::new(std::cell::Cell::new(true));
        let valid = || revision.get();
        let guard = Guard {
            cancel: &token,
            generation: &token,
            valid: &valid,
        };
        let mut fake = Fake {
            revision: Some(revision.clone()),
            ..Default::default()
        };
        assert_eq!(
            Delivery::default()
                .deliver("never", &mut fake, &guard)
                .await,
            Err(Error::Cancelled)
        );
        assert!(fake.writes.is_empty());
    }

    #[tokio::test]
    async fn backend_rechecks_guard_immediately_before_effect() {
        let cancel = CancellationToken::new();
        let guard = Guard {
            cancel: &cancel,
            generation: &cancel,
            valid: &|| true,
        };
        let mut fake = Fake {
            revoke: true,
            ..Default::default()
        };
        assert_eq!(
            Delivery::default()
                .deliver("never", &mut fake, &guard)
                .await,
            Err(Error::Cancelled)
        );
        assert!(fake.writes.is_empty());
    }
}
