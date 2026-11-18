//! Fixed, local energy gate for press-to-talk; no samples leave capture before onset.
use std::time::Duration;

const WINDOWS_PER_SECOND: usize = 50;
const QUIET_WINDOWS: usize = WINDOWS_PER_SECOND;
const RMS_THRESHOLD: u64 = 512; // About -36 dBFS in signed PCM16.
const INITIAL_WAIT: Duration = Duration::from_secs(5);

pub(super) fn no_speech(detector: &Option<Detector>, elapsed: Duration) -> bool {
    elapsed >= INITIAL_WAIT && detector.as_ref().is_some_and(|detector| !detector.heard)
}

pub(super) struct Detector {
    window_samples: usize,
    pre_roll_bytes: usize,
    samples: usize,
    energy: u64,
    heard: bool,
    quiet: usize,
}

impl Detector {
    pub(super) fn new(sample_rate: u32) -> Self {
        Self {
            window_samples: sample_rate as usize / WINDOWS_PER_SECOND,
            pre_roll_bytes: (sample_rate as usize / 10) * 2,
            samples: 0,
            energy: 0,
            heard: false,
            quiet: 0,
        }
    }

    pub(super) fn pre_roll(&self) -> Option<usize> {
        (!self.heard).then_some(self.pre_roll_bytes)
    }

    /// Return true on sustained quiet. Window boundaries are independent of source frames.
    pub(super) fn observe(&mut self, pcm: &[u8]) -> bool {
        if self.quiet == QUIET_WINDOWS {
            return false; // The rest is native flush, not another utterance.
        }
        for pair in pcm.chunks_exact(2) {
            let sample = i16::from_le_bytes([pair[0], pair[1]]);
            self.energy += u64::from(sample.unsigned_abs()).pow(2);
            self.samples += 1;
            if self.samples == self.window_samples {
                if self.energy >= RMS_THRESHOLD * RMS_THRESHOLD * self.samples as u64 {
                    self.heard = true;
                    self.quiet = 0;
                } else if self.heard {
                    self.quiet += 1;
                }
                self.samples = 0;
                self.energy = 0;
                if self.quiet == QUIET_WINDOWS {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(sample: i16, count: usize) -> Vec<u8> {
        sample.to_le_bytes().repeat(count)
    }

    #[test]
    fn waits_through_noise_and_ends_after_consecutive_quiet_windows_only() {
        let mut detector = Detector::new(8_000);
        assert!(!detector.observe(&pcm(300, 8_000)));
        assert_eq!(detector.pre_roll(), Some(1_600));
        // Near-full-scale negative PCM, split across callback boundaries.
        assert!(!detector.observe(&pcm(i16::MIN, 1)));
        assert!(!detector.observe(&pcm(0, 159)));
        assert_eq!(detector.pre_roll(), None);
        assert!(!detector.observe(&pcm(0, 7_840)));
        assert!(!detector.observe(&pcm(800, 160)));
        assert!(!detector.observe(&pcm(200, 7_999)));
        assert!(detector.observe(&pcm(200, 1)));
        assert!(!detector.observe(&pcm(800, 160)));
    }

    #[test]
    fn initial_deadline_does_not_depend_on_frame_arrival_or_apply_once_speech_is_heard() {
        let mut detector = Some(Detector::new(48_000));
        assert!(!no_speech(&detector, Duration::from_millis(4_999)));
        assert!(no_speech(&detector, INITIAL_WAIT));
        detector.as_mut().unwrap().observe(&pcm(1_000, 960));
        assert!(!no_speech(&detector, INITIAL_WAIT));
        assert!(!no_speech(&None, INITIAL_WAIT));
    }
}
