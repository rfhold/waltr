//! Audio metadata and validation, without conversion or device access.
use crate::error::Error;

/// Headerless signed 16-bit little-endian interleaved PCM format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmS16Le {
    rate: i32,
    channels: i32,
}
impl PcmS16Le {
    /// Validate positive sample rate and channel count.
    pub const fn new(rate: i32, channels: i32) -> Result<Self, Error> {
        if rate <= 0 || channels <= 0 {
            return Err(Error::Invalid("PCM rate and channels must be positive"));
        }
        Ok(Self { rate, channels })
    }
    /// Sample rate in Hz.
    #[must_use]
    pub const fn rate(self) -> i32 {
        self.rate
    }
    /// Interleaved channel count.
    #[must_use]
    pub const fn channels(self) -> i32 {
        self.channels
    }
    /// Require whole interleaved frames, including at each streaming chunk boundary.
    #[allow(clippy::cast_sign_loss)] // Constructor guarantees positive channels.
    pub const fn validate(self, bytes: &[u8]) -> Result<(), Error> {
        if !(bytes.len() as u64).is_multiple_of(self.channels as u64 * 2) {
            return Err(Error::Invalid("incomplete PCM frame"));
        }
        Ok(())
    }
}
/// Truthful upstream container information; no bytes are converted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// Google unary LINEAR16, MULAW and ALAW include a WAV header.
    Wav,
    /// Headerless PCM returned by streaming LINEAR16 or PCM encoding.
    RawPcm,
    /// Other upstream encoding, identified by its original enum value.
    Encoded(i32),
}
/// Complete unary synthesis result with its container, retaining original bytes.
#[derive(Debug)]
pub struct SynthesizedAudio {
    /// Original upstream response.
    pub response: crate::tts::SynthesizeSpeechResponse,
    /// Container implied by the requested upstream encoding.
    pub container: Container,
}
