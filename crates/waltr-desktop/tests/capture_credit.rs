//! Exact byte-credit boundaries with synthetic PCM only.
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use waltr_audio_io::{Config, Terminal};
use waltr_desktop::capture::{CaptureWorker, Error, Factory, Source};

struct Burst {
    frames: VecDeque<Vec<u8>>,
    joined: Arc<AtomicBool>,
}
struct FactoryBurst(Vec<Vec<u8>>, Arc<AtomicBool>);
impl Factory for FactoryBurst {
    fn open(&self, _: Config) -> Result<Box<dyn Source>, Error> {
        Ok(Box::new(Burst {
            frames: self.0.clone().into(),
            joined: self.1.clone(),
        }))
    }
}
impl Source for Burst {
    fn frame(&mut self) -> Option<Vec<u8>> {
        self.frames.pop_front()
    }
    fn terminal(&self) -> Terminal {
        Terminal::Completed
    }
    fn stop(&self) {}
    fn shutdown(self: Box<Self>) {
        self.joined.store(true, Ordering::SeqCst);
    }
}
fn config() -> Config {
    let mut config = Config::default();
    config.limits.max_chunk_bytes = 100;
    config
}

#[tokio::test]
async fn many_small_frames_fill_exact_byte_credit_not_item_credit() {
    let joined = Arc::new(AtomicBool::new(false));
    let expected: Vec<Vec<u8>> = (0..=199u8).map(|value| vec![value; 4]).collect();
    let mut worker = CaptureWorker::start(
        Arc::new(FactoryBurst(expected.clone(), joined.clone())),
        config(),
    );
    // No receiver polls until the source exhausts its exact 800-byte queue credit.
    assert_eq!(worker.join().await, Ok(()));
    assert!(joined.load(Ordering::SeqCst));
    let mut actual = Vec::new();
    while let Some(frame) = worker.frames.recv().await {
        actual.push(frame);
    }
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn byte_exhaustion_and_malformed_frames_are_distinct_and_join_source() {
    for (frames, error) in [
        (vec![vec![0; 2]; 401], Error::Overflow),
        (vec![vec![0; 102]], Error::MalformedFrame),
        (vec![vec![0; 3]], Error::MalformedFrame),
    ] {
        let joined = Arc::new(AtomicBool::new(false));
        let mut worker =
            CaptureWorker::start(Arc::new(FactoryBurst(frames, joined.clone())), config());
        assert_eq!(worker.join().await, Err(error));
        assert!(joined.load(Ordering::SeqCst));
        assert_eq!(worker.check_failure(), Err(error));
    }
}

struct Paced {
    start: std::time::Instant,
    sent: u16,
    joined: Arc<AtomicBool>,
}
impl Source for Paced {
    fn frame(&mut self) -> Option<Vec<u8>> {
        if self.sent == 120
            || self.start.elapsed() < std::time::Duration::from_millis(u64::from(self.sent) * 2)
        {
            return None;
        }
        let frame = self.sent.to_le_bytes().repeat(48);
        self.sent += 1;
        Some(frame)
    }
    fn terminal(&self) -> Terminal {
        if self.sent == 120 {
            Terminal::Completed
        } else {
            Terminal::Running
        }
    }
    fn stop(&self) {}
    fn shutdown(self: Box<Self>) {
        self.joined.store(true, Ordering::SeqCst);
    }
}
struct PacedFactory(Arc<AtomicBool>);
impl Factory for PacedFactory {
    fn open(&self, _: Config) -> Result<Box<dyn Source>, Error> {
        Ok(Box::new(Paced {
            start: std::time::Instant::now(),
            sent: 0,
            joined: self.0.clone(),
        }))
    }
}
#[tokio::test]
async fn frequent_small_frames_continue_to_drain_during_250ms_consumer_delay() {
    let joined = Arc::new(AtomicBool::new(false));
    let config = waltr_desktop::capture::native_config(&waltr_desktop::config::Audio {
        device: waltr_desktop::config::DeviceSelector::Default,
        sample_rate_hz: 24_000,
        channels: 1,
    })
    .unwrap();
    let mut worker = CaptureWorker::start(Arc::new(PacedFactory(joined.clone())), config);
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert_eq!(worker.join().await, Ok(()));
    let mut actual = Vec::new();
    while let Some(frame) = worker.frames.recv().await {
        actual.extend(frame);
    }
    let expected: Vec<u8> = (0..120u16)
        .flat_map(|sample| sample.to_le_bytes().repeat(48))
        .collect();
    assert_eq!(actual, expected);
    assert!(joined.load(Ordering::SeqCst));
}

#[tokio::test]
async fn empty_frames_do_not_consume_credit() {
    let joined = Arc::new(AtomicBool::new(false));
    let mut frames = vec![vec![]; 1000];
    frames.push(vec![1, 2]);
    let mut worker = CaptureWorker::start(Arc::new(FactoryBurst(frames, joined.clone())), config());
    assert_eq!(worker.join().await, Ok(()));
    assert_eq!(worker.frames.recv().await, Some(vec![1, 2]));
    assert_eq!(worker.frames.recv().await, None);
    assert!(joined.load(Ordering::SeqCst));
}
