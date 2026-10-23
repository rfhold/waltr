use crate::{Config, Error};
/// Fixed resampler input block; shared with native admission accounting.
pub const INPUT_FRAMES: usize = 256;
/// Sinc kernel/history length; shared with native admission accounting.
pub const FILTER_LEN: usize = 128;
/// Maximum interleaved samples in one native pool block or converter push.
pub const MAX_RAW_SAMPLES: usize = 4096;
/// Hard zero-pad flush bound; shared with transient PCM capacity accounting.
pub const FLUSH_BLOCKS: usize = 4;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

/// Actual native input layout, separate from the output wire format.
#[derive(Clone, Copy, Debug)]
pub struct InputFormat {
    /// Native sample rate in Hz, 8000 through 192000.
    pub rate: u32,
    /// Interleaved channel count, one through 32.
    pub channels: u16,
}

/// Borrowed native interleaved samples; Debug redacts all sample values.
#[derive(Clone, Copy)]
pub enum Samples<'a> {
    /// Float samples (finite, clipped to unity before mixing).
    F32(&'a [f32]),
    /// Signed 16-bit native samples.
    I16(&'a [i16]),
    /// Unsigned 16-bit native samples centered on 32768.
    U16(&'a [u16]),
}

/// Stateful, single-stream converter. Never reset between callbacks. No sample-bearing Debug.
pub struct Converter {
    config: Config,
    input: InputFormat,
    sinc: SincFixedIn<f32>,
    staging: Vec<f32>,
    output: Vec<Vec<f32>>,
    skip: usize,
    received: u64,
    emitted: u64,
    closed: bool,
}

impl Converter {
    #[cfg(test)]
    pub(crate) fn output_capacity_frames(&self) -> usize {
        self.output[0].len()
    }
    /// Construct the stateful anti-aliasing resampler without opening a device.
    ///
    /// # Errors
    /// Returns invalid configuration, unsupported native layout, or resampler initialization errors.
    pub fn new(config: Config, input: InputFormat) -> Result<Self, Error> {
        config.validate()?;
        if !(8_000..=192_000).contains(&input.rate) || input.channels == 0 || input.channels > 32 {
            return Err(Error::UnsupportedFormat);
        }
        config.preflight(input)?;
        let sinc = SincFixedIn::<f32>::new(
            f64::from(config.target_rate) / f64::from(input.rate),
            1.0,
            SincInterpolationParameters {
                sinc_len: FILTER_LEN,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            },
            INPUT_FRAMES,
            1,
        )
        .map_err(|e| Error::Backend(e.to_string()))?;
        let skip = sinc.output_delay();
        let output = sinc.output_buffer_allocate(true);
        Ok(Self {
            config,
            input,
            sinc,
            staging: Vec::with_capacity(INPUT_FRAMES),
            output,
            skip,
            received: 0,
            emitted: 0,
            closed: false,
        })
    }

    /// At most 4096 interleaved samples per call; validates the entire block before mutation.
    ///
    /// # Errors
    /// Returns Closed, `MisalignedInput`, `NonFiniteSample`, Limit, or resampler errors.
    pub fn push(&mut self, samples: Samples<'_>) -> Result<Vec<u8>, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        let len = match &samples {
            Samples::F32(v) => v.len(),
            Samples::I16(v) => v.len(),
            Samples::U16(v) => v.len(),
        };
        if !len.is_multiple_of(usize::from(self.input.channels)) {
            return Err(Error::MisalignedInput);
        }
        if len > MAX_RAW_SAMPLES {
            return Err(Error::Limit);
        }
        if let Samples::F32(v) = &samples
            && v.iter().any(|s| !s.is_finite())
        {
            return Err(Error::NonFiniteSample);
        }
        let frames = len / usize::from(self.input.channels);
        let next = self
            .received
            .checked_add(frames as u64)
            .ok_or(Error::Limit)?;
        if next > u64::from(self.input.rate) * u64::from(self.config.limits.max_duration_seconds) {
            return Err(Error::Limit);
        }
        self.received = next;
        let mut bytes = Vec::new();
        for frame in 0..frames {
            let mut sum = 0.0;
            for channel in 0..usize::from(self.input.channels) {
                let i = frame * usize::from(self.input.channels) + channel;
                sum += match &samples {
                    Samples::F32(v) => v[i].clamp(-1.0, 1.0),
                    Samples::I16(v) => f32::from(v[i]) / 32768.0,
                    Samples::U16(v) => (f32::from(v[i]) - 32768.0) / 32768.0,
                };
            }
            self.staging.push(sum / f32::from(self.input.channels));
            if self.staging.len() == self.sinc.input_frames_next() {
                self.process(&mut bytes, u64::MAX)?;
            }
        }
        Ok(bytes)
    }

    fn process(&mut self, bytes: &mut Vec<u8>, ceiling: u64) -> Result<(), Error> {
        let (_, written) = self
            .sinc
            .process_into_buffer(&[&self.staging], &mut self.output, None)
            .map_err(|e| Error::Backend(e.to_string()))?;
        self.staging.clear();
        for sample in &self.output[0][..written] {
            if self.skip > 0 {
                self.skip -= 1;
                continue;
            }
            if self.emitted >= ceiling {
                break;
            }
            if !sample.is_finite() {
                return Err(Error::NonFiniteSample);
            }
            if self.emitted.checked_mul(2).ok_or(Error::Limit)? >= self.config.max_output_bytes()? {
                return Err(Error::Limit);
            }
            #[allow(clippy::cast_possible_truncation)]
            // Finite, rounded, and clamped to i16 bounds.
            let pcm = (sample.clamp(-1.0, 1.0) * 32768.0)
                .round()
                .clamp(-32768.0, 32767.0) as i16;
            bytes.extend_from_slice(&pcm.to_le_bytes());
            self.emitted += 1;
        }
        Ok(())
    }

    /// Zero-pad the sinc tail, remove filter delay, and end at `floor(input_frames * ratio)`.
    ///
    /// # Errors
    /// Returns Closed for an ended stream, or conversion/arithmetic errors.
    pub fn finish(&mut self) -> Result<Vec<u8>, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        self.closed = true;
        let target = self
            .received
            .checked_mul(u64::from(self.config.target_rate))
            .ok_or(Error::Limit)?
            / u64::from(self.input.rate);
        let mut bytes = Vec::new();
        let mut blocks = 0;
        while self.emitted < target {
            if blocks == FLUSH_BLOCKS {
                return Err(Error::Limit);
            }
            blocks += 1;
            self.staging.resize(self.sinc.input_frames_next(), 0.0);
            self.process(&mut bytes, target)?;
        }
        Ok(bytes)
    }

    /// Discard buffered audio. Subsequent pushes and finish return Closed.
    pub fn cancel(&mut self) {
        self.closed = true;
        self.staging.clear();
    }
}

impl std::fmt::Debug for Converter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Converter")
            .field("input", &self.input)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for Samples<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Samples { <redacted> }")
    }
}
