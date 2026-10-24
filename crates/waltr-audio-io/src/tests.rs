#![allow(
    clippy::cast_precision_loss,
    clippy::many_single_char_names,
    clippy::field_reassign_with_default
)]

use super::*;
use crate::capture::{Backend, Block};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

fn converted(rate: u32, channels: u16, frames: usize, chunk: usize) -> Vec<u8> {
    let mut c = Converter::new(Config::default(), InputFormat { rate, channels }).unwrap();
    let samples: Vec<f32> = (0..frames)
        .flat_map(|i| {
            std::iter::repeat_n(
                (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5,
                usize::from(channels),
            )
        })
        .collect();
    let mut bytes = Vec::new();
    for part in samples.chunks(chunk * usize::from(channels)) {
        bytes.extend(c.push(Samples::F32(part)).unwrap());
    }
    bytes.extend(c.finish().unwrap());
    bytes
}

#[test]
fn exact_duration_and_callback_continuity() {
    for rate in [44_100, 48_000] {
        for frames in [0, 1, 2, 63, 257, 1001, rate as usize] {
            let whole = converted(rate, 1, frames, 1024);
            assert_eq!(whole.len(), frames * 24_000 / rate as usize * 2);
            assert_eq!(whole, converted(rate, 1, frames, 7));
            assert_eq!(whole, converted(rate, 2, frames, 173));
        }
    }
}

#[test]
fn formats_clipping_and_nonfinite() {
    let make = || {
        Converter::new(
            Config::default(),
            InputFormat {
                rate: 48_000,
                channels: 1,
            },
        )
        .unwrap()
    };
    let mut a = make();
    let mut b = make();
    let mut c = make();
    let mut x = a.push(Samples::F32(&[0.5; 1024])).unwrap();
    x.extend(a.finish().unwrap());
    let mut y = b.push(Samples::I16(&[16384; 1024])).unwrap();
    y.extend(b.finish().unwrap());
    let mut z = c.push(Samples::U16(&[49152; 1024])).unwrap();
    z.extend(c.finish().unwrap());
    assert_eq!(x, y);
    assert_eq!(x, z);
    let mut a = make();
    assert_eq!(
        a.push(Samples::F32(&[f32::NAN])),
        Err(Error::NonFiniteSample)
    );
    assert_eq!(
        a.push(Samples::F32(&[f32::INFINITY])),
        Err(Error::NonFiniteSample)
    );
    assert!(a.push(Samples::F32(&[10.0; 1024])).is_ok());
    a.cancel();
    assert_eq!(a.finish(), Err(Error::Closed));
}

#[test]
fn hour_duration_ceiling_is_explicit_and_bounded() {
    let mut config = Config::default();
    assert_eq!(config.limits.max_duration_seconds, 300);
    for seconds in [3600, 3660] {
        config.limits.max_duration_seconds = seconds;
        assert_eq!(config.validate(), Ok(()));
        assert_eq!(
            config.max_output_bytes().unwrap(),
            u64::from(seconds) * 24_000 * 2
        );
    }
    for seconds in [0, 3661, u32::MAX] {
        config.limits.max_duration_seconds = seconds;
        assert_eq!(config.validate(), Err(Error::InvalidConfig));
    }
}

#[test]
fn limits_and_alignment() {
    let mut config = Config::default();
    config.target_channels = 2;
    assert_eq!(config.validate(), Err(Error::InvalidConfig));
    config = Config::default();
    config.limits.max_duration_seconds = 1;
    let mut c = Converter::new(
        config,
        InputFormat {
            rate: 8_000,
            channels: 2,
        },
    )
    .unwrap();
    assert_eq!(c.push(Samples::I16(&[1])), Err(Error::MisalignedInput));
    assert_eq!(c.push(Samples::I16(&[1; 4098])), Err(Error::Limit));
    for _ in 0..4 {
        c.push(Samples::I16(&[1; 4000])).unwrap();
    }
    assert_eq!(c.push(Samples::I16(&[1, 1])), Err(Error::Limit));
}

struct Script {
    blocks: VecDeque<Block>,
    stopped: Arc<AtomicBool>,
    command: Arc<AtomicU8>,
    failure: u8,
}
impl Backend for Script {
    fn format(&self) -> InputFormat {
        InputFormat {
            rate: 48_000,
            channels: 1,
        }
    }
    fn next(&mut self) -> Option<Block> {
        if self.failure != 0 {
            let _ =
                self.command
                    .compare_exchange(0, self.failure, Ordering::AcqRel, Ordering::Acquire);
        }
        self.blocks.pop_front()
    }
    fn recycle(&mut self, _: Block) {}
    fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}
impl Drop for Script {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}
fn scripted(failure: u8, blocks: usize) -> (Capture, CaptureOutput, Arc<AtomicBool>) {
    let stopped = Arc::new(AtomicBool::new(false));
    let seen = stopped.clone();
    let mut config = Config::default();
    config.limits.output_queue_bytes = 32;
    config.limits.output_queue_items = 1;
    config.limits.max_chunk_bytes = 16;
    let (capture, output) = Capture::spawn(config, move |_, command| {
        Ok(Script {
            blocks: (0..blocks)
                .map(|index| match index % 3 {
                    0 => Block::F32(vec![0.1; 1024]),
                    1 => Block::I16(vec![3277; 1024]),
                    _ => Block::U16(vec![36045; 1024]),
                })
                .collect(),
            stopped: seen,
            command,
            failure,
        })
    })
    .unwrap();
    (capture, output, stopped)
}
fn until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !predicate() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn normal_stop_drains_aligned_frames_and_joins() {
    let (capture, output, stopped) = scripted(0, 4);
    let control = capture.control();
    control.stop();
    let mut bytes = 0;
    until(|| {
        while let Some(frame) = output.try_frame() {
            assert_eq!(frame.len() % 2, 0);
            assert!(frame.len() <= 16);
            bytes += frame.len();
        }
        output.terminal() != Terminal::Running
    });
    while let Some(frame) = output.try_frame() {
        bytes += frame.len();
    }
    assert_eq!(bytes, 4096);
    assert!(stopped.load(Ordering::Acquire));
    assert_eq!(capture.shutdown().unwrap(), Terminal::Completed);
}
#[test]
fn full_queue_stop_then_cancel_closes_promptly() {
    let (capture, _output, stopped) = scripted(0, 100);
    let control = capture.control();
    std::thread::sleep(Duration::from_millis(20));
    control.stop();
    until(|| stopped.load(Ordering::Acquire));
    assert_eq!(capture.shutdown().unwrap(), Terminal::Cancelled);
}
#[test]
fn idle_stop_and_cancel_wake_parked_owner() {
    for cancel in [false, true] {
        let (capture, output, stopped) = scripted(0, 0);
        let control = capture.control();
        // No callbacks or output reader activity can wake this empty backend.
        if cancel {
            control.cancel();
        } else {
            control.stop();
        }
        until(|| output.terminal() != Terminal::Running);
        assert_eq!(
            output.terminal(),
            if cancel {
                Terminal::Cancelled
            } else {
                Terminal::Completed
            }
        );
        assert!(stopped.load(Ordering::Acquire));
        capture.shutdown().unwrap();
    }
}

#[test]
fn full_output_callback_failures_wake_and_remain_terminal() {
    for code in [
        crate::capture::POOL_EXHAUSTED,
        crate::capture::RAW_QUEUE_FULL,
    ] {
        let (capture, output, stopped) = scripted(0, 100);
        let control = capture.control();
        until(|| control.output_backpressured.load(Ordering::Acquire));
        control.command.store(code, Ordering::Release);
        // Callback and controller share the same retained-token wake primitive.
        control.stop();
        until(|| output.terminal() != Terminal::Running);
        let expected = if code == crate::capture::POOL_EXHAUSTED {
            Error::CallbackPoolExhausted
        } else {
            Error::CallbackQueueFull
        };
        assert_eq!(output.terminal(), Terminal::Failed(expected.clone()));
        assert!(output.try_frame().is_none());
        assert!(stopped.load(Ordering::Acquire));
        assert_eq!(capture.shutdown().unwrap(), Terminal::Failed(expected));
    }
}

#[test]
fn output_drop_and_owner_drop_join() {
    let (capture, output, stopped) = scripted(0, 100);
    drop(output);
    drop(capture);
    assert!(stopped.load(Ordering::Acquire));
}
#[test]
fn startup_failure_and_panic_join_without_opening_hardware() {
    let failure = Capture::spawn::<Script, _>(Config::default(), |_, _| Err(Error::DeviceNotFound));
    assert!(matches!(failure, Err(Error::DeviceNotFound)));
    let panic =
        Capture::spawn::<Script, _>(Config::default(), |_, _| panic!("scripted startup panic"));
    assert!(matches!(panic, Err(Error::WorkerPanicked)));
    let mut config = Config::default();
    config.limits.output_queue_bytes = 0;
    let invalid = Capture::spawn::<Script, _>(config, |_, _| {
        panic!("validation must precede backend creation")
    });
    assert!(matches!(invalid, Err(Error::InvalidConfig)));
}

#[test]
fn cancel_discards_queued_frames_and_joins() {
    let (capture, output, stopped) = scripted(0, 100);
    std::thread::sleep(Duration::from_millis(20));
    capture.control().cancel();
    assert!(output.try_frame().is_none());
    assert_eq!(capture.shutdown().unwrap(), Terminal::Cancelled);
    assert!(stopped.load(Ordering::Acquire));
}

#[test]
fn errors_use_independent_terminal_lane() {
    for code in [crate::capture::OVERFLOW, crate::capture::BACKEND] {
        let (capture, output, stopped) = scripted(code, 100);
        until(|| output.terminal() != Terminal::Running);
        assert!(matches!(output.terminal(), Terminal::Failed(_)));
        assert!(output.try_frame().is_none());
        capture.shutdown().unwrap();
        assert!(stopped.load(Ordering::Acquire));
    }
}
