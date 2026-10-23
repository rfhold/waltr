#[cfg(not(target_os = "android"))]
use crate::DeviceSelection;
use crate::{
    Config, Converter, Error, InputFormat,
    capture::{BACKEND, Backend, Block, OVERFLOW, POOL_EXHAUSTED, RAW_QUEUE_FULL, STOP},
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
    mpsc::{self, Receiver, SyncSender},
};

#[cfg(any(target_os = "android", test))]
mod android;
#[cfg(target_os = "android")]
pub use android::input_devices;

/// Explicitly query input device names. Names are not stable identifiers and can duplicate.
///
/// # Errors
/// Returns a backend error if enumeration or retrieving a device name fails.
#[cfg(not(target_os = "android"))]
pub fn input_devices() -> Result<Vec<String>, Error> {
    cpal::default_host()
        .input_devices()
        .map_err(backend_error)?
        .map(|d| d.name().map_err(backend_error))
        .collect()
}
fn backend_error(e: impl std::fmt::Display) -> Error {
    Error::Backend(e.to_string())
}

pub struct Native {
    stream: Option<cpal::Stream>,
    format: InputFormat,
    blocks: Receiver<Block>,
    pool: SyncSender<Block>,
    callback_closed: Receiver<()>,
    converter: Option<Converter>,
}
impl Backend for Native {
    fn take_converter(&mut self) -> Option<Converter> {
        self.converter.take()
    }
    fn start(&mut self) -> Result<(), Error> {
        self.stream
            .as_ref()
            .ok_or(Error::Closed)?
            .play()
            .map_err(backend_error)
    }
    fn format(&self) -> InputFormat {
        self.format
    }
    fn next(&mut self) -> Option<Block> {
        self.blocks.try_recv().ok()
    }
    fn recycle(&mut self, mut block: Block) {
        block.clear();
        let _ignored = self.pool.try_send(block);
    }
    fn stop(&mut self) {
        self.stream.take();
        // Closure destruction proves no admitted callback can still enqueue or report overflow.
        let _closed = self.callback_closed.recv();
    }
}

#[cfg(target_os = "android")]
fn select_input(
    config: &Config,
) -> Result<(cpal::Device, cpal::StreamConfig, cpal::SampleFormat), Error> {
    // Name/config discovery borrows ndk-context; only this default-device path is context-free.
    android::select_input(&config.device, || {
        cpal::default_host().default_input_device()
    })
}

#[cfg(not(target_os = "android"))]
fn select_input(
    config: &Config,
) -> Result<(cpal::Device, cpal::StreamConfig, cpal::SampleFormat), Error> {
    let host = cpal::default_host();
    let device = match &config.device {
        DeviceSelection::Default => host.default_input_device().ok_or(Error::DeviceNotFound)?,
        DeviceSelection::Name(name) => {
            let mut selected = None;
            for device in host.input_devices().map_err(backend_error)? {
                if device.name().map_err(backend_error)? == *name {
                    if selected.is_some() {
                        return Err(Error::AmbiguousDevice);
                    }
                    selected = Some(device);
                }
            }
            selected.ok_or(Error::DeviceNotFound)?
        }
    };
    let supported = device.default_input_config().map_err(backend_error)?;
    Ok((device, supported.config(), supported.sample_format()))
}

pub fn open(config: &Config, command: Arc<AtomicU8>) -> Result<Native, Error> {
    let (device, stream_config, sample_format) = select_input(config)?;
    let format = InputFormat {
        rate: stream_config.sample_rate.0,
        channels: stream_config.channels,
    };
    if format.channels == 0 || format.channels > 32 || !(8_000..=192_000).contains(&format.rate) {
        return Err(Error::UnsupportedFormat);
    }
    config.preflight(format)?;
    let slots = config.callback_queue_items(format)?;
    // Some CPAL backends start during build_input_stream, before play is called.
    let converter = Converter::new(config.clone(), format)?;
    let (pool_tx, pool_rx) = mpsc::sync_channel(slots + 1);
    let (raw_tx, raw_rx) = mpsc::sync_channel(slots);
    for _ in 0..=slots {
        let block = match sample_format {
            cpal::SampleFormat::F32 => {
                Block::F32(Vec::with_capacity(crate::converter::MAX_RAW_SAMPLES))
            }
            cpal::SampleFormat::I16 => {
                Block::I16(Vec::with_capacity(crate::converter::MAX_RAW_SAMPLES))
            }
            cpal::SampleFormat::U16 => {
                Block::U16(Vec::with_capacity(crate::converter::MAX_RAW_SAMPLES))
            }
            _ => return Err(Error::UnsupportedFormat),
        };
        pool_tx.try_send(block).map_err(|_| Error::Limit)?;
    }
    let (callback_lifetime, callback_closed) = mpsc::sync_channel(0);
    let queues = CallbackQueues {
        pool: pool_rx,
        raw: raw_tx,
        command,
        _callback_lifetime: callback_lifetime,
        owner: std::thread::current(),
    };
    let stream = match sample_format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &stream_config, queues),
        cpal::SampleFormat::I16 => build::<i16>(&device, &stream_config, queues),
        cpal::SampleFormat::U16 => build::<u16>(&device, &stream_config, queues),
        _ => return Err(Error::UnsupportedFormat),
    }?;
    Ok(Native {
        stream: Some(stream),
        format,
        blocks: raw_rx,
        pool: pool_tx,
        callback_closed,
        converter: Some(converter),
    })
}

struct CallbackQueues {
    pool: Receiver<Block>,
    raw: SyncSender<Block>,
    command: Arc<AtomicU8>,
    _callback_lifetime: SyncSender<()>,
    owner: std::thread::Thread,
}
trait CopySamples: cpal::SizedSample + Copy + 'static {
    fn copy_into(samples: &[Self], buffer: &mut Block) -> bool;
}
macro_rules! copy_samples {
    ($sample:ty, $variant:ident) => {
        impl CopySamples for $sample {
            fn copy_into(samples: &[Self], buffer: &mut Block) -> bool {
                if let Block::$variant(values) = buffer {
                    values.extend_from_slice(samples);
                    true
                } else {
                    false
                }
            }
        }
    };
}
copy_samples!(f32, F32);
copy_samples!(i16, I16);
copy_samples!(u16, U16);

fn build<T: CopySamples>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    queues: CallbackQueues,
) -> Result<cpal::Stream, Error> {
    let error_command = queues.command.clone();
    let error_owner = queues.owner.clone();
    let channels = usize::from(config.channels);
    device
        .build_input_stream(
            config,
            move |data: &[T], _| copy_callback(data, channels, &queues),
            move |_| {
                fail(&error_command, BACKEND);
                error_owner.unpark();
            },
            None,
        )
        .map_err(backend_error)
}

fn copy_callback<T: CopySamples>(data: &[T], channels: usize, queues: &CallbackQueues) {
    #[cfg(test)]
    copy_callback_checked(data, channels, queues, || {});
    #[cfg(not(test))]
    copy_callback_checked(data, channels, queues);
}

fn copy_callback_checked<T: CopySamples>(
    data: &[T],
    channels: usize,
    queues: &CallbackQueues,
    #[cfg(test)] after_check: impl FnOnce(),
) {
    if queues.command.load(Ordering::Acquire) != 0 {
        return;
    }
    #[cfg(test)]
    after_check();
    if !data.len().is_multiple_of(channels) {
        fail_callback(queues, BACKEND);
        return;
    }
    let chunk = crate::converter::MAX_RAW_SAMPLES / channels * channels;
    for samples in data.chunks(chunk) {
        let Ok(mut buffer) = queues.pool.try_recv() else {
            fail_callback(queues, POOL_EXHAUSTED);
            return;
        };
        if !T::copy_into(samples, &mut buffer) {
            fail_callback(queues, BACKEND);
            return;
        }
        if queues.raw.try_send(buffer).is_err() {
            fail_callback(queues, RAW_QUEUE_FULL);
            return;
        }
        queues.owner.unpark();
    }
}

fn fail_callback(queues: &CallbackQueues, code: u8) {
    fail(&queues.command, code);
    queues.owner.unpark();
}

fn fail(command: &AtomicU8, code: u8) {
    // Overflow proves an admitted callback lost audio, even if stop raced that callback.
    // Backend notifications after intentional destruction can be spurious: only accept them live.
    let _ignored = command.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        (current == 0
            || (current == STOP && matches!(code, OVERFLOW | POOL_EXHAUSTED | RAW_QUEUE_FULL)))
        .then_some(code)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_raw_queue_saturation_and_duplicate_errors_are_terminal() {
        let (pool_tx, pool_rx) = mpsc::sync_channel(2);
        let (raw_tx, raw_rx) = mpsc::sync_channel(1);
        pool_tx
            .try_send(Block::I16(Vec::with_capacity(
                crate::converter::MAX_RAW_SAMPLES,
            )))
            .ok()
            .unwrap();
        pool_tx
            .try_send(Block::I16(Vec::with_capacity(
                crate::converter::MAX_RAW_SAMPLES,
            )))
            .ok()
            .unwrap();
        let command = Arc::new(AtomicU8::new(0));
        let queues = CallbackQueues {
            pool: pool_rx,
            raw: raw_tx,
            command: command.clone(),
            _callback_lifetime: mpsc::sync_channel(0).0,
            owner: std::thread::current(),
        };
        copy_callback(&[123_i16; 4096], 2, &queues);
        assert_eq!(command.load(Ordering::Acquire), 0);
        copy_callback(&[123_i16; 2], 2, &queues);
        assert_eq!(command.load(Ordering::Acquire), RAW_QUEUE_FULL);
        fail(&command, BACKEND);
        fail(&command, BACKEND);
        assert_eq!(command.load(Ordering::Acquire), RAW_QUEUE_FULL);
        let Block::I16(block) = raw_rx.try_recv().unwrap() else {
            panic!("wrong format")
        };
        assert_eq!(block.len(), 4096);
        assert_eq!(block.capacity(), 4096);
        command.store(2, Ordering::Release);
        fail(&command, BACKEND);
        assert_eq!(command.load(Ordering::Acquire), 2);
    }

    struct PausedCallback {
        native: Native,
        stopping: SyncSender<()>,
        callback: Option<std::thread::JoinHandle<()>>,
    }
    impl Backend for PausedCallback {
        fn format(&self) -> InputFormat {
            self.native.format
        }
        fn next(&mut self) -> Option<Block> {
            None // Keep the scripted raw queue full until the admitted callback resumes.
        }
        fn recycle(&mut self, _: Block) {}
        fn stop(&mut self) {
            self.stopping.send(()).unwrap();
            self.native.stop();
            self.callback.take().unwrap().join().unwrap();
        }
    }

    fn assert_stop_racing_overflow(raw_full: bool) {
        use crate::{Capture, Terminal};
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = mpsc::sync_channel(0);
        let (stopping_tx, stopping_rx) = mpsc::sync_channel(1);
        let (capture, output) = Capture::spawn(Config::default(), move |_, command| {
            let (pool_tx, pool_rx) = mpsc::sync_channel(1);
            let (raw_tx, raw_rx) = mpsc::sync_channel(1);
            let (callback_lifetime, callback_closed) = mpsc::sync_channel(0);
            if raw_full {
                pool_tx
                    .try_send(Block::I16(Vec::with_capacity(
                        crate::converter::MAX_RAW_SAMPLES,
                    )))
                    .ok()
                    .unwrap();
                raw_tx.try_send(Block::I16(vec![0; 2])).ok().unwrap();
            }
            let callback = std::thread::spawn(move || {
                let queues = CallbackQueues {
                    pool: pool_rx,
                    raw: raw_tx,
                    command,
                    _callback_lifetime: callback_lifetime,
                    owner: std::thread::current(),
                };
                copy_callback_checked(&[123_i16; 2], 1, &queues, || {
                    entered_tx.send(()).unwrap();
                    resume_rx
                        .recv_timeout(std::time::Duration::from_secs(3))
                        .unwrap();
                });
            });
            Ok(PausedCallback {
                native: Native {
                    stream: None,
                    format: InputFormat {
                        rate: 48_000,
                        channels: 1,
                    },
                    blocks: raw_rx,
                    pool: pool_tx,
                    callback_closed,
                    converter: None,
                },
                stopping: stopping_tx,
                callback: Some(callback),
            })
        })
        .unwrap();
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        capture.control().stop();
        stopping_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        resume_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while output.terminal() == Terminal::Running {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let expected = if raw_full {
            Error::CallbackQueueFull
        } else {
            Error::CallbackPoolExhausted
        };
        assert_eq!(output.terminal(), Terminal::Failed(expected.clone()));
        assert_eq!(capture.shutdown().unwrap(), Terminal::Failed(expected));
    }

    #[test]
    fn admitted_overflow_supersedes_stop_after_callback_quiesces() {
        assert_stop_racing_overflow(false);
        assert_stop_racing_overflow(true);
    }

    #[test]
    fn overflow_preserves_cancel_and_first_failure_but_backend_ignores_stop() {
        for current in [2, OVERFLOW, BACKEND] {
            let command = AtomicU8::new(current);
            fail(&command, OVERFLOW);
            fail(&command, BACKEND);
            assert_eq!(command.load(Ordering::Acquire), current);
        }
        let command = AtomicU8::new(STOP);
        fail(&command, BACKEND);
        assert_eq!(command.load(Ordering::Acquire), STOP);
        fail(&command, OVERFLOW);
        assert_eq!(command.load(Ordering::Acquire), OVERFLOW);
        command.store(2, Ordering::Release);
        fail(&command, OVERFLOW);
        assert_eq!(command.load(Ordering::Acquire), 2);
    }

    fn synthetic_queues(slots: usize) -> (CallbackQueues, Receiver<Block>, SyncSender<Block>) {
        let (pool_tx, pool_rx) = mpsc::sync_channel(slots + 1);
        let (raw_tx, raw_rx) = mpsc::sync_channel(slots);
        for _ in 0..=slots {
            pool_tx
                .try_send(Block::I16(Vec::with_capacity(
                    crate::converter::MAX_RAW_SAMPLES,
                )))
                .ok()
                .unwrap();
        }
        (
            CallbackQueues {
                pool: pool_rx,
                raw: raw_tx,
                command: Arc::new(AtomicU8::new(0)),
                _callback_lifetime: mpsc::sync_channel(0).0,
                owner: std::thread::current(),
            },
            raw_rx,
            pool_tx,
        )
    }

    #[test]
    fn bounded_burst_includes_converter_owned_block_and_unscheduled_multiblock_callback() {
        let (queues, raw, pool) = synthetic_queues(4);
        copy_callback(&[1_i16; 128], 1, &queues);
        let mut converter_owned = raw.try_recv().unwrap();
        copy_callback(&[2_i16; 8192], 1, &queues);
        copy_callback(&[3_i16; 128], 1, &queues);
        copy_callback(&[4_i16; 128], 1, &queues);
        assert_eq!(queues.command.load(Ordering::Acquire), 0);
        // A held owner plus four queued blocks exhausts the five-block reserve, not the raw lane.
        copy_callback(&[5_i16; 128], 1, &queues);
        assert_eq!(queues.command.load(Ordering::Acquire), POOL_EXHAUSTED);
        assert_eq!(raw.try_iter().count(), 4);
        converter_owned.clear();
        pool.try_send(converter_owned).ok().unwrap();
    }

    #[test]
    fn finite_excessive_unscheduled_burst_reports_raw_queue_saturation() {
        let (queues, raw, _) = synthetic_queues(4);
        copy_callback(&[1_i16; 4096 * 5], 1, &queues);
        assert_eq!(queues.command.load(Ordering::Acquire), RAW_QUEUE_FULL);
        assert_eq!(raw.try_iter().count(), 4);
    }

    #[test]
    fn callback_wakeup_token_survives_delayed_owner_park_and_errors_wake() {
        for error in [false, true] {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let resume = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker_resume = resume.clone();
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            let worker = std::thread::spawn(move || {
                let (queues, raw, pool) = synthetic_queues(4);
                ready_tx.try_send((queues, pool)).ok().unwrap();
                // Blocking channel waits also use the thread's park token, so delay without one.
                while !worker_resume.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                std::thread::park();
                done_tx.try_send(raw.try_recv().is_ok()).unwrap();
            });
            let (queues, _pool) = ready_rx.recv().unwrap();
            if error {
                fail_callback(&queues, BACKEND);
            } else {
                copy_callback(&[1_i16; 128], 1, &queues);
            }
            resume.store(true, Ordering::Release);
            assert_eq!(
                done_rx
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap(),
                !error
            );
            worker.join().unwrap();
        }
    }

    struct Constructed(Native);
    impl Backend for Constructed {
        fn take_converter(&mut self) -> Option<Converter> {
            self.0.take_converter()
        }
        fn format(&self) -> InputFormat {
            self.0.format()
        }
        fn next(&mut self) -> Option<Block> {
            self.0.next()
        }
        fn recycle(&mut self, block: Block) {
            self.0.recycle(block);
        }
        fn stop(&mut self) {
            self.0.stop();
        }
    }

    #[test]
    fn construction_time_callbacks_use_prepared_converter_and_drain_on_stop() {
        use crate::{Capture, Terminal};
        let (capture, output) = Capture::spawn(Config::default(), |config, command| {
            let format = InputFormat {
                rate: 48_000,
                channels: 1,
            };
            let converter = Converter::new(config.clone(), format)?;
            let (mut queues, blocks, pool) = synthetic_queues(4);
            queues.command = command;
            // Model CPAL calling back synchronously during stream construction, before start().
            copy_callback(&[1_i16; 8192], 1, &queues);
            assert_eq!(queues.command.load(Ordering::Acquire), 0);
            drop(queues);
            Ok(Constructed(Native {
                stream: None,
                format,
                blocks,
                pool,
                callback_closed: mpsc::sync_channel(0).1,
                converter: Some(converter),
            }))
        })
        .unwrap();
        capture.control().stop();
        let mut bytes = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while output.terminal() == Terminal::Running {
            while let Some(frame) = output.try_frame() {
                bytes += frame.len();
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        while let Some(frame) = output.try_frame() {
            bytes += frame.len();
        }
        assert_eq!(bytes, 8192);
        assert_eq!(output.terminal(), Terminal::Completed);
        assert_eq!(capture.shutdown().unwrap(), Terminal::Completed);
    }

    #[test]
    fn native_burst_failure_interrupts_full_pcm_output_and_cancel_discards_it() {
        use crate::{Capture, Terminal};
        for cancel in [false, true] {
            let (callback_tx, callback_rx) = mpsc::sync_channel(1);
            let mut config = Config::default();
            config.limits.output_queue_items = 1;
            config.limits.output_queue_bytes = 2;
            config.limits.max_chunk_bytes = 2;
            let (capture, output) = Capture::spawn(config, move |config, command| {
                let format = InputFormat {
                    rate: 48_000,
                    channels: 1,
                };
                let converter = Converter::new(config.clone(), format)?;
                let (mut queues, blocks, pool) = synthetic_queues(4);
                queues.command = command;
                copy_callback(&[1_i16; 4096], 1, &queues);
                callback_tx.try_send(queues).ok().unwrap();
                Ok(Constructed(Native {
                    stream: None,
                    format,
                    blocks,
                    pool,
                    callback_closed: mpsc::sync_channel(0).1,
                    converter: Some(converter),
                }))
            })
            .unwrap();
            let queues = callback_rx.recv().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !capture
                .control()
                .output_backpressured
                .load(Ordering::Acquire)
            {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            // The first converted block exceeds the entire PCM lane; no further output drains.
            if cancel {
                capture.control().cancel();
                copy_callback(&[1_i16; 4096 * 6], 1, &queues);
            } else {
                copy_callback(&[1_i16; 4096 * 6], 1, &queues);
            }
            while output.terminal() == Terminal::Running {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert!(output.try_frame().is_none());
            let terminal = output.terminal();
            if cancel {
                assert_eq!(terminal, Terminal::Cancelled);
            } else {
                assert!(matches!(
                    terminal,
                    Terminal::Failed(Error::CallbackQueueFull | Error::CallbackPoolExhausted)
                ));
            }
            assert_eq!(capture.shutdown().unwrap(), terminal);
        }
    }

    #[test]
    fn callback_rejects_incomplete_native_frames() {
        let (_, pool_rx) = mpsc::sync_channel(1);
        let (raw_tx, _) = mpsc::sync_channel(1);
        let queues = CallbackQueues {
            pool: pool_rx,
            raw: raw_tx,
            command: Arc::new(AtomicU8::new(0)),
            _callback_lifetime: mpsc::sync_channel(0).0,
            owner: std::thread::current(),
        };
        copy_callback(&[0.0_f32], 2, &queues);
        assert_eq!(queues.command.load(Ordering::Acquire), BACKEND);
    }
}
