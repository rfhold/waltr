//! Inert platform selection and bounded Unicode batches.
use super::{Error, Factory, Session};

/// Current-focus external-text backend. Construction never requests OS access.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeFactory;
impl Factory for NativeFactory {
    fn create(&self) -> Result<Box<dyn Session>, Error> {
        #[cfg(target_os = "macos")]
        return Ok(Box::new(super::macos::NativeSession::default()));
        #[cfg(target_os = "linux")]
        return Ok(Box::new(super::wayland::NativeSession::default()));
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        Err(Error::Unsupported)
    }
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
pub(super) fn chunk_end(text: &str, limit: usize, utf16: bool) -> usize {
    let mut used = 0;
    for (offset, ch) in text.char_indices() {
        used += if utf16 { ch.len_utf16() } else { ch.len_utf8() };
        if used > limit {
            return offset;
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_batches_never_split_scalars_or_surrogates() {
        for (limit, utf16) in [(20, true), (254, false)] {
            let text = format!("{}中🙂é", "🙂".repeat(180));
            let mut rest = text.as_str();
            let mut reconstructed = String::new();
            while !rest.is_empty() {
                let end = chunk_end(rest, limit, utf16);
                assert!(end > 0);
                let batch = &rest[..end];
                assert!(
                    if utf16 {
                        batch.encode_utf16().count()
                    } else {
                        batch.len()
                    } <= limit
                );
                reconstructed.push_str(batch);
                rest = &rest[end..];
            }
            assert_eq!(reconstructed, text);
        }
        assert_eq!(chunk_end("1234567890123456789🙂", 20, true), 19);
    }
}
